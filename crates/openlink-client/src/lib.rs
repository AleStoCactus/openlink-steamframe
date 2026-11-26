//! Platform-independent headset client.
//!
//! The Quest app supplies a [`TrackingSource`] backed by OpenXR; the `mock-headset` binary
//! supplies a synthetic one so the whole PC side can be exercised without a headset.
//!
//! Threading mirrors what the real app needs:
//! * the **tracking thread** (the caller of [`Client::run`]) samples at a fixed high rate and
//!   sends immediately;
//! * a **receive thread** answers clock pings the instant they arrive, so the `t1`/`t2`
//!   timestamps are not inflated by waiting for the next tracking tick. It also reassembles
//!   the video stream and hands complete frames to [`Client::video_frames`], and queues game
//!   audio for [`Client::speakers`].
//!
//! The microphone goes the other way through [`Client::microphone`], from whatever thread the
//! platform records on.

use openlink_protocol::{
    self as proto,
    audio::{self, JitterBuffer, Packetizer},
    time::monotonic_ns,
    video, HapticPulse, HeadsetInfo, HeadsetMessage,
    ServerMessage, StreamConfig, TrackingSample,
};
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub trait TrackingSource {
    fn info(&self) -> HeadsetInfo;
    /// Sample every device **at** `time_ns` (headset clock). No forward prediction.
    fn sample(&mut self, time_ns: i64) -> TrackingSample;
    fn haptics(&mut self, _pulse: HapticPulse) {}
}

#[derive(Debug, Clone, Default)]
pub struct ClientState {
    pub welcome: Option<StreamConfig>,
    pub video: video::ReassemblyStats,
    /// Complete frames dropped because the consumer of [`Client::video_frames`] fell behind.
    pub video_overflow: u64,
    /// Set by the consumer of [`Client::video_frames`] when its decoder cannot go on without
    /// an IDR (it restarted, say): the receive thread then drops frames until one arrives and
    /// asks the server for it, as after a loss. Without it nothing would ask, and on a clean
    /// link the picture would stay frozen until the next loss.
    pub idr_wanted: bool,
}

/// Frames waiting for the decoder. More than this and the oldest are of no use anyway.
const VIDEO_QUEUE: usize = 4;
/// Reserve the game audio buffer keeps against Wi-Fi jitter, on top of what the jitter needs.
const SPEAKER_MARGIN_MS: u32 = 10;

pub struct Client {
    sock: UdpSocket,
    server: SocketAddr,
    state: Arc<Mutex<ClientState>>,
    haptics: Receiver<HapticPulse>,
    video: Option<Receiver<video::Frame>>,
    speakers: Arc<Mutex<JitterBuffer>>,
}

impl Client {
    pub fn connect(server: SocketAddr) -> std::io::Result<Self> {
        let bind: SocketAddr = if server.is_ipv4() {
            "0.0.0.0:0"
        } else {
            "[::]:0"
        }
        .parse()
        .unwrap();
        let sock = UdpSocket::bind(bind)?;
        sock.connect(server)?;
        set_recv_buffer(&sock, 4 << 20);
        let state = Arc::new(Mutex::new(ClientState::default()));
        let (tx, haptics) = mpsc::channel();
        let (video_tx, video) = mpsc::sync_channel(VIDEO_QUEUE);
        let speakers = Arc::new(Mutex::new(JitterBuffer::new(audio::SPEAKER_CHANNELS, SPEAKER_MARGIN_MS)));
        let rx_speakers = speakers.clone();

        let rx_sock = sock.try_clone()?;
        let rx_state = state.clone();
        std::thread::Builder::new()
            .name("openlink-rx".into())
            .spawn(move || {
                let mut buf = [0u8; 2048];
                let mut reassembler = video::Reassembler::default();
                loop {
                    let n = match rx_sock.recv(&mut buf) {
                        Ok(n) => n,
                        // Nothing listening yet (ICMP port unreachable) or socket closed.
                        Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused => continue,
                        Err(_) => return,
                    };
                    if video::is_video(&buf[..n]) {
                        on_video(&rx_sock, &mut reassembler, &buf[..n], &video_tx, &rx_state);
                        continue;
                    }
                    if let Some(p) = audio::parse(&buf[..n]) {
                        if p.stream == audio::Stream::Speakers {
                            rx_speakers.lock().unwrap().push(p.seq, p.pcm);
                        }
                        continue;
                    }
                    let t1 = monotonic_ns();
                    match proto::decode::<ServerMessage>(&buf[..n]) {
                        Ok(ServerMessage::ClockPing { t0 }) => {
                            let pong = HeadsetMessage::ClockPong {
                                t0,
                                t1,
                                t2: monotonic_ns(),
                            };
                            let _ = rx_sock.send(&proto::encode(&pong));
                        }
                        Ok(ServerMessage::Welcome(cfg)) => {
                            let mut st = rx_state.lock().unwrap();
                            if st.welcome.is_none() {
                                log::info!("server accepted us: {cfg:?}");
                                // A new session: its video and audio start over.
                                reassembler.restart();
                                rx_speakers.lock().unwrap().reset();
                            }
                            st.welcome = Some(cfg);
                        }
                        Ok(ServerMessage::Haptics(p)) => {
                            let _ = tx.send(p);
                        }
                        Ok(ServerMessage::Disconnect) => {
                            log::info!("server closed the session");
                            rx_state.lock().unwrap().welcome = None;
                        }
                        Err(e) => log::debug!("bad datagram: {e}"),
                    }
                }
            })?;

        Ok(Self {
            sock,
            server,
            state,
            haptics,
            video: Some(video),
            speakers,
        })
    }

    /// Game audio from the PC ([`audio::SPEAKER_CHANNELS`] interleaved, [`audio::SAMPLE_RATE`]),
    /// for the audio output callback to [`JitterBuffer::pop`] from. Plays silence until the
    /// server sends some; only sent when [`StreamConfig::speakers`] is set.
    pub fn speakers(&self) -> Arc<Mutex<JitterBuffer>> {
        self.speakers.clone()
    }

    /// For the audio input callback: sends the microphone to the PC. Only wanted when
    /// [`StreamConfig::microphone`] is set.
    pub fn microphone(&self) -> std::io::Result<MicSender> {
        Ok(MicSender { sock: self.sock.try_clone()?, packetizer: Packetizer::new(audio::Stream::Microphone) })
    }

    /// Complete, decodable video frames (starting at an IDR), oldest first. Can be taken once.
    pub fn video_frames(&mut self) -> Option<Receiver<video::Frame>> {
        self.video.take()
    }

    /// For the render thread: reports when frames reach the display.
    pub fn frame_reporter(&self) -> std::io::Result<FrameReporter> {
        Ok(FrameReporter { sock: self.sock.try_clone()? })
    }

    /// Live session state (welcome, video statistics), shared with the receive thread.
    pub fn state(&self) -> Arc<Mutex<ClientState>> {
        self.state.clone()
    }

    pub fn server(&self) -> SocketAddr {
        self.server
    }

    /// Run until `stop` is set: say Hello until welcomed, then stream tracking at `rate_hz`.
    pub fn run(&mut self, source: &mut impl TrackingSource, rate_hz: f64, stop: &AtomicBool) {
        let period = Duration::from_secs_f64(1.0 / rate_hz);
        let mut next = Instant::now();
        let mut last_hello: Option<Instant> = None;
        let mut seq = 0u32;

        while !stop.load(Ordering::Acquire) {
            let welcomed = self.state.lock().unwrap().welcome.is_some();
            if !welcomed && last_hello.is_none_or(|t| t.elapsed() > Duration::from_secs(1)) {
                let _ = self
                    .sock
                    .send(&proto::encode(&HeadsetMessage::Hello(source.info())));
                last_hello = Some(Instant::now());
            }
            if welcomed {
                let sample = source.sample(monotonic_ns());
                let _ = self
                    .sock
                    .send(&proto::encode(&HeadsetMessage::Tracking { seq, sample }));
                seq = seq.wrapping_add(1);
            }
            while let Ok(p) = self.haptics.try_recv() {
                source.haptics(p);
            }

            next += period;
            let now = Instant::now();
            if next > now {
                std::thread::sleep(next - now);
            } else {
                next = now; // fell behind; do not try to catch up with a burst
            }
        }
        let _ = self.sock.send(&proto::encode(&HeadsetMessage::Disconnect));
    }
}

fn on_video(
    sock: &UdpSocket,
    reassembler: &mut video::Reassembler,
    datagram: &[u8],
    frames: &SyncSender<video::Frame>,
    state: &Mutex<ClientState>,
) {
    let result = reassembler.push(datagram);
    match result {
        Ok(frame) => match frames.try_send(frame) {
            Ok(()) | Err(TrySendError::Disconnected(_)) => {}
            Err(TrySendError::Full(_)) => {
                // The next frame would reference this one: recover with an IDR (the
                // reassembler asks for it with the next datagram).
                state.lock().unwrap().video_overflow += 1;
                reassembler.require_idr();
            }
        },
        Err(video::Event::Loss) => {
            let frame = reassembler.stats.lost_frames as u32;
            let _ = sock.send(&proto::encode(&HeadsetMessage::VideoLoss { frame }));
        }
        Err(video::Event::Pending) => {}
    }
    // Published on every datagram: drops while waiting for an IDR must be visible too.
    let mut st = state.lock().unwrap();
    st.video = reassembler.stats;
    if std::mem::take(&mut st.idr_wanted) {
        reassembler.require_idr();
    }
}

/// A frame arrives as a burst of a few hundred datagrams; the default buffer drops some.
fn set_recv_buffer(sock: &UdpSocket, bytes: i32) {
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        // SAFETY: valid socket and option value.
        let r = unsafe {
            libc::setsockopt(
                sock.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_RCVBUF,
                (&raw const bytes).cast(),
                std::mem::size_of::<i32>() as libc::socklen_t,
            )
        };
        if r != 0 {
            log::warn!("cannot enlarge the UDP receive buffer: {}", std::io::Error::last_os_error());
            return;
        }
        // Linux silently caps the request at net.core.rmem_max (and reports double the
        // usable size); a small buffer drops parts of large frames at high bitrates.
        let mut got: i32 = 0;
        let mut len = std::mem::size_of::<i32>() as libc::socklen_t;
        // SAFETY: valid socket, output buffer and length.
        let r = unsafe {
            libc::getsockopt(sock.as_raw_fd(), libc::SOL_SOCKET, libc::SO_RCVBUF, (&raw mut got).cast(), &mut len)
        };
        if r == 0 && got / 2 < bytes {
            log::warn!(
                "UDP receive buffer is {} KB, not the {} KB asked for: large frames may lose packets at high bitrates (raise net.core.rmem_max)",
                got / 2 / 1024,
                bytes / 1024
            );
        }
    }
    #[cfg(not(unix))]
    let _ = (sock, bytes);
}

/// The broadcast address of every IPv4 network this device is on (loopback excluded).
fn interface_broadcasts() -> Vec<std::net::Ipv4Addr> {
    let mut out = Vec::new();
    #[cfg(unix)]
    // SAFETY: getifaddrs/freeifaddrs pair; the list is only read in between.
    unsafe {
        let mut list: *mut libc::ifaddrs = std::ptr::null_mut();
        if libc::getifaddrs(&mut list) != 0 {
            return out;
        }
        let mut cur = list;
        while !cur.is_null() {
            let ifa = &*cur;
            cur = ifa.ifa_next;
            let flags = ifa.ifa_flags;
            if ifa.ifa_addr.is_null()
                || ifa.ifa_netmask.is_null()
                || (*ifa.ifa_addr).sa_family as i32 != libc::AF_INET
                || flags & libc::IFF_UP as u32 == 0
                || flags & libc::IFF_BROADCAST as u32 == 0
                || flags & libc::IFF_LOOPBACK as u32 != 0
            {
                continue;
            }
            let addr = u32::from_be((*(ifa.ifa_addr as *const libc::sockaddr_in)).sin_addr.s_addr);
            let mask = u32::from_be((*(ifa.ifa_netmask as *const libc::sockaddr_in)).sin_addr.s_addr);
            let b = std::net::Ipv4Addr::from(addr | !mask);
            if !out.contains(&b) {
                out.push(b);
            }
        }
        libc::freeifaddrs(list);
    }
    out
}

/// Finds a server on the LAN: broadcasts `Hello` until one answers with `Welcome`, for up to
/// `timeout`. The server then simply sees the real client's `Hello` as a reconnect.
pub fn discover(info: &HeadsetInfo, timeout: Duration) -> std::io::Result<Option<SocketAddr>> {
    let sock = UdpSocket::bind("0.0.0.0:0")?;
    sock.set_broadcast(true)?;
    sock.set_read_timeout(Some(Duration::from_millis(250)))?;
    let hello = proto::encode(&HeadsetMessage::Hello(info.clone()));
    let start = Instant::now();
    let mut buf = [0u8; 2048];
    while start.elapsed() < timeout {
        // 255.255.255.255 leaves by the default route only; each network's own broadcast
        // address also reaches a PC on another one (the Steam Frame's adapter joins the
        // headset's access point, beside the home network).
        let mut targets = vec![std::net::Ipv4Addr::BROADCAST];
        targets.extend(interface_broadcasts().into_iter().filter(|b| *b != std::net::Ipv4Addr::BROADCAST));
        for ip in targets {
            if let Err(e) = sock.send_to(&hello, SocketAddr::from((ip, proto::SERVER_PORT))) {
                log::debug!("discovery to {ip}: {e}");
            }
        }
        let deadline = Instant::now() + Duration::from_millis(500);
        while Instant::now() < deadline {
            match sock.recv_from(&mut buf) {
                Ok((n, from)) => {
                    if let Ok(ServerMessage::Welcome(_)) = proto::decode::<ServerMessage>(&buf[..n]) {
                        let _ = sock.send_to(&proto::encode(&HeadsetMessage::Disconnect), from);
                        return Ok(Some(from));
                    }
                }
                Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {}
                Err(e) => return Err(e),
            }
        }
    }
    Ok(None)
}

/// Tells the server when video frames actually reach the display, so it can measure the
/// present-to-photon latency SteamVR should predict across.
pub struct FrameReporter {
    sock: UdpSocket,
}

impl FrameReporter {
    /// Asks for SteamVR's vsync to move by `shift_ns` (+ = later).
    pub fn vsync_shift(&self, shift_ns: i64) {
        let _ = self.sock.send(&proto::encode(&HeadsetMessage::VsyncShift { shift_ns }));
    }

    /// The display's measured frame period, headset clock.
    pub fn display_period(&self, period_ns: i64) {
        let _ = self.sock.send(&proto::encode(&HeadsetMessage::DisplayPeriod { period_ns }));
    }

    /// `present_ns` from the frame's info; the rest on this device's monotonic clock.
    pub fn displayed(&self, present_ns: i64, received_ns: i64, queued_ns: i64, decoded_ns: i64, display_ns: i64) {
        let msg = HeadsetMessage::FrameDisplayed { present_ns, received_ns, queued_ns, decoded_ns, display_ns };
        let _ = self.sock.send(&proto::encode(&msg));
    }
}

/// Sends microphone audio ([`audio::MIC_CHANNELS`] interleaved, [`audio::SAMPLE_RATE`]).
pub struct MicSender {
    sock: UdpSocket,
    packetizer: Packetizer,
}

impl MicSender {
    pub fn send(&mut self, samples: impl IntoIterator<Item = i16>) {
        let sock = &self.sock;
        self.packetizer.push(samples, |d| {
            let _ = sock.send(d);
        });
    }
}
