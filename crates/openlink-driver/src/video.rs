//! Video link to the server (Linux): dmabuf fds and per-frame metadata out, releases back.
//! See [`openlink_protocol::video_ipc`].
//!
//! The server listens; this thread connects whenever it is running, announces every current
//! swap texture set, then relays the server's releases to [`crate::direct_mode`].

use crate::direct_mode;
use openlink_protocol::video_ipc::{self as vipc, Channel, DriverVideoMsg, ServerVideoMsg};
use openlink_protocol::{self as proto};
use std::io;
use std::os::fd::RawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const RECONNECT_EVERY: Duration = Duration::from_secs(1);

static CHANNEL: Mutex<Option<Arc<Channel>>> = Mutex::new(None);

fn channel() -> Option<Arc<Channel>> {
    CHANNEL.lock().unwrap().clone()
}

fn drop_channel(why: &io::Error) {
    if CHANNEL.lock().unwrap().take().is_some() {
        log::info!("video link to server closed: {why}");
        direct_mode::release_all();
    }
}

/// Announces a new swap texture set, if the server is connected.
pub fn send_set(set: &vipc::TextureSet, fds: &[RawFd]) {
    if let Some(ch) = channel() {
        if let Err(e) = ch.send(&proto::encode(&DriverVideoMsg::TextureSet(set.clone())), fds) {
            drop_channel(&e);
        }
    }
}

pub fn send_destroyed(handles: Vec<u64>) {
    if let Some(ch) = channel() {
        if let Err(e) = ch.send(&proto::encode(&DriverVideoMsg::TexturesDestroyed { handles }), &[]) {
            drop_channel(&e);
        }
    }
}

/// Sends a frame without blocking the compositor. `false` if it was not sent (no server, or
/// the server is behind): the frame is then simply not streamed.
pub fn send_frame(msg: &DriverVideoMsg) -> bool {
    let Some(ch) = channel() else { return false };
    match ch.try_send(&proto::encode(msg)) {
        Ok(()) => true,
        Err(e) if e.kind() == io::ErrorKind::WouldBlock => false,
        Err(e) => {
            drop_channel(&e);
            false
        }
    }
}

pub struct VideoLink {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl VideoLink {
    pub fn start() -> io::Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let stop = stop.clone();
            std::thread::Builder::new()
                .name("openlink-video".into())
                .spawn(move || run(&stop))?
        };
        Ok(Self { stop, thread: Some(thread) })
    }
}

impl Drop for VideoLink {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        CHANNEL.lock().unwrap().take();
    }
}

fn connect() -> io::Result<Arc<Channel>> {
    let ch = Arc::new(Channel::connect()?);
    let hello = DriverVideoMsg::Hello { driver_version: env!("CARGO_PKG_VERSION").into() };
    ch.send(&proto::encode(&hello), &[])?;
    let result = std::cell::RefCell::new(Ok(()));
    let publish = ch.clone();
    direct_mode::announce_all(
        &mut |set, fds| {
            let mut r = result.borrow_mut();
            if r.is_ok() {
                *r = ch.send(&proto::encode(&DriverVideoMsg::TextureSet(set.clone())), fds);
            }
        },
        || {
            if result.borrow().is_ok() {
                *CHANNEL.lock().unwrap() = Some(publish);
            }
        },
    );
    result.into_inner().map(|()| ch)
}

fn run(stop: &AtomicBool) {
    let mut buf = vec![0u8; vipc::MAX_MESSAGE];
    let mut last_attempt: Option<Instant> = None;
    let mut logged_absent = false;
    while !stop.load(Ordering::Acquire) {
        let Some(ch) = channel() else {
            if last_attempt.is_some_and(|t| t.elapsed() < RECONNECT_EVERY) {
                std::thread::sleep(Duration::from_millis(100));
                continue;
            }
            last_attempt = Some(Instant::now());
            match connect() {
                Ok(_) => {
                    log::info!("video link to server connected");
                    logged_absent = false;
                }
                Err(e) => {
                    if !logged_absent {
                        log::info!("video link: server not listening yet ({e})");
                        logged_absent = true;
                    }
                }
            }
            continue;
        };
        match ch.recv(&mut buf, 100) {
            Ok(None) => {}
            Ok(Some((n, _fds))) => match proto::decode::<ServerVideoMsg>(&buf[..n]) {
                Ok(ServerVideoMsg::Release { frame_id }) => direct_mode::release(frame_id),
                Err(e) => log::warn!("bad video message from server: {e}"),
            },
            Err(e) => drop_channel(&e),
        }
    }
}
