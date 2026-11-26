//! The app: OpenXR session and frame loop on the main thread, with a tracking/network thread
//! and a decoder thread beside it. The same structure as the Quest app; what differs is the
//! platform: SteamVR's arm64 OpenXR runtime on SteamOS, desktop GL over EGL, and V4L2 decode.
//!
//! Each displayed frame is the newest decoded picture, submitted as a projection layer at the
//! eye poses SteamVR rendered it with. The headset's compositor reprojects it to where the head
//! is now, so head motion stays smooth even when the stream lags.

use crate::decoder::{self, Output};
use crate::egl;
use crate::render::Renderer;
use crate::xr_egl::{self, EglGl};
use glam::Vec3;
use openlink_client::{Client, ClientState, FrameReporter};
use openlink_protocol::{Fov, HeadsetInfo};
use openlink_xr::tracking::{Actions, XrTracking};
use openlink_xr::{current_rate, eye_poses, monotonic_ns, request_sustained_performance, Stats};
use openxr as xr;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

const TRACKING_HZ: f64 = 500.0;
const GL_SRGB8_ALPHA8: u32 = 0x8C43;
const GL_RGBA8: u32 = 0x8058;
/// Where SteamVR keeps its OpenXR loader on the Steam Frame. The loader on the default search
/// path may be a stub, so this one is tried first.
const STEAMVR_LOADER: &str = "/opt/steamvr/bin/linuxarm64/libopenxr_loader.so";

/// Set by SIGINT/SIGTERM: leave the session cleanly.
static QUIT: AtomicBool = AtomicBool::new(false);

struct Eye {
    swapchain: xr::Swapchain<EglGl>,
    images: Vec<u32>,
    width: i32,
    height: i32,
}

pub struct Options {
    /// The PC to connect to; found by LAN broadcast if not given.
    pub server: Option<SocketAddr>,
    /// Debug: show each eye the other's picture.
    pub swap_eyes: bool,
}

pub fn run(options: Options) -> Result<(), String> {
    fn e(what: &'static str) -> impl Fn(xr::sys::Result) -> String {
        move |err| format!("{what}: {err}")
    }
    install_signal_handlers();

    let entry = load_openxr()?;
    let available = entry.enumerate_extensions().map_err(e("extensions"))?;
    if !available.khr_opengl_enable || !available.mndx_egl_enable {
        return Err(format!(
            "the OpenXR runtime lacks {} (needed for OpenGL over EGL)",
            [("XR_KHR_opengl_enable", available.khr_opengl_enable), ("XR_MNDX_egl_enable", available.mndx_egl_enable)]
                .iter()
                .filter(|(_, have)| !have)
                .map(|(n, _)| *n)
                .collect::<Vec<_>>()
                .join(" and ")
        ));
    }
    let mut exts = xr::ExtensionSet::default();
    exts.khr_opengl_enable = true;
    exts.mndx_egl_enable = true;
    exts.khr_convert_timespec_time = available.khr_convert_timespec_time;
    exts.fb_display_refresh_rate = available.fb_display_refresh_rate;
    exts.ext_performance_settings = available.ext_performance_settings;
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
            &(),
        )
        .map_err(e("xrCreateInstance"))?;
    let runtime = instance.properties().map_err(e("instance properties"))?;
    let system = instance.system(xr::FormFactor::HEAD_MOUNTED_DISPLAY).map_err(e("xrGetSystem"))?;
    let props = instance.system_properties(system).map_err(e("system properties"))?;
    log::info!("OpenXR runtime {} {} on {}", runtime.runtime_name, runtime.runtime_version, props.system_name);
    if !exts.khr_convert_timespec_time {
        log::warn!("XR_KHR_convert_timespec_time unavailable: tracking timestamps assume XrTime is CLOCK_MONOTONIC");
    }
    let req = instance.graphics_requirements::<EglGl>(system).map_err(e("OpenGL requirements"))?;
    let min = req.min_api_version_supported;

    let egl = egl::Context::new((min.major(), min.minor()))?;
    let mut renderer = Renderer::new(egl.gl())?;
    // SAFETY: a valid desktop GL context, current on this thread, matching the binding.
    let (session, mut waiter, mut stream) = unsafe {
        instance.create_session::<EglGl>(
            system,
            &xr_egl::SessionCreateInfo {
                get_proc_address: egl::get_proc_address,
                display: egl.display.as_ptr(),
                config: egl.config.as_ptr(),
                context: egl.context.as_ptr(),
            },
        )
    }
    .map_err(e("xrCreateSession"))?;
    let formats = session.enumerate_swapchain_formats().map_err(e("swapchain formats"))?;
    let (format, srgb) = if formats.contains(&GL_SRGB8_ALPHA8) {
        (GL_SRGB8_ALPHA8, true)
    } else if formats.contains(&GL_RGBA8) {
        log::warn!("no sRGB swapchain format; colours are written as they are");
        (GL_RGBA8, false)
    } else {
        return Err(format!("no RGBA8 swapchain format among {formats:#x?}"));
    };

    // Touch bindings. On the Steam Frame, SteamVR maps them onto the Frame controllers (its
    // documented fallback for apps without a Frame controller binding), and the PC side keeps
    // presenting Touch controllers to games, whose Touch bindings then apply unchanged.
    let actions = Actions::new(&instance).map_err(e("actions"))?;
    session.attach_action_sets(&[&actions.set]).map_err(e("attach actions"))?;
    let mut actions = Some(actions);
    let stage = session
        .create_reference_space(xr::ReferenceSpaceType::STAGE, xr::Posef::IDENTITY)
        .map_err(e("stage space"))?;
    let views = instance
        .enumerate_view_configuration_views(system, xr::ViewConfigurationType::PRIMARY_STEREO)
        .map_err(e("view configuration"))?;
    let head = session
        .create_reference_space(xr::ReferenceSpaceType::VIEW, xr::Posef::IDENTITY)
        .map_err(e("view space"))?;
    let recommended = (views[0].recommended_image_rect_width, views[0].recommended_image_rect_height);
    let mut eyes: Vec<Eye> = Vec::new();

    let mut refresh_rates = Vec::new();
    if exts.fb_display_refresh_rate {
        if let Ok(r) = session.enumerate_display_refresh_rates() {
            refresh_rates = r;
        }
    }
    // Without a decoder the app still streams tracking and input (useful on its own for
    // testing), but shows nothing.
    let decoders = decoder::supported();
    if decoders.is_empty() {
        log::error!("no hardware video decoder: tracking and input only, no picture");
    }

    let output = Arc::new(Mutex::new(Output::default()));
    let mut session_running = false;
    let client: Arc<OnceLock<Connected>> = Arc::new(OnceLock::new());
    let mut last_present_ns = None;
    let mut applied_rate: Option<f32> = None;
    let stop = Arc::new(AtomicBool::new(false));
    let mut ipd = 0.063f32;
    let mut fov = [xr::Fovf::default(); 2];
    let mut event_storage = xr::EventDataBuffer::new();
    let mut quit = false;
    let mut stats = Stats::default();
    let mut period_window: Option<(i64, std::time::Instant)> = None;
    if options.swap_eyes {
        log::warn!("debug: eyes swapped");
    }

    while !quit {
        if QUIT.load(Ordering::Relaxed) {
            if session_running {
                // Ask to leave; the runtime answers with STOPPING, then EXITING.
                let _ = session.request_exit();
                QUIT.store(false, Ordering::Relaxed);
            } else {
                break;
            }
        }
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
                InstanceLossPending(_) => quit = true,
                _ => {}
            }
        }
        if !session_running {
            std::thread::sleep(Duration::from_millis(50));
            continue;
        }

        let state = waiter.wait().map_err(e("xrWaitFrame"))?;
        // Measure the display's real period and pace SteamVR to it.
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
                    // Without XR_FB_display_refresh_rate the rate is whatever the display runs.
                    let measured = 1e9 / state.predicted_display_period.as_nanos().max(1) as f32;
                    let current = if exts.fb_display_refresh_rate { current_rate(&session, true) } else { measured.round() };
                    let rates = if refresh_rates.is_empty() { vec![current] } else { refresh_rates.clone() };
                    let info = HeadsetInfo {
                        model: model_name(&props.system_name),
                        serial: format!("OpenLink-{:x}", props.system_id.into_raw()),
                        eye_width: recommended.0,
                        eye_height: recommended.1,
                        refresh_rates: rates,
                        current_refresh_rate: current,
                        ipd_m: ipd,
                        fov: fov.map(|f| Fov {
                            angle_left: f.angle_left,
                            angle_right: f.angle_right,
                            angle_up: f.angle_up,
                            angle_down: f.angle_down,
                        }),
                        decoders: decoders.clone(),
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
                        grips: [
                            actions.grip_space(&session, 0).map_err(e("grip space"))?,
                            actions.grip_space(&session, 1).map_err(e("grip space"))?,
                        ],
                        actions,
                        instance: instance.clone(),
                        info,
                        last_hmd: None,
                        lost_hmd: (0, None),
                    };
                    start_client(tracking, options.server, output.clone(), client.clone(), stop.clone());
                }
            }
        }

        // Size the swapchains to the stream, and follow the refresh rate the server streams at.
        let welcome = client.get().and_then(|c| c.state.lock().unwrap().welcome.clone());
        if let Some(cfg) = &welcome {
            let size = (cfg.eye_width as i32, cfg.eye_height as i32);
            if eyes.first().is_none_or(|e| (e.width, e.height) != size) {
                eyes = [0, 1].map(|_| create_eye(&session, format, size)).into_iter().collect::<Result<_, _>>()?;
                log::info!("eye swapchains {}x{}", size.0, size.1);
            }
            if exts.fb_display_refresh_rate && applied_rate != Some(cfg.refresh_rate) {
                match session.request_display_refresh_rate(cfg.refresh_rate) {
                    Ok(()) => log::info!("display refresh rate {} Hz", cfg.refresh_rate),
                    Err(err) => log::warn!("cannot switch to {} Hz: {err}", cfg.refresh_rate),
                }
                applied_rate = Some(cfg.refresh_rate);
            }
        }

        let latest = output.lock().unwrap().next();
        if let Some(d) = &latest {
            stats.slack_ms.push((openlink_protocol::time::monotonic_ns() - d.picture.decoded_ns) as f32 * 1e-6);
        }
        stats.displayed += 1;
        stats.new_pictures += latest.is_some() as u32;
        let shown = renderer.update(&egl, latest);
        if stats.since.elapsed() >= Duration::from_secs(5) {
            let mut out = output.lock().unwrap();
            if out.trimmed + out.unmatched > 0 {
                log::info!(
                    "pictures dropped: {} from the backlog, {} without frame info ({} decoded)",
                    out.trimmed,
                    out.unmatched,
                    out.decoded
                );
            }
            (out.trimmed, out.unmatched, out.decoded) = (0, 0, 0);
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
            let source = if options.swap_eyes { 1 - i } else { i };
            renderer.draw(source, eye.images[index as usize], eye.width, eye.height, srgb);
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
        stream
            .end(state.predicted_display_time, xr::EnvironmentBlendMode::OPAQUE, &[&layer])
            .map_err(e("xrEndFrame"))?;
    }
    stop.store(true, Ordering::Release);
    // Let the tracking thread say goodbye to the server.
    std::thread::sleep(Duration::from_millis(20));
    Ok(())
}

struct Connected {
    state: Arc<Mutex<ClientState>>,
    reporter: FrameReporter,
}

/// `OPENLINK_OPENXR_LOADER`, then SteamVR's own loader, then the system's.
fn load_openxr() -> Result<xr::Entry, String> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(p) = std::env::var_os("OPENLINK_OPENXR_LOADER") {
        candidates.push(p.into());
    }
    candidates.extend([STEAMVR_LOADER, "libopenxr_loader.so.1", "libopenxr_loader.so"].map(PathBuf::from));
    let mut errors = Vec::new();
    for path in candidates {
        // SAFETY: an OpenXR loader library.
        match unsafe { xr::Entry::load_from(&path, &()) } {
            Ok(entry) => {
                // A stub loader loads but enumerates nothing.
                if entry.enumerate_extensions().is_ok() {
                    log::info!("OpenXR loader {}", path.display());
                    return Ok(entry);
                }
                errors.push(format!("{}: no runtime", path.display()));
            }
            Err(err) => errors.push(format!("{}: {err}", path.display())),
        }
    }
    Err(format!("no usable OpenXR loader ({})", errors.join("; ")))
}

/// SteamVR names the Frame's system after its codename.
fn model_name(system_name: &str) -> String {
    if let Ok(m) = std::env::var("OPENLINK_MODEL") {
        return m;
    }
    let s = system_name.trim();
    if s.ends_with(": cv") || s.to_ascii_lowercase().contains("steam frame") {
        "Steam Frame".into()
    } else {
        s.to_string()
    }
}

fn create_eye(session: &xr::Session<EglGl>, format: u32, (width, height): (i32, i32)) -> Result<Eye, String> {
    let swapchain = session
        .create_swapchain(&xr::SwapchainCreateInfo {
            create_flags: xr::SwapchainCreateFlags::EMPTY,
            usage_flags: xr::SwapchainUsageFlags::COLOR_ATTACHMENT | xr::SwapchainUsageFlags::SAMPLED,
            format,
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

/// Connects (discovering the server unless one was named) and streams tracking on its own
/// thread; decoded video flows into `output`. `connected` is set once connected.
fn start_client(
    mut tracking: XrTracking,
    server: Option<SocketAddr>,
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
                if let Some(addr) = server {
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
            client.run(&mut tracking, TRACKING_HZ, &stop);
        })
        .expect("spawn tracking thread");
}

/// `host` or `host:port`.
pub fn parse_server(s: &str) -> Option<SocketAddr> {
    s.parse::<SocketAddr>()
        .ok()
        .or_else(|| s.parse::<std::net::IpAddr>().ok().map(|ip| SocketAddr::new(ip, openlink_protocol::SERVER_PORT)))
}

fn install_signal_handlers() {
    extern "C" fn on_signal(_: libc::c_int) {
        QUIT.store(true, Ordering::Relaxed);
    }
    for sig in [libc::SIGINT, libc::SIGTERM] {
        // SAFETY: the handler only stores to an atomic, which is async-signal-safe.
        unsafe { libc::signal(sig, on_signal as *const () as libc::sighandler_t) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_the_frame() {
        assert_eq!(model_name("SteamVR/OpenXR : cv"), "Steam Frame");
        assert_eq!(model_name("Monado: Simulated HMD"), "Monado: Simulated HMD");
    }

    #[test]
    fn parses_server_addresses() {
        assert_eq!(parse_server("192.168.1.10"), Some(SocketAddr::from(([192, 168, 1, 10], openlink_protocol::SERVER_PORT))));
        assert_eq!(parse_server("10.0.0.2:1234"), Some(SocketAddr::from(([10, 0, 0, 2], 1234))));
        assert_eq!(parse_server("pc.local"), None);
    }
}
