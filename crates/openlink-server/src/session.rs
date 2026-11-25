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
}

struct Shared {
    status: Arc<Mutex<Status>>,
    settings: Arc<Mutex<Settings>>,
    headset_addr: Mutex<Option<SocketAddr>>,
    headset_info: Mutex<Option<HeadsetInfo>>,
    stop: AtomicBool,
}

pub struct Session {
    shared: Arc<Shared>,
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
        driver_sock.set_read_timeout(Some(Duration::from_millis(100)))?;

        let shared = Arc::new(Shared {
            status,
            settings,
            headset_addr: Mutex::new(None),
            headset_info: Mutex::new(None),
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
            threads: vec![headset_thread, driver_thread],
        })
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
    StreamConfig {
        codec,
        refresh_rate: settings.refresh_rate,
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
            }
            let cfg = stream_config(&shared.settings.lock().unwrap(), info);
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
            });
            last_rx = Instant::now();
            continue;
        }
        if current != Some(from) {
            continue; // not our headset (or it has not said Hello yet)
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
                to_driver(&driver_sock, &ToDriver::Tracking(sample));

                rate_window.1 += 1;
                let elapsed = rate_window.0.elapsed();
                if elapsed >= Duration::from_secs(1) {
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
            HeadsetMessage::Disconnect => {
                log::info!("headset {from} disconnected");
                disconnect(shared, &driver_sock, &mut clock);
            }
        }
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
