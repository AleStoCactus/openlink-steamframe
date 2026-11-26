//! Loopback link to the OpenLink server process.
//!
//! Tracking is pushed to SteamVR **from this thread the moment a packet arrives**, rather than
//! being buffered until the next `RunFrame`, so a pose never waits on vrserver's main loop.

use crate::devices::{self, to_ovr_pose};
use crate::host::host;
use crate::ovr::{k_unTrackedDeviceIndexInvalid, DriverPose_t};
use crate::state::{display_cache, shared, DisplayConfig};
use openlink_protocol::{self as proto, time::monotonic_ns, FromDriver, Hand, ToDriver};
use openlink_tracking::{controller_raw_from_grip, to_driver_pose, ControllerModel};
use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// If no tracking arrives for this long the devices are reported disconnected.
const TRACKING_TIMEOUT: Duration = Duration::from_millis(500);
const HEARTBEAT_EVERY: Duration = Duration::from_millis(500);

pub struct Link {
    socket: UdpSocket,
    server: SocketAddr,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Link {
    pub fn start() -> std::io::Result<Self> {
        let socket = openlink_protocol::net::bind_udp((Ipv4Addr::LOCALHOST, proto::DRIVER_PORT))?;
        socket.set_read_timeout(Some(Duration::from_millis(50)))?;
        let server = SocketAddr::from((Ipv4Addr::LOCALHOST, proto::SERVER_DRIVER_PORT));
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let socket = socket.try_clone()?;
            let stop = stop.clone();
            std::thread::Builder::new()
                .name("openlink-link".into())
                .spawn(move || run(socket, server, stop))?
        };
        Ok(Self {
            socket,
            server,
            stop,
            thread: Some(thread),
        })
    }

    pub fn send(&self, msg: &FromDriver) {
        let _ = self.socket.send_to(&proto::encode(msg), self.server);
    }
}

impl Drop for Link {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn run(socket: UdpSocket, server: SocketAddr, stop: Arc<AtomicBool>) {
    let mut buf = [0u8; 2048];
    let mut last_tracking: Option<Instant> = None;
    let mut last_heartbeat = Instant::now() - HEARTBEAT_EVERY;

    while !stop.load(Ordering::Acquire) {
        if last_heartbeat.elapsed() >= HEARTBEAT_EVERY {
            let _ = socket.send_to(&proto::encode(&FromDriver::Heartbeat), server);
            last_heartbeat = Instant::now();
        }

        match socket.recv_from(&mut buf) {
            Ok((n, from)) if from.ip().is_loopback() => {
                match proto::decode::<ToDriver>(&buf[..n]) {
                    Ok(msg) => handle(msg, &mut last_tracking),
                    Err(e) => log::warn!("bad datagram from server: {e}"),
                }
            }
            Ok(_) => {}
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(e) => {
                log::error!("link socket: {e}");
                std::thread::sleep(Duration::from_millis(100));
            }
        }

        if last_tracking.is_some_and(|t| t.elapsed() > TRACKING_TIMEOUT) {
            log::info!("tracking timed out; marking devices disconnected");
            last_tracking = None;
            shared().tracking_active.store(false, Ordering::Release);
            report_disconnected();
        }
    }
}

/// What SteamVR is told of the head's velocity (see the head in `handle`).
const HMD_VELOCITY_SCALE: f64 = 0.01;

/// Share of the measured photon latency controllers are predicted over.
const CONTROLLER_PREDICTION: f64 = 0.6;

fn handle(msg: ToDriver, last_tracking: &mut Option<Instant>) {
    let s = shared();
    match msg {
        ToDriver::HeadsetConnected(info) => {
            log::info!(
                "headset connected: {} ({}x{} @ {} Hz)",
                info.model,
                info.eye_width,
                info.eye_height,
                info.current_refresh_rate
            );
            let d = DisplayConfig::from_headset(&info);
            if display_cache::load().as_ref() != Some(&info) {
                if let Err(e) = display_cache::save(&info) {
                    log::error!("cannot save the headset's display for the next SteamVR start: {e}");
                }
            }
            // SteamVR keeps the display it started with (see `DisplayConfig::renders_like`), so
            // a different headset needs a restart, which picks the saved display up. Only ask
            // once the cache holds it, or the restarted SteamVR would ask again, forever.
            if !s.startup_display.renders_like(&d)
                && display_cache::load().as_ref() == Some(&info)
                && !s.restart_requested.swap(true, Ordering::AcqRel)
            {
                let was = &s.startup_display;
                log::warn!(
                    "SteamVR started for a {}x{} display, the headset wants {}x{} \
                     (FOV {:?}, was {:?}): restarting SteamVR to apply it",
                    was.eye_width,
                    was.eye_height,
                    d.eye_width,
                    d.eye_height,
                    d.fov,
                    was.fov
                );
                host().request_restart("OpenLink: restarting SteamVR for the headset's display");
            }
            devices::apply_display(&d);
            *s.display.lock().unwrap() = d;
            devices::apply_controller_model(ControllerModel::from_headset_model(&info.model));
        }
        ToDriver::HeadsetDisconnected => {
            *last_tracking = None;
            s.tracking_active.store(false, Ordering::Release);
            report_disconnected();
        }
        ToDriver::Tracking(sample) => {
            *last_tracking = Some(Instant::now());
            s.tracking_active.store(true, Ordering::Release);
            let h = host();
            let now = monotonic_ns();
            let model = *s.controller_model.lock().unwrap();

            let hmd_id = s.hmd_id.load(Ordering::Acquire);
            // An invalid head sample carries no pose (identity, at the floor): SteamVR has been
            // seen rendering an eye from it anyway. Keep the last good head pose instead.
            if hmd_id != k_unTrackedDeviceIndexInvalid && sample.hmd.flags.is_valid() {
                // The head's velocity is reported scaled right down, so SteamVR practically does
                // not extrapolate it: SteamVR samples the head pose more than once per frame
                // (the pose it reports for a frame is not the one it rendered with), and with
                // real velocities each new sample re-predicts and the two drift apart frame to
                // frame, which the headset sees as world jitter. The headset predicts the head
                // itself and its reprojection corrects the rest.
                // Not zero: with zero velocity and a fast-moving pose SteamVR kept putting the
                // headset into standby (frames dropped to 0, then black) mid-game.
                let mut st = to_driver_pose(&sample.hmd, None, sample.sample_time_ns, now, &s.prediction);
                st.velocity *= HMD_VELOCITY_SCALE;
                st.angular_velocity *= HMD_VELOCITY_SCALE;
                crate::render_pose::record(now, st.position.as_vec3(), st.rotation.as_quat());
                h.pose_updated(hmd_id, &to_ovr_pose(&st));
            }

            for hand in [Hand::Left, Hand::Right] {
                let slot = &s.controllers[hand as usize];
                let id = slot.id.load(Ordering::Acquire);
                if id == k_unTrackedDeviceIndexInvalid {
                    continue;
                }
                match &sample.controllers[hand as usize] {
                    Some(c) => {
                        let off = controller_raw_from_grip(model, hand);
                        let st = to_driver_pose(
                            &c.motion,
                            Some(off),
                            sample.sample_time_ns,
                            now,
                            &s.prediction,
                        );
                        // SteamVR predicts to vsync only (SecondsFromVsyncToPhotons is 0): call the
                        // sample older so it predicts towards the photons. Only part of the way:
                        // swings decelerate, and linear extrapolation over the full latency
                        // overshoots (swings looked wider than they were).
                        let mut predicted = st;
                        predicted.time_offset_s -=
                            CONTROLLER_PREDICTION * s.photon_latency_us.load(Ordering::Acquire) as f64 * 1e-6;
                        h.pose_updated(id, &to_ovr_pose(&predicted));
                        if let Some(handles) = *slot.handles.lock().unwrap() {
                            devices::update_inputs(&handles, c, st.time_offset_s);
                        }
                    }
                    None => h.pose_updated(id, &DriverPose_t::disconnected()),
                }
            }
        }
        ToDriver::PhotonLatency { seconds } => devices::set_photon_latency(seconds),
        ToDriver::VsyncShift { shift_ns } => {
            #[cfg(any(target_os = "linux", windows))]
            crate::direct_mode::VSYNC_SHIFT_NS.fetch_add(shift_ns.clamp(-5_000_000, 5_000_000), Ordering::AcqRel);
            #[cfg(not(any(target_os = "linux", windows)))]
            let _ = shift_ns;
        }
        ToDriver::DisplayPeriod { period_ns } => {
            if (5_000_000..40_000_000).contains(&period_ns) {
                let old = s.vsync_period_ns.swap(period_ns, Ordering::AcqRel);
                if (old - period_ns).abs() > 1000 {
                    log::info!("pacing vsync to the headset display: {:.4} Hz", 1e9 / period_ns as f64);
                }
            }
        }
        ToDriver::Battery { .. } => {
            // TODO: Prop_DeviceBatteryPercentage_Float per device.
        }
    }
}

fn report_disconnected() {
    let s = shared();
    let h = host();
    let ids = [
        s.hmd_id.load(Ordering::Acquire),
        s.controllers[0].id.load(Ordering::Acquire),
        s.controllers[1].id.load(Ordering::Acquire),
    ];
    for id in ids
        .into_iter()
        .filter(|&id| id != k_unTrackedDeviceIndexInvalid)
    {
        h.pose_updated(id, &DriverPose_t::disconnected());
    }
}


