//! A fake Quest for testing the PC side (server + SteamVR driver) without a headset.
//!
//! ```text
//! mock-headset [SERVER_ADDR] [--rate HZ] [--delay-ms MS] [--spin-tilted] [--video-out PATH]
//! ```
//!
//! * default: head gently sways, both controllers do fast Beat Saber style vertical swings
//!   (peak ~11 rad/s) with exact analytic velocities, triggers pulse.
//! * `--delay-ms N`: report every sample as N ms old. If SteamVR's prediction is fed correctly
//!   the controllers in the SteamVR view stay smooth and in phase regardless of N; any error in
//!   our velocities or time offsets shows up as lag or overshoot that grows with N.
//! * `--spin-tilted`: controllers spin at a constant rate about an axis tilted away from every
//!   world and body axis. Used to settle whether `DriverPose_t::vecAngularVelocity` is in
//!   world or local space: with the wrong space, the spin wobbles instead of staying on-axis.
//! * `--yaw-deg A --yaw-hz F`: head yaw oscillation (default 8.6 deg at 0.2 Hz); with
//!   `--yaw-steps` the head jumps between -A and +A instead, holding still in between.
//! * `--still-controllers`: controllers hold still, as world-fixed reference points.
//! * `--video-out PATH`: write the received video stream (Annex B) to PATH. Either way, video
//!   statistics and the frames' render poses are logged every few seconds.

use glam::{DQuat, DVec3};
use openlink_client::{Client, TrackingSource};
use openlink_protocol::{
    buttons, Codec, ControllerInput, ControllerState, Fov, HapticPulse, HeadsetInfo, Motion,
    MotionFlags, Pose, TrackingSample,
};
use std::f64::consts::{PI, TAU};
use std::net::{SocketAddr, ToSocketAddrs};
use std::sync::atomic::AtomicBool;

struct Mock {
    start_ns: i64,
    delay_ns: i64,
    spin_tilted: bool,
    /// Head yaw oscillation: amplitude (rad) and frequency (Hz).
    yaw: (f64, f64),
    /// Jump between the yaw extremes instead of oscillating smoothly.
    yaw_steps: bool,
    /// Controllers hold still (world-fixed reference points for checking video against poses).
    still_controllers: bool,
}

fn motion(p: DVec3, q: DQuat, v: DVec3, w: DVec3) -> Motion {
    let q = q.normalize();
    Motion {
        pose: Pose {
            position: p.as_vec3().to_array(),
            orientation: q.as_quat().to_array(),
        },
        linear_velocity: v.as_vec3().to_array(),
        angular_velocity: w.as_vec3().to_array(),
        flags: MotionFlags::ALL,
    }
}

impl Mock {
    fn controller(&self, t: f64, side: f64) -> ControllerState {
        let center = DVec3::new(0.25 * side, 1.25, -0.35);
        let m = if self.spin_tilted {
            let axis = DVec3::new(0.5, 0.7, 0.3 * side).normalize();
            let omega = 6.0;
            let q0 = DQuat::from_rotation_x(PI / 2.0) * DQuat::from_rotation_z(0.4);
            let q = DQuat::from_axis_angle(axis, omega * t) * q0;
            motion(center, q, DVec3::ZERO, axis * omega)
        } else {
            // theta(t) = A sin(wt): a vertical saber swing pivoting around the elbow.
            let (a, f, r) = (if self.still_controllers { 0.0 } else { 1.2 }, 1.5 + 0.1 * side, 0.18);
            let wt = TAU * f * t;
            let theta = a * wt.sin();
            let theta_dot = a * TAU * f * wt.cos();
            let p = center + r * DVec3::new(0.0, theta.sin(), -theta.cos());
            let v = r * theta_dot * DVec3::new(0.0, theta.cos(), theta.sin());
            motion(
                p,
                DQuat::from_rotation_x(theta),
                v,
                DVec3::new(theta_dot, 0.0, 0.0),
            )
        };
        let pressed = (t * 2.0).fract() < 0.5;
        ControllerState {
            motion: m,
            input: ControllerInput {
                buttons: if pressed { buttons::TRIGGER_TOUCH } else { 0 },
                trigger: if pressed { 1.0 } else { 0.0 },
                grip: 0.0,
                thumbstick: [0.0, 0.0],
            },
        }
    }
}

impl TrackingSource for Mock {
    fn info(&self) -> HeadsetInfo {
        let left = Fov {
            angle_left: -0.90,
            angle_right: 0.75,
            angle_up: 0.82,
            angle_down: -0.88,
        };
        let right = Fov {
            angle_left: -0.75,
            angle_right: 0.90,
            ..left
        };
        HeadsetInfo {
            model: "Meta Quest 3 (mock)".into(),
            serial: "MOCK0001".into(),
            eye_width: 2064,
            eye_height: 2208,
            refresh_rates: vec![72.0, 80.0, 90.0, 120.0],
            current_refresh_rate: 90.0,
            ipd_m: 0.064,
            fov: [left, right],
            decoders: vec![Codec::H264, Codec::H265, Codec::Av1],
        }
    }

    fn sample(&mut self, now_ns: i64) -> TrackingSample {
        let sample_time_ns = now_ns - self.delay_ns;
        let t = (sample_time_ns - self.start_ns) as f64 * 1e-9;
        let (amp, f) = self.yaw;
        let (yaw, yaw_dot) = if self.yaw_steps {
            // Hold at -amp, then +amp, each for half a period: no velocity to predict with.
            (if (t * f).fract() < 0.5 { -amp } else { amp }, 0.0)
        } else {
            (amp * (TAU * f * t).sin(), amp * TAU * f * (TAU * f * t).cos())
        };
        TrackingSample {
            sample_time_ns,
            hmd: motion(
                DVec3::new(0.0, 1.7, 0.0),
                DQuat::from_rotation_y(yaw),
                DVec3::ZERO,
                DVec3::Y * yaw_dot,
            ),
            controllers: [
                Some(self.controller(t, -1.0)),
                Some(self.controller(t, 1.0)),
            ],
        }
    }

    fn haptics(&mut self, p: HapticPulse) {
        log::info!(
            "haptics: {:?} {:.0} ms @ {:.0} Hz, amp {:.2}",
            p.hand,
            p.duration_s * 1e3,
            p.frequency_hz,
            p.amplitude
        );
    }
}

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let mut args = std::env::args().skip(1);
    let mut server = format!("127.0.0.1:{}", openlink_protocol::SERVER_PORT);
    let mut rate = 500.0;
    let mut delay_ms = 0.0;
    let mut spin_tilted = false;
    let mut video_out: Option<String> = None;
    let (mut yaw_deg, mut yaw_hz) = (8.6, 0.2);
    let mut yaw_steps = false;
    let mut still_controllers = false;
    while let Some(a) = args.next() {
        match a.as_str() {
            "--rate" => rate = args.next().and_then(|v| v.parse().ok()).expect("--rate HZ"),
            "--delay-ms" => {
                delay_ms = args
                    .next()
                    .and_then(|v| v.parse().ok())
                    .expect("--delay-ms MS")
            }
            "--spin-tilted" => spin_tilted = true,
            "--video-out" => video_out = Some(args.next().expect("--video-out PATH")),
            "--yaw-deg" => yaw_deg = args.next().and_then(|v| v.parse().ok()).expect("--yaw-deg DEG"),
            "--yaw-steps" => yaw_steps = true,
            "--still-controllers" => still_controllers = true,
            "--yaw-hz" => yaw_hz = args.next().and_then(|v| v.parse().ok()).expect("--yaw-hz HZ"),
            "-h" | "--help" => {
                println!(
                    "usage: mock-headset [SERVER_ADDR] [--rate HZ] [--delay-ms MS] [--spin-tilted] [--video-out PATH]"
                );
                return;
            }
            other => server = other.to_string(),
        }
    }
    let addr: SocketAddr = server
        .to_socket_addrs()
        .ok()
        .and_then(|mut a| a.next())
        .unwrap_or_else(|| panic!("cannot resolve {server}"));

    let mut mock = Mock {
        start_ns: openlink_protocol::time::monotonic_ns(),
        delay_ns: (delay_ms * 1e6) as i64,
        spin_tilted,
        yaw: (f64::to_radians(yaw_deg), yaw_hz),
        yaw_steps,
        still_controllers,
    };
    let mut client = Client::connect(addr).expect("open socket");
    log::info!(
        "mock headset -> {addr} at {rate} Hz (delay {delay_ms} ms, spin_tilted {spin_tilted})"
    );
    let frames = client.video_frames().expect("video receiver");
    let state = client.state();
    std::thread::spawn(move || video_sink(frames, state, video_out));
    let stop = AtomicBool::new(false);
    client.run(&mut mock, rate, &stop);
}

/// Stands in for the decoder: optionally writes the stream, and logs what arrives.
fn video_sink(
    frames: std::sync::mpsc::Receiver<openlink_protocol::video::Frame>,
    state: std::sync::Arc<std::sync::Mutex<openlink_client::ClientState>>,
    out: Option<String>,
) {
    use std::io::Write;
    let mut file = out.map(|p| std::fs::File::create(&p).unwrap_or_else(|e| panic!("{p}: {e}")));
    let (mut since, mut count, mut bytes, mut idrs) = (std::time::Instant::now(), 0u32, 0usize, 0u32);
    for f in frames {
        if let Some(file) = &mut file {
            file.write_all(&f.bitstream).expect("write video");
        }
        count += 1;
        bytes += f.bitstream.len();
        idrs += f.info.idr as u32;
        let elapsed = since.elapsed().as_secs_f64();
        if elapsed >= 5.0 {
            let st = state.lock().unwrap().clone();
            let p = f.info.render_pose;
            log::info!(
                "video: {:.1} fps, {:.1} Mbit/s, {idrs} IDR, total {} frames / {} lost / {} dropped / {} overflow; render pose {:.2?} {:.3?}",
                count as f64 / elapsed,
                bytes as f64 * 8.0 / elapsed / 1e6,
                st.video.frames,
                st.video.lost_frames,
                st.video.dropped_frames,
                st.video_overflow,
                p.position,
                p.orientation,
            );
            (since, count, bytes, idrs) = (std::time::Instant::now(), 0, 0, 0);
        }
    }
}
