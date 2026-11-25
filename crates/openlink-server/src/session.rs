//! Headset session and driver relay.
//!
//! Two threads:
//! * **headset**: receives tracking/input from the headset, rewrites the timestamps into PC
//!   time and forwards them to the driver immediately; runs clock sync.
//! * **driver**: receives heartbeats and haptics from the SteamVR driver and routes haptics to
//!   the headset.

use crate::config::Settings;
use openlink_protocol::{
    self as proto,
    clock::{ClockSample, ClockSync},
    time::monotonic_ns,
    Codec, FromDriver, HeadsetInfo, HeadsetMessage, ServerMessage, StreamConfig, ToDriver,
};
use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const HEADSET_TIMEOUT: Duration = Duration::from_secs(2);
const DRIVER_TIMEOUT: Duration = Duration::from_secs(2);
const CLOCK_PING_WARMUP: Duration = Duration::from_millis(20);
const CLOCK_PING_STEADY: Duration = Duration::from_millis(250);

#[derive(Debug, Clone, Default)]
pub struct Status {
    pub driver_connected: bool,
    pub headset: Option<HeadsetStatus>,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone)]
pub struct HeadsetStatus {
    pub addr: SocketAddr,
    pub info: HeadsetInfo,
    pub clock_offset_ns: Option<i64>,
    pub min_rtt_ns: Option<i64>,
    pub tracking_hz: f32,
    /// Age of the newest tracking sample when it reached the server.
    pub sample_age_ms: f32,
    /// Present-to-photon latency of the video, as measured by the headset.
    pub photon_latency_ms: Option<f32>,
}

struct Shared {
    status: Arc<Mutex<Status>>,
    settings: Arc<Mutex<Settings>>,
    headset_addr: Mutex<Option<SocketAddr>>,
    headset_info: Mutex<Option<HeadsetInfo>>,
    /// The headset needs an IDR: it just connected, or reported lost video.
    idr_requested: AtomicBool,
    stop: AtomicBool,
}

/// Where the video thread sends frames: the session's own socket, so the headset sees one
/// peer, and the current headset address.
pub struct VideoSink {
    sock: UdpSocket,
    shared: Arc<Shared>,
}

impl VideoSink {
    pub fn headset(&self) -> Option<SocketAddr> {
        *self.shared.headset_addr.lock().unwrap()
    }

    pub fn send(&self, datagram: &[u8], to: SocketAddr) -> std::io::Result<()> {
        self.sock.send_to(datagram, to).map(|_| ())
    }

    /// Whether an IDR was requested since the last call.
    pub fn take_idr_request(&self) -> bool {
        self.shared.idr_requested.swap(false, Ordering::AcqRel)
    }
}

pub struct Session {
    shared: Arc<Shared>,
    sock: UdpSocket,
    threads: Vec<JoinHandle<()>>,
}

impl Session {
    pub fn start(
        status: Arc<Mutex<Status>>,
        settings: Arc<Mutex<Settings>>,
    ) -> std::io::Result<Self> {
        let headset_sock = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, proto::SERVER_PORT))?;
        headset_sock.set_read_timeout(Some(Duration::from_millis(10)))?;
        let driver_sock = UdpSocket::bind((Ipv4Addr::LOCALHOST, proto::SERVER_DRIVER_PORT))?;
        let video_sock = headset_sock.try_clone()?;
        set_send_buffer(&video_sock, 8 << 20);
        driver_sock.set_read_timeout(Some(Duration::from_millis(100)))?;

        let shared = Arc::new(Shared {
            status,
            settings,
            headset_addr: Mutex::new(None),
            headset_info: Mutex::new(None),
            idr_requested: AtomicBool::new(false),
            stop: AtomicBool::new(false),
        });

        let headset_thread = {
            let (shared, hs, ds) = (
                shared.clone(),
                headset_sock.try_clone()?,
                driver_sock.try_clone()?,
            );
            std::thread::Builder::new()
                .name("headset".into())
                .spawn(move || headset_loop(&shared, hs, ds))?
        };
        let driver_thread = {
            let shared = shared.clone();
            std::thread::Builder::new()
                .name("driver".into())
                .spawn(move || driver_loop(&shared, driver_sock, headset_sock))?
        };
        log::info!("listening for headsets on UDP {}", proto::SERVER_PORT);
        Ok(Self {
            shared,
            sock: video_sock,
            threads: vec![headset_thread, driver_thread],
        })
    }

    pub fn video_sink(&self) -> std::io::Result<VideoSink> {
        Ok(VideoSink { sock: self.sock.try_clone()?, shared: self.shared.clone() })
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}

/// A frame is sent as one burst of a few hundred datagrams; the default buffer drops some.
fn set_send_buffer(sock: &UdpSocket, bytes: i32) {
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::AsRawFd;
        // SAFETY: valid socket and option value.
        let r = unsafe {
            libc::setsockopt(
                sock.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_SNDBUF,
                (&raw const bytes).cast(),
                std::mem::size_of::<i32>() as libc::socklen_t,
            )
        };
        if r != 0 {
            log::warn!("cannot enlarge the UDP send buffer: {}", std::io::Error::last_os_error());
        }
    }
    #[cfg(not(target_os = "linux"))]
    let _ = (sock, bytes);
}

fn driver_addr() -> SocketAddr {
    SocketAddr::from((Ipv4Addr::LOCALHOST, proto::DRIVER_PORT))
}

fn to_driver(sock: &UdpSocket, msg: &ToDriver) {
    // The driver may simply not be running yet; that is not an error.
    let _ = sock.send_to(&proto::encode(msg), driver_addr());
}

fn stream_config(settings: &Settings, info: &HeadsetInfo) -> StreamConfig {
    use crate::config::CodecPreference;
    let pref = match settings.codec {
        CodecPreference::Auto => None,
        CodecPreference::Fixed(c) => Some(c),
    };
    // Until encoders are probed, assume H.264/HEVC are available everywhere.
    let codec = crate::encoder::negotiate(&[Codec::H264, Codec::H265], &info.decoders, pref)
        .unwrap_or(Codec::H264);
    let scale = settings.resolution_scale.clamp(0.25, 2.0);
    let even = |v: f32| ((v as u32) + 1) & !1;
    // The configured rate if the headset has it, else its closest one.
    let refresh_rate = info
        .refresh_rates
        .iter()
        .copied()
        .min_by(|a, b| (a - settings.refresh_rate).abs().total_cmp(&(b - settings.refresh_rate).abs()))
        .unwrap_or(settings.refresh_rate);
    StreamConfig {
        codec,
        refresh_rate,
        eye_width: even(info.eye_width as f32 * scale),
        eye_height: even(info.eye_height as f32 * scale),
        bitrate_bps: settings.bitrate_mbps as u64 * 1_000_000,
    }
}

fn headset_loop(shared: &Shared, sock: UdpSocket, driver_sock: UdpSocket) {
    let mut buf = [0u8; 2048];
    let mut clock = ClockSync::default();
    let mut last_rx = Instant::now();
    let mut last_ping = Instant::now();
    let mut rate_window = (Instant::now(), 0u32);
    let mut latency = LatencyMeter::default();
    let mut invalid_hmd = 0u32;
    // Controller samples, with angular velocity flagged valid, peak |w| and |v| (diagnostics).
    let mut ctrl_stats = (0u32, 0u32, 0f32, 0f32);

    while !shared.stop.load(Ordering::Acquire) {
        let current = *shared.headset_addr.lock().unwrap();

        if let Some(addr) = current {
            if last_rx.elapsed() > HEADSET_TIMEOUT {
                log::info!("headset {addr} timed out");
                disconnect(shared, &driver_sock, &mut clock);
                continue;
            }
            let interval = if clock.offset_ns().is_some() {
                CLOCK_PING_STEADY
            } else {
                CLOCK_PING_WARMUP
            };
            if last_ping.elapsed() >= interval {
                let _ = sock.send_to(
                    &proto::encode(&ServerMessage::ClockPing { t0: monotonic_ns() }),
                    addr,
                );
                last_ping = Instant::now();
            }
        }

        let (n, from) = match sock.recv_from(&mut buf) {
            Ok(v) => v,
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                continue
            }
            Err(e) => {
                log::warn!("headset socket: {e}");
                continue;
            }
        };
        let t_rx = monotonic_ns();
        let msg = match proto::decode::<HeadsetMessage>(&buf[..n]) {
            Ok(m) => m,
            Err(e) => {
                log::debug!("ignoring datagram from {from}: {e}");
                continue;
            }
        };

        if let HeadsetMessage::Hello(info) = &msg {
            if current != Some(from) {
                log::info!("headset {} connected from {from}", info.model);
                clock = ClockSync::default();
                shared.idr_requested.store(true, Ordering::Release);
            }
            let cfg = stream_config(&shared.settings.lock().unwrap(), info);
            // The headset switches to the stream's rate on Welcome; everything here (SteamVR,
            // the encoder) runs at that rate, not the one it happened to report.
            let mut info = info.clone();
            info.current_refresh_rate = cfg.refresh_rate;
            let info = &info;
            let _ = sock.send_to(&proto::encode(&ServerMessage::Welcome(cfg)), from);
            *shared.headset_addr.lock().unwrap() = Some(from);
            *shared.headset_info.lock().unwrap() = Some(info.clone());
            to_driver(&driver_sock, &ToDriver::HeadsetConnected(info.clone()));
            shared.status.lock().unwrap().headset = Some(HeadsetStatus {
                addr: from,
                info: info.clone(),
                clock_offset_ns: None,
                min_rtt_ns: None,
                tracking_hz: 0.0,
                sample_age_ms: 0.0,
                photon_latency_ms: None,
            });
            last_rx = Instant::now();
            continue;
        }
        if current != Some(from) {
            // Not our headset, or one still streaming to a previous run of the server: make it
            // start over with Hello.
            if !matches!(msg, HeadsetMessage::Disconnect) {
                let _ = sock.send_to(&proto::encode(&ServerMessage::Disconnect), from);
            }
            continue;
        }
        last_rx = Instant::now();

        match msg {
            HeadsetMessage::Hello(_) => unreachable!(),
            HeadsetMessage::ClockPong { t0, t1, t2 } => {
                if let Some(s) = ClockSample::from_exchange(t0, t1, t2, t_rx) {
                    clock.push(s);
                    if let Some(h) = shared.status.lock().unwrap().headset.as_mut() {
                        h.clock_offset_ns = clock.offset_ns();
                        h.min_rtt_ns = clock.min_rtt_ns();
                    }
                }
            }
            HeadsetMessage::Tracking { sample, .. } => {
                let Some(pc_time) = clock.headset_to_pc(sample.sample_time_ns) else {
                    continue;
                };
                let mut sample = sample;
                sample.sample_time_ns = pc_time;
                if !sample.hmd.flags.is_valid() {
                    invalid_hmd += 1;
                }
                for c in sample.controllers.iter().flatten() {
                    let m = &c.motion;
                    let norm = |v: [f32; 3]| (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
                    ctrl_stats.0 += 1;
                    if m.flags.has(proto::MotionFlags::ANGULAR_VELOCITY_VALID) {
                        ctrl_stats.1 += 1;
                    }
                    ctrl_stats.2 = ctrl_stats.2.max(norm(m.angular_velocity));
                    ctrl_stats.3 = ctrl_stats.3.max(norm(m.linear_velocity));
                }
                to_driver(&driver_sock, &ToDriver::Tracking(sample));

                rate_window.1 += 1;
                let elapsed = rate_window.0.elapsed();
                if elapsed >= Duration::from_secs(1) {
                    if ctrl_stats.2 > 1.0 {
                        log::info!(
                            "controllers: {} of {} samples with angular velocity; peak {:.1} rad/s, {:.2} m/s",
                            ctrl_stats.1,
                            ctrl_stats.0,
                            ctrl_stats.2,
                            ctrl_stats.3
                        );
                    }
                    ctrl_stats = (0, 0, 0.0, 0.0);
                    if invalid_hmd > 0 {
                        log::warn!("headset sent {invalid_hmd} tracking samples without a valid head pose");
                        invalid_hmd = 0;
                    }
                    if let Some(h) = shared.status.lock().unwrap().headset.as_mut() {
                        h.tracking_hz = rate_window.1 as f32 / elapsed.as_secs_f32();
                        h.sample_age_ms = (t_rx - pc_time) as f32 * 1e-6;
                    }
                    rate_window = (Instant::now(), 0);
                }
            }
            HeadsetMessage::Battery {
                hmd,
                left,
                right,
                charging,
            } => {
                to_driver(
                    &driver_sock,
                    &ToDriver::Battery {
                        hmd,
                        left,
                        right,
                        charging,
                    },
                );
            }
            HeadsetMessage::FrameDisplayed { present_ns, received_ns, queued_ns, decoded_ns, display_ns } => {
                let Some(received_pc) = clock.headset_to_pc(received_ns) else {
                    continue;
                };
                let stages = [
                    received_pc - present_ns,
                    queued_ns - received_ns,
                    decoded_ns - queued_ns,
                    display_ns - decoded_ns,
                ];
                if let Some(seconds) = latency.push(stages) {
                    to_driver(&driver_sock, &ToDriver::PhotonLatency { seconds });
                    if let Some(h) = shared.status.lock().unwrap().headset.as_mut() {
                        h.photon_latency_ms = Some(seconds * 1e3);
                    }
                }
            }
            HeadsetMessage::DisplayPeriod { period_ns } => {
                to_driver(&driver_sock, &ToDriver::DisplayPeriod { period_ns });
            }
            HeadsetMessage::VideoLoss { frame } => {
                log::debug!("headset lost video at frame {frame}");
                shared.idr_requested.store(true, Ordering::Release);
            }
            HeadsetMessage::Disconnect => {
                log::info!("headset {from} disconnected");
                disconnect(shared, &driver_sock, &mut clock);
            }
        }
    }
}

/// Smooths per-frame present-to-photon latencies (median over a window) and decides when
/// SteamVR's prediction horizon needs updating.
#[derive(Default)]
struct LatencyMeter {
    /// Per frame: present -> received (encode + network), received -> queued into the
    /// decoder, queued -> decoded, decoded -> photons.
    window: Vec<[i64; 4]>,
    applied: Option<f32>,
    last_log: Option<Instant>,
}

impl LatencyMeter {
    const WINDOW: usize = 180;
    /// Changes smaller than this are not worth re-telling SteamVR.
    const HYSTERESIS_S: f32 = 0.002;

    /// Returns a new latency (seconds) to apply, when it moved enough.
    fn push(&mut self, stages: [i64; 4]) -> Option<f32> {
        let total: i64 = stages.iter().sum();
        if !(0..1_000_000_000).contains(&total) {
            return None; // clock not converged yet, or nonsense
        }
        self.window.push(stages);
        if self.window.len() < Self::WINDOW {
            return None;
        }
        let window = std::mem::take(&mut self.window);
        let median_ms = |f: &dyn Fn(&[i64; 4]) -> i64| {
            let mut v: Vec<i64> = window.iter().map(f).collect();
            v.sort_unstable();
            v[v.len() / 2] as f32 * 1e-6
        };
        let total_ms = median_ms(&|s| s.iter().sum());
        let median = total_ms * 1e-3;
        if self.last_log.is_none_or(|t| t.elapsed() >= Duration::from_secs(5)) {
            log::info!(
                "present-to-photon {total_ms:.1} ms (medians: encode+network {:.1}, wait for decoder {:.1}, decode {:.1}, render + display {:.1})",
                median_ms(&|s| s[0]),
                median_ms(&|s| s[1]),
                median_ms(&|s| s[2]),
                median_ms(&|s| s[3]),
            );
            self.last_log = Some(Instant::now());
        }
        if self.applied.is_some_and(|a| (a - median).abs() < Self::HYSTERESIS_S) {
            return None;
        }
        self.applied = Some(median);
        Some(median)
    }
}

fn disconnect(shared: &Shared, driver_sock: &UdpSocket, clock: &mut ClockSync) {
    *shared.headset_addr.lock().unwrap() = None;
    *shared.headset_info.lock().unwrap() = None;
    shared.status.lock().unwrap().headset = None;
    *clock = ClockSync::default();
    to_driver(driver_sock, &ToDriver::HeadsetDisconnected);
}

fn driver_loop(shared: &Shared, sock: UdpSocket, headset_sock: UdpSocket) {
    let mut buf = [0u8; 2048];
    let mut last_heartbeat: Option<Instant> = None;

    while !shared.stop.load(Ordering::Acquire) {
        let alive = last_heartbeat.is_some_and(|t| t.elapsed() < DRIVER_TIMEOUT);
        shared.status.lock().unwrap().driver_connected = alive;

        let msg = match sock.recv_from(&mut buf) {
            Ok((n, from)) if from.ip().is_loopback() => proto::decode::<FromDriver>(&buf[..n]),
            Ok(_) => continue,
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                continue
            }
            Err(e) => {
                log::warn!("driver socket: {e}");
                continue;
            }
        };
        match msg {
            Ok(FromDriver::Heartbeat) => {
                if !alive {
                    log::info!("SteamVR driver connected");
                    // The driver (re)started after the headset connected: replay the headset info.
                    if let Some(info) = shared.headset_info.lock().unwrap().clone() {
                        to_driver(&sock, &ToDriver::HeadsetConnected(info));
                    }
                }
                last_heartbeat = Some(Instant::now());
            }
            Ok(FromDriver::Haptics(h)) => {
                if let Some(addr) = *shared.headset_addr.lock().unwrap() {
                    let _ = headset_sock.send_to(&proto::encode(&ServerMessage::Haptics(h)), addr);
                }
            }
            Err(e) => log::debug!("bad driver datagram: {e}"),
        }
    }
}
