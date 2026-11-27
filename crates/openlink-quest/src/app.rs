//! The app: OpenXR session and frame loop on the main thread, with a tracking/network thread
//! and a decoder thread beside it.
//!
//! Each displayed frame is the newest decoded picture, submitted as a projection layer at the
//! eye poses SteamVR rendered it with. The Quest compositor reprojects it to where the head is
//! now, so head motion stays smooth even when the stream lags.

use crate::decoder::{self, Output};
use crate::egl;
use crate::render::Renderer;
use crate::tracking::{Actions, XrTracking};
use android_activity::{AndroidApp, MainEvent, PollEvent};
use glam::{Quat, Vec3};
use openlink_client::{Client, ClientState, FrameReporter};
use openlink_protocol::{Fov, HeadsetInfo, Pose};
use openxr as xr;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

const TRACKING_HZ: f64 = 500.0;
/// `adb shell setprop debug.openlink.server 192.168.1.10` skips LAN discovery.
const SERVER_PROPERTY: &str = "debug.openlink.server";
const GL_SRGB8_ALPHA8: u32 = 0x8C43;

struct Eye {
    swapchain: xr::Swapchain<xr::OpenGlEs>,
    images: Vec<u32>,
    width: i32,
    height: i32,
}

pub fn run(app: &AndroidApp) -> Result<(), String> {
    fn e(what: &'static str) -> impl Fn(xr::sys::Result) -> String {
        move |err| format!("{what}: {err}")
    }

    // SAFETY: the JavaVM and Activity pointers come from android-activity and outlive the app.
    let platform = unsafe { xr::AndroidPlatformInfo::new(app.vm_as_ptr(), app.activity_as_ptr()) };
    // SAFETY: loading the Khronos loader bundled in the APK.
    let entry = unsafe { xr::Entry::load(&platform) }.map_err(|err| format!("OpenXR loader: {err}"))?;
    let available = entry.enumerate_extensions().map_err(e("extensions"))?;
    let mut exts = xr::ExtensionSet::default();
    exts.khr_android_create_instance = true;
    exts.khr_opengl_es_enable = true;
    exts.khr_convert_timespec_time = available.khr_convert_timespec_time;
    exts.fb_display_refresh_rate = available.fb_display_refresh_rate;
    exts.ext_performance_settings = available.ext_performance_settings;
    exts.fb_composition_layer_settings = available.fb_composition_layer_settings;
    exts.ext_interaction_profile_battery_state_display = available.ext_interaction_profile_battery_state_display;
    // Interaction profiles that tell Touch Plus and Touch Pro apart from Quest 2 Touch.
    exts.meta_touch_controller_plus = available.meta_touch_controller_plus;
    exts.fb_touch_controller_pro = available.fb_touch_controller_pro;
    log::info!(
        "controller battery: {}",
        if available.ext_interaction_profile_battery_state_display { "reported" } else { "not available from this runtime" }
    );
    let instance = entry
        .create_instance(
            &xr::ApplicationInfo {
                application_name: "OpenLink",
                application_version: 1,
                engine_name: "OpenLink",
                engine_version: 1,
                api_version: xr::Version::new(1, 0, 0),
            },
            &exts,
            &[],
            &platform,
        )
        .map_err(e("xrCreateInstance"))?;
    let system = instance.system(xr::FormFactor::HEAD_MOUNTED_DISPLAY).map_err(e("xrGetSystem"))?;
    let props = instance.system_properties(system).map_err(e("system properties"))?;
    log::info!("OpenXR runtime on {}", props.system_name);
    let _ = instance.graphics_requirements::<xr::OpenGlEs>(system).map_err(e("GLES requirements"))?;

    crate::audio::request_microphone_permission(app);

    let egl = egl::Context::new()?;
    let mut renderer = Renderer::new(egl.gl())?;
    // SAFETY: a valid, current GLES context on this thread.
    let (session, mut waiter, mut stream) = unsafe {
        instance.create_session::<xr::OpenGlEs>(
            system,
            &xr::opengles::SessionCreateInfo::Android {
                display: egl.display.as_ptr(),
                config: egl.config.as_ptr(),
                context: egl.context.as_ptr(),
            },
        )
    }
    .map_err(e("xrCreateSession"))?;

    let actions = Actions::new(&instance).map_err(e("actions"))?;
    session.attach_action_sets(&[&actions.set]).map_err(e("attach actions"))?;
    let mut actions = Some(actions);
    let stage = session
        .create_reference_space(xr::ReferenceSpaceType::STAGE, xr::Posef::IDENTITY)
        .map_err(e("stage space"))?;
    let views = instance
        .enumerate_view_configuration_views(system, xr::ViewConfigurationType::PRIMARY_STEREO)
        .map_err(e("view configuration"))?;
    // Head pose, for checking frames' render poses against where the head really is.
    let head = session
        .create_reference_space(xr::ReferenceSpaceType::VIEW, xr::Posef::IDENTITY)
        .map_err(e("view space"))?;
    let recommended = (views[0].recommended_image_rect_width, views[0].recommended_image_rect_height);
    // Sized to the stream once the server says what it sends, so pictures are not resampled twice.
    let mut eyes: Vec<Eye> = Vec::new();

    let mut refresh_rates = vec![72.0];
    if exts.fb_display_refresh_rate {
        if let Ok(r) = session.enumerate_display_refresh_rates() {
            refresh_rates = r;
        }
        // Quest 3 on Horizon OS v2.7 runs at up to 207 Hz (240 Hz in developer mode, with display scaling), but its
        // runtime never lists those rates: not at OpenXR API version 1.0.0, 1.0.63 or 1.1.63,
        // although Meta documents 1.0.63+ as enough. It does accept them when requested
        // (against the spec, which says an unlisted rate must be refused). So ask for each and
        // keep the ones it takes, then go back to where it was; a runtime that follows the
        // spec refuses them all and nothing changes.
        let before = current_rate(&session, true);
        let mut probed = Vec::new();
        for &hz in openlink_protocol::high_refresh_rates(&props.system_name) {
            if !refresh_rates.contains(&hz) && session.request_display_refresh_rate(hz).is_ok() {
                probed.push(hz);
                refresh_rates.push(hz);
            }
        }
        if !probed.is_empty() {
            let _ = session.request_display_refresh_rate(before);
        }
        log::info!("display refresh rates {refresh_rates:?} (accepted beyond the runtime's list: {probed:?})");
    }

    let output = Arc::new(Mutex::new(Output::default()));
    let mut session_running = false;
    let client: Arc<OnceLock<Connected>> = Arc::new(OnceLock::new());
    let mut last_present_ns = None;
    let mut applied_rate: Option<f32> = None;
    // When the rate was last asked for, to ask again while the display has not followed.
    let mut rate_requested_at = std::time::Instant::now();
    let stop = Arc::new(AtomicBool::new(false));
    let mut ipd = 0.063f32;
    let mut fov = [xr::Fovf::default(); 2];
    let mut event_storage = xr::EventDataBuffer::new();
    let mut quit = false;
    let mut stats = Stats::default();
    let mut period_window: Option<(i64, std::time::Instant)> = None;
    // Debug: `adb shell setprop debug.openlink.swap_eyes 1` shows each eye the other's picture.
    let swap_eyes = system_property("debug.openlink.swap_eyes").as_deref() == Some("1");
    let (mut frozen, mut freeze_checked) = (false, std::time::Instant::now());
    let mut layer_flags = layer_filter();
    log::info!("compositor layer filter: {layer_flags:?} (available: {})", exts.fb_composition_layer_settings);
    if swap_eyes {
        log::warn!("debug: eyes swapped");
    }

    while !quit {
        // Android lifecycle. Block only while the session is idle.
        let timeout = if session_running { Some(Duration::ZERO) } else { Some(Duration::from_millis(100)) };
        app.poll_events(timeout, |event| {
            if let PollEvent::Main(MainEvent::Destroy) = event {
                quit = true;
            }
        });

        // OpenXR lifecycle.
        while let Some(event) = instance.poll_event(&mut event_storage).map_err(e("poll event"))? {
            use xr::Event::*;
            match event {
                SessionStateChanged(s) => {
                    log::info!("session state {:?}", s.state());
                    match s.state() {
                        xr::SessionState::READY => {
                            session.begin(xr::ViewConfigurationType::PRIMARY_STEREO).map_err(e("xrBeginSession"))?;
                            session_running = true;
                            request_sustained_performance(&instance, &session);
                        }
                        xr::SessionState::STOPPING => {
                            session.end().map_err(e("xrEndSession"))?;
                            session_running = false;
                        }
                        xr::SessionState::EXITING | xr::SessionState::LOSS_PENDING => quit = true,
                        _ => {}
                    }
                }
                DisplayRefreshRateChangedFB(r) => {
                    log::info!("display switched from {} to {} Hz", r.from_display_refresh_rate(), r.to_display_refresh_rate());
                }
                InstanceLossPending(_) => quit = true,
                _ => {}
            }
        }
        if !session_running {
            continue;
        }

        let state = waiter.wait().map_err(e("xrWaitFrame"))?;
        // Measure the display's real period (display times span whole periods, even across
        // skipped frames) and pace SteamVR to it.
        let t = state.predicted_display_time.as_nanos();
        match period_window {
            None => period_window = Some((t, std::time::Instant::now())),
            Some((t0, since)) if since.elapsed() >= Duration::from_secs(3) => {
                let nominal = state.predicted_display_period.as_nanos().max(1);
                let periods = ((t - t0) as f64 / nominal as f64).round();
                if periods >= 1.0 {
                    if let Some(c) = client.get() {
                        c.reporter.display_period(((t - t0) as f64 / periods).round() as i64);
                    }
                }
                period_window = Some((t, std::time::Instant::now()));
            }
            Some(_) => {}
        }
        stream.begin().map_err(e("xrBeginFrame"))?;

        // Once the views are known, the headset can describe itself and connect.
        if actions.is_some() {
            if let Ok((flags, v)) =
                session.locate_views(xr::ViewConfigurationType::PRIMARY_STEREO, state.predicted_display_time, &stage)
            {
                if flags.contains(xr::ViewStateFlags::POSITION_VALID) && v.len() == 2 {
                    let (a, b) = (v[0].pose.position, v[1].pose.position);
                    ipd = Vec3::new(a.x - b.x, a.y - b.y, a.z - b.z).length().clamp(0.05, 0.08);
                    fov = [v[0].fov, v[1].fov];
                    let info = HeadsetInfo {
                        model: props.system_name.clone(),
                        serial: format!("OpenLink-{:x}", props.system_id.into_raw()),
                        eye_width: recommended.0,
                        eye_height: recommended.1,
                        refresh_rates: refresh_rates.clone(),
                        current_refresh_rate: current_rate(&session, exts.fb_display_refresh_rate),
                        ipd_m: ipd,
                        fov: fov.map(|f| Fov {
                            angle_left: f.angle_left,
                            angle_right: f.angle_right,
                            angle_up: f.angle_up,
                            angle_down: f.angle_down,
                        }),
                        decoders: decoder::supported(),
                    };
                    log::info!("headset: {info:?}");
                    let actions = actions.take().unwrap();
                    let tracking = XrTracking {
                        session: session.clone().into_any_graphics(),
                        stage: session
                            .create_reference_space(xr::ReferenceSpaceType::STAGE, xr::Posef::IDENTITY)
                            .map_err(e("stage space"))?,
                        view: session
                            .create_reference_space(xr::ReferenceSpaceType::VIEW, xr::Posef::IDENTITY)
                            .map_err(e("view space"))?,
                        grips: [actions.grip_space(&session, 0).map_err(e("grip space"))?, actions.grip_space(&session, 1).map_err(e("grip space"))?],
                        actions,
                        instance: instance.clone(),
                        info,
                        last_hmd: None,
                        lost_hmd: (0, None),
                        app: app.clone(),
                    };
                    start_client(app.clone(), tracking, output.clone(), client.clone(), stop.clone());
                }
            }
        }

        // Follow the refresh rate the server streams at.
        let welcome = client.get().and_then(|c| c.state.lock().unwrap().welcome.clone());
        if let Some(cfg) = &welcome {
            let size = (cfg.eye_width as i32, cfg.eye_height as i32);
            if eyes.first().is_none_or(|e| (e.width, e.height) != size) {
                eyes = [0, 1].map(|_| create_eye(&session, size)).into_iter().collect::<Result<_, _>>()?;
                log::info!("eye swapchains {}x{}", size.0, size.1);
            }
        }
        if let Some(cfg) = welcome {
            let retry = applied_rate == Some(cfg.refresh_rate)
                && rate_requested_at.elapsed() >= Duration::from_secs(1)
                && (current_rate(&session, exts.fb_display_refresh_rate) - cfg.refresh_rate).abs() >= 0.5;
            if retry {
                log::warn!(
                    "display still at {} Hz, not the requested {} Hz; asking again",
                    current_rate(&session, true),
                    cfg.refresh_rate
                );
            }
            if exts.fb_display_refresh_rate && (applied_rate != Some(cfg.refresh_rate) || retry) {
                rate_requested_at = std::time::Instant::now();
                match session.request_display_refresh_rate(cfg.refresh_rate) {
                    Ok(()) => {
                        log::info!("display refresh rate {} Hz", cfg.refresh_rate);
                        // Measure the new rate's period from scratch, not across the switch.
                        period_window = None;
                    }
                    Err(err) => log::warn!("cannot switch to {} Hz: {err}", cfg.refresh_rate),
                }
                applied_rate = Some(cfg.refresh_rate);
            }
        }

        // Debug: `adb shell setprop debug.openlink.freeze 1` holds the current picture, still
        // submitted at its render pose. With working reprojection the frozen world stays put
        // as the head turns.
        if freeze_checked.elapsed() > Duration::from_millis(500) {
            freeze_checked = std::time::Instant::now();
            let now = system_property("debug.openlink.freeze").as_deref() == Some("1");
            if now != frozen {
                log::warn!("debug: picture {}", if now { "frozen" } else { "live" });
            }
            frozen = now;
            let filter = layer_filter();
            if filter != layer_flags {
                log::info!("compositor layer filter: {filter:?}");
                layer_flags = filter;
            }
        }
        let latest = if frozen { None } else { output.lock().unwrap().next() };
        if let Some((_, p)) = &latest {
            stats.slack_ms.push((openlink_protocol::time::monotonic_ns() - p.decoded_ns) as f32 * 1e-6);
        }
        stats.displayed += 1;
        stats.new_pictures += latest.is_some() as u32;
        let shown = renderer.update(&egl, latest);
        if stats.since.elapsed() >= Duration::from_secs(5) {
            let mut out = output.lock().unwrap();
            if out.trimmed + out.unmatched > 0 || out.released != out.collected {
                log::info!(
                    "pictures dropped: {} from the backlog, {} without frame info, {} replaced before collection ({} decoded, {} collected)",
                    out.trimmed,
                    out.unmatched,
                    out.released.saturating_sub(out.collected),
                    out.released,
                    out.collected
                );
            }
            (out.trimmed, out.unmatched, out.released, out.collected) = (0, 0, 0, 0);
        }
        stats.period_ms = state.predicted_display_period.as_nanos() as f32 * 1e-6;
        if let Some(shift_ns) = stats.report(client.get().map(|c| &c.state)) {
            if let Some(c) = client.get() {
                c.reporter.vsync_shift(shift_ns);
            }
        }
        let Some(picture) = shown.filter(|_| state.should_render && !eyes.is_empty()) else {
            stream.end(state.predicted_display_time, xr::EnvironmentBlendMode::OPAQUE, &[]).map_err(e("xrEndFrame"))?;
            continue;
        };

        for (i, eye) in eyes.iter_mut().enumerate() {
            let index = eye.swapchain.acquire_image().map_err(e("acquire swapchain image"))?;
            eye.swapchain.wait_image(xr::Duration::INFINITE).map_err(e("wait swapchain image"))?;
            let source = if swap_eyes { 1 - i } else { i };
            renderer.draw(source, eye.images[index as usize], eye.width, eye.height);
            eye.swapchain.release_image().map_err(e("release swapchain image"))?;
        }
        let info = picture.info;
        if last_present_ns != Some(info.present_ns) {
            last_present_ns = Some(info.present_ns);
            if let Ok((loc, vel)) = head.relate(&stage, state.predicted_display_time) {
                stats.pose_error(&info.render_pose, &loc.pose, &vel.angular_velocity);
            }
            if let Some(c) = client.get() {
                c.reporter.displayed(
                    info.present_ns,
                    picture.received_ns,
                    picture.queued_ns,
                    picture.decoded_ns,
                    monotonic_ns(&instance, state.predicted_display_time),
                );
            }
        }
        let poses = eye_poses(&info.render_pose, ipd);
        let views: Vec<_> = (0..2)
            .map(|i| {
                xr::CompositionLayerProjectionView::new().pose(poses[i]).fov(fov[i]).sub_image(
                    xr::SwapchainSubImage::new()
                        .swapchain(&eyes[i].swapchain)
                        .image_rect(xr::Rect2Di {
                            offset: xr::Offset2Di { x: 0, y: 0 },
                            extent: xr::Extent2Di { width: eyes[i].width, height: eyes[i].height },
                        }),
                )
            })
            .collect();
        let layer = xr::CompositionLayerProjection::new().space(&stage).views(&views);
        // How the runtime's compositor filters the layer onto the display (see `layer_filter`).
        let settings = xr::sys::CompositionLayerSettingsFB {
            ty: xr::sys::CompositionLayerSettingsFB::TYPE,
            next: std::ptr::null(),
            layer_flags,
        };
        let layer = if exts.fb_composition_layer_settings && layer_flags != xr::sys::CompositionLayerSettingsFlagsFB::EMPTY {
            let mut raw = layer.into_raw();
            raw.next = (&raw const settings).cast();
            // SAFETY: the same layer with `settings` chained; it and the views it points to
            // outlive the xrEndFrame call below.
            unsafe { xr::CompositionLayerProjection::from_raw(raw) }
        } else {
            layer
        };
        stream
            .end(state.predicted_display_time, xr::EnvironmentBlendMode::OPAQUE, &[&layer])
            .map_err(e("xrEndFrame"))?;
    }
    stop.store(true, Ordering::Release);
    Ok(())
}

struct Connected {
    state: Arc<Mutex<ClientState>>,
    reporter: FrameReporter,
}

fn create_eye(session: &xr::Session<xr::OpenGlEs>, (width, height): (i32, i32)) -> Result<Eye, String> {
    let swapchain = session
        .create_swapchain(&xr::SwapchainCreateInfo {
            create_flags: xr::SwapchainCreateFlags::EMPTY,
            usage_flags: xr::SwapchainUsageFlags::COLOR_ATTACHMENT | xr::SwapchainUsageFlags::SAMPLED,
            format: GL_SRGB8_ALPHA8,
            sample_count: 1,
            width: width as u32,
            height: height as u32,
            face_count: 1,
            array_size: 1,
            mip_count: 1,
        })
        .map_err(|err| format!("swapchain: {err}"))?;
    let images = swapchain.enumerate_images().map_err(|err| format!("swapchain images: {err}"))?;
    Ok(Eye { swapchain, images, width, height })
}

/// `XrTime` -> `CLOCK_MONOTONIC` nanoseconds (the clock the protocol uses).
fn monotonic_ns(instance: &xr::Instance, t: xr::Time) -> i64 {
    let Some(ext) = instance.exts().khr_convert_timespec_time else { return t.as_nanos() };
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: valid instance and out pointer.
    let r = unsafe { (ext.convert_time_to_timespec_time)(instance.as_raw(), t, &mut ts) };
    if r.into_raw() < 0 {
        return t.as_nanos();
    }
    ts.tv_sec * 1_000_000_000 + ts.tv_nsec
}

/// Asks for sustained high CPU and GPU clocks. The app's own load is light, so by default the
/// runtime lowered them (CPU often at 1.4 GHz), and the decoder and the threads feeding it slowed
/// and grew jittery: decode time rose from 6 to 10 ms and pictures started missing frames.
fn request_sustained_performance(instance: &xr::Instance, session: &xr::Session<xr::OpenGlEs>) {
    let Some(ext) = instance.exts().ext_performance_settings else {
        log::warn!("XR_EXT_performance_settings unavailable; clocks stay dynamic");
        return;
    };
    for domain in [xr::PerfSettingsDomainEXT::CPU, xr::PerfSettingsDomainEXT::GPU] {
        // SAFETY: valid session and enum values.
        let r = unsafe {
            (ext.perf_settings_set_performance_level)(session.as_raw(), domain, xr::PerfSettingsLevelEXT::SUSTAINED_HIGH)
        };
        if r.into_raw() < 0 {
            log::warn!("cannot raise {domain:?} performance level: {r:?}");
        }
    }
    log::info!("requested sustained high CPU and GPU performance");
}

fn current_rate(session: &xr::Session<xr::OpenGlEs>, ext: bool) -> f32 {
    if ext {
        session.get_display_refresh_rate().unwrap_or(72.0)
    } else {
        72.0
    }
}

/// SteamVR renders each eye at the head pose offset by half the IPD along the head's X axis.
fn eye_poses(head: &Pose, ipd: f32) -> [xr::Posef; 2] {
    let q = Quat::from_array(head.orientation);
    let p = Vec3::from_array(head.position);
    [-0.5f32, 0.5].map(|side| {
        let e = p + q * Vec3::new(side * ipd, 0.0, 0.0);
        xr::Posef {
            orientation: xr::Quaternionf { x: q.x, y: q.y, z: q.z, w: q.w },
            position: xr::Vector3f { x: e.x, y: e.y, z: e.z },
        }
    })
}

/// Connects (discovering the server unless the property names one) and streams tracking on
/// its own thread; decoded video flows into `output`, audio both ways through AAudio.
/// `connected` is set once connected.
fn start_client(
    app: AndroidApp,
    mut tracking: XrTracking,
    output: Arc<Mutex<Output>>,
    connected: Arc<OnceLock<Connected>>,
    stop: Arc<AtomicBool>,
) {
    std::thread::Builder::new()
        .name("tracking".into())
        .spawn(move || {
            let server = loop {
                if stop.load(Ordering::Acquire) {
                    return;
                }
                if let Some(addr) = server_from_property() {
                    break addr;
                }
                match openlink_client::discover(&tracking.info, Duration::from_secs(2)) {
                    Ok(Some(addr)) => break addr,
                    Ok(None) => log::info!("no OpenLink server found on the LAN yet"),
                    Err(err) => {
                        log::warn!("discovery: {err}");
                        std::thread::sleep(Duration::from_secs(1));
                    }
                }
            };
            log::info!("connecting to {server}");
            let mut client = match Client::connect(server) {
                Ok(c) => c,
                Err(err) => {
                    log::error!("cannot open a socket: {err}");
                    return;
                }
            };
            match client.frame_reporter() {
                Ok(reporter) => {
                    let _ = connected.set(Connected { state: client.state(), reporter });
                }
                Err(err) => {
                    log::error!("cannot open a socket: {err}");
                    return;
                }
            }
            decoder::spawn(client.video_frames().expect("video receiver"), client.state(), output);
            match client.microphone() {
                Ok(mic) => crate::audio::spawn(app, client.speakers(), mic, client.state(), stop.clone()),
                Err(err) => log::error!("cannot open a socket for the microphone: {err}"),
            }
            client.run(&mut tracking, TRACKING_HZ, &stop);
        })
        .expect("spawn tracking thread");
}

fn system_property(name: &str) -> Option<String> {
    let name = std::ffi::CString::new(name).ok()?;
    let mut buf = [0u8; 92];
    // SAFETY: PROP_VALUE_MAX is 92 bytes.
    let n = unsafe { libc::__system_property_get(name.as_ptr(), buf.as_mut_ptr().cast()) };
    if n <= 0 {
        return None;
    }
    Some(String::from_utf8_lossy(&buf[..n as usize]).into_owned())
}

/// How the Quest compositor filters our eye images onto the display
/// (`XR_FB_composition_layer_settings`). They are encoded at about the panel's size, and the lens
/// shows the center of view at about 1.5x the panel's density, so the compositor enlarges them
/// there: plain bilinear looks soft next to Virtual Desktop and Steam Link, which both turn on its
/// sharpening. Quality sharpening by default; `adb shell setprop debug.openlink.layer_filter`
/// `none`, `sharpen` (normal), `supersample` or `quality-supersample` to compare, live.
fn layer_filter() -> xr::sys::CompositionLayerSettingsFlagsFB {
    use xr::sys::CompositionLayerSettingsFlagsFB as F;
    match system_property("debug.openlink.layer_filter").as_deref() {
        Some("none") => F::EMPTY,
        Some("sharpen") => F::NORMAL_SHARPENING,
        Some("supersample") => F::NORMAL_SUPER_SAMPLING,
        Some("quality-supersample") => F::QUALITY_SUPER_SAMPLING,
        _ => F::QUALITY_SHARPENING,
    }
}

fn server_from_property() -> Option<SocketAddr> {
    let s = system_property(SERVER_PROPERTY)?;
    let s = s.as_str();
    s.parse::<SocketAddr>()
        .ok()
        .or_else(|| s.parse::<std::net::IpAddr>().ok().map(|ip| SocketAddr::new(ip, openlink_protocol::SERVER_PORT)))
}

/// Logged every few seconds: what reached the screen, and what the network lost.
struct Stats {
    since: std::time::Instant,
    displayed: u32,
    new_pictures: u32,
    /// Per new picture: degrees and millimetres between the pose it was rendered for and the
    /// head pose when it is displayed (what reprojection has to make up).
    errors: Vec<(f32, f32)>,
    /// Per new picture with the head turning: how far ahead (ms, + = too far) of the actual
    /// head pose the render pose was, along the motion. Calibrates SteamVR's prediction.
    lead_ms: Vec<f32>,
    /// Per shown picture: ms between decode finishing and the render loop picking it up.
    slack_ms: Vec<f32>,
    /// Display frame period (ms), for the phase of the slack.
    period_ms: f32,
}

impl Default for Stats {
    fn default() -> Self {
        Self { since: std::time::Instant::now(), displayed: 0, new_pictures: 0, errors: Vec::new(), lead_ms: Vec::new(), slack_ms: Vec::new(), period_ms: 11.1 }
    }
}

impl Stats {
    fn pose_error(&mut self, rendered: &Pose, actual: &xr::Posef, omega: &xr::Vector3f) {
        let a = Quat::from_array(rendered.orientation);
        let o = actual.orientation;
        let b = Quat::from_xyzw(o.x, o.y, o.z, o.w);
        let degrees = a.angle_between(b).to_degrees();
        // World-frame rotation from the actual pose to the rendered one, as a rotation vector.
        let (axis, angle) = (a * b.inverse()).normalize().to_axis_angle();
        let angle = if angle > std::f32::consts::PI { angle - std::f32::consts::TAU } else { angle };
        let w = Vec3::new(omega.x, omega.y, omega.z);
        if w.length() > 30f32.to_radians() {
            self.lead_ms.push((axis * angle).dot(w) / w.length_squared() * 1e3);
        }
        let p = actual.position;
        let mm = (Vec3::from_array(rendered.position) - Vec3::new(p.x, p.y, p.z)).length() * 1e3;
        self.errors.push((degrees, mm));
    }

    /// Every few seconds: logs, and returns a vsync shift (ns) that moves pictures' arrival
    /// to `TARGET_SLACK_MS` before they are needed.
    fn report(&mut self, client: Option<&Arc<Mutex<ClientState>>>) -> Option<i64> {
        const TARGET_SLACK_MS: f32 = 6.0;
        let elapsed = self.since.elapsed().as_secs_f32();
        if elapsed < 5.0 {
            return None;
        }
        // Slack is circular in the frame period: 0.5 ms and 10.6 ms (at 11.1) are the same
        // phase, right at the boundary. Its circular mean is where pictures arrive.
        let mut shift = None;
        let p = self.period_ms.max(1.0);
        if self.slack_ms.len() >= 20 {
            let (mut sx, mut sy) = (0.0f32, 0.0f32);
            for s in &self.slack_ms {
                let a = s / p * std::f32::consts::TAU;
                sx += a.cos();
                sy += a.sin();
            }
            let mean = (sy.atan2(sx) / std::f32::consts::TAU * p).rem_euclid(p);
            let mut error = mean - TARGET_SLACK_MS;
            if error > p / 2.0 {
                error -= p;
            } else if error < -p / 2.0 {
                error += p;
            }
            log::info!("picture phase: arrive {mean:.1} ms before pickup (target {TARGET_SLACK_MS}); shifting vsync by {:.1} ms", error * 0.5);
            shift = Some((error * 0.5 * 1e6) as i64);
        }
        if !self.errors.is_empty() {
            let pct = |v: &mut Vec<f32>, q: f32| {
                v.sort_by(f32::total_cmp);
                v[((v.len() - 1) as f32 * q) as usize]
            };
            let mut deg: Vec<f32> = self.errors.iter().map(|e| e.0).collect();
            let mut mm: Vec<f32> = self.errors.iter().map(|e| e.1).collect();
            log::info!(
                "render pose vs head at display: {:.2} deg median, {:.2} p95, {:.2} max; {:.1} mm median, {:.1} p95",
                pct(&mut deg, 0.5),
                pct(&mut deg, 0.95),
                pct(&mut deg, 1.0),
                pct(&mut mm, 0.5),
                pct(&mut mm, 0.95)
            );
        }
        if self.slack_ms.len() >= 20 {
            let mut v = std::mem::take(&mut self.slack_ms);
            v.sort_by(f32::total_cmp);
            let q = |f: f32| v[((v.len() - 1) as f32 * f) as usize];
            log::info!(
                "picture slack (decoded -> picked up): p5 {:.1} ms, p25 {:.1}, median {:.1}, p75 {:.1}, p95 {:.1}",
                q(0.05),
                q(0.25),
                q(0.5),
                q(0.75),
                q(0.95)
            );
        }
        if self.lead_ms.len() >= 20 {
            let mut v = std::mem::take(&mut self.lead_ms);
            v.sort_by(f32::total_cmp);
            let median = v[v.len() / 2];
            // Close the loop on a slight under-prediction: falling short is corrected by the
            // display's reprojection, overshooting magnifies every small head movement.
            const TARGET_LEAD_MS: f32 = -8.0;
            let old = crate::tracking::HEAD_HORIZON_US.load(std::sync::atomic::Ordering::Relaxed);
            let new = (old as f32 - (median - TARGET_LEAD_MS) * 1000.0 * 0.5).clamp(0.0, 100_000.0) as i32;
            crate::tracking::HEAD_HORIZON_US.store(new, std::sync::atomic::Ordering::Relaxed);
            log::info!(
                "head prediction lead over {} turning frames: {:.1} ms median (p10 {:.1}, p90 {:.1}); + = predicts too far; horizon {:.1} -> {:.1} ms",
                v.len(),
                median,
                v[v.len() / 10],
                v[v.len() * 9 / 10],
                old as f32 / 1000.0,
                new as f32 / 1000.0
            );
        }
        if let Some(c) = client {
            let c = c.lock().unwrap();
            log::info!(
                "display {:.1} fps, new pictures {:.1} fps; video total {} frames, {} lost, {} dropped waiting for IDR, {} decoder overflow",
                self.displayed as f32 / elapsed,
                self.new_pictures as f32 / elapsed,
                c.video.frames,
                c.video.lost_frames,
                c.video.dropped_frames,
                c.video_overflow
            );
        }
        let period_ms = self.period_ms;
        *self = Self { period_ms, ..Self::default() };
        shift
    }
}
