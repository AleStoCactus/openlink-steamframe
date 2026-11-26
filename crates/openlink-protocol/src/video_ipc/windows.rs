//! Windows transport: a loopback TCP connection, each message prefixed with its length
//! (`u32` LE).
//!
//! It behaves like the Linux `SOCK_SEQPACKET` socket where the callers depend on it: whole
//! messages in order, [`Channel::try_send`] never blocks the compositor (messages go through a
//! bounded queue to a writer thread, and a full queue is `WouldBlock`), and a vanished peer is
//! `UnexpectedEof`. Nothing rides along: Windows handles travel inside the messages.

use super::{MAX_MESSAGE, VIDEO_PORT};
use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::os::windows::io::AsRawSocket;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{sync_channel, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Nothing rides along with a message on Windows.
pub type Attachment = std::convert::Infallible;
pub type RawAttachment = std::convert::Infallible;

/// Messages queued for the writer thread before `try_send` reports `WouldBlock`. Frames are
/// ~200 bytes, so this is far below what the socket buffers anyway.
const QUEUE: usize = 64;

/// Server side: the listening socket.
pub struct Listener {
    listener: TcpListener,
}

impl Listener {
    pub fn bind() -> io::Result<Self> {
        Self::bind_port(VIDEO_PORT)
    }

    fn bind_port(port: u16) -> io::Result<Self> {
        // Loopback only: the driver is on this machine, and nothing else should reach it.
        Ok(Self { listener: TcpListener::bind((Ipv4Addr::LOCALHOST, port))? })
    }

    /// Waits up to `timeout_ms` for the driver to connect.
    pub fn accept(&self, timeout_ms: i32) -> io::Result<Option<Channel>> {
        if !poll_readable(self.listener.as_raw_socket(), timeout_ms)? {
            return Ok(None);
        }
        let (stream, _) = self.listener.accept()?;
        Channel::new(stream).map(Some)
    }
}

/// `WSAPoll` for readability (data, a pending connection, or the peer closing).
fn poll_readable(socket: u64, timeout_ms: i32) -> io::Result<bool> {
    use windows_sys::Win32::Networking::WinSock::{WSAGetLastError, WSAPoll, POLLRDNORM, WSAPOLLFD};
    let mut p = WSAPOLLFD { fd: socket as usize, events: POLLRDNORM as _, revents: 0 };
    // SAFETY: one valid WSAPOLLFD for a live socket.
    let n = unsafe { WSAPoll(&mut p, 1, timeout_ms.max(0)) };
    match n {
        // SAFETY: plain call.
        n if n < 0 => Err(io::Error::from_raw_os_error(unsafe { WSAGetLastError() })),
        0 => Ok(false),
        _ => Ok(true),
    }
}

struct Reader {
    stream: TcpStream,
    /// Bytes received but not yet returned (a partial message, or messages after one).
    pending: Vec<u8>,
}

impl Reader {
    /// Moves the first complete message into `buf`.
    fn take(&mut self, buf: &mut [u8]) -> io::Result<Option<usize>> {
        let Some(len) = self.pending.first_chunk::<4>().map(|b| u32::from_le_bytes(*b) as usize) else {
            return Ok(None);
        };
        if len > buf.len().max(MAX_MESSAGE) {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "message too long"));
        }
        if self.pending.len() < 4 + len {
            return Ok(None);
        }
        if len > buf.len() {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "message truncated"));
        }
        buf[..len].copy_from_slice(&self.pending[4..4 + len]);
        self.pending.drain(..4 + len);
        Ok(Some(len))
    }
}

/// A connected link: messages in order, whole.
pub struct Channel {
    tx: Option<SyncSender<Vec<u8>>>,
    /// The writer failed; every later send fails too.
    broken: Arc<AtomicBool>,
    reader: Mutex<Reader>,
    stream: TcpStream,
    writer: Option<JoinHandle<()>>,
}

impl Channel {
    /// Driver side: connect to the server, if it is listening.
    pub fn connect() -> io::Result<Self> {
        Self::connect_port(VIDEO_PORT)
    }

    fn connect_port(port: u16) -> io::Result<Self> {
        let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
        Self::new(TcpStream::connect_timeout(&addr, Duration::from_millis(500))?)
    }

    fn new(stream: TcpStream) -> io::Result<Self> {
        // Frames are latency-critical and tiny: never wait to coalesce them.
        stream.set_nodelay(true)?;
        let (tx, rx) = sync_channel::<Vec<u8>>(QUEUE);
        let broken = Arc::new(AtomicBool::new(false));
        let writer = {
            let mut out = stream.try_clone()?;
            let broken = broken.clone();
            std::thread::Builder::new().name("openlink-video-tx".into()).spawn(move || {
                for msg in rx {
                    if out.write_all(&msg).is_err() {
                        broken.store(true, Ordering::Release);
                        let _ = out.shutdown(Shutdown::Both);
                        break;
                    }
                }
            })?
        };
        Ok(Self {
            tx: Some(tx),
            broken,
            reader: Mutex::new(Reader { stream: stream.try_clone()?, pending: Vec::new() }),
            stream,
            writer: Some(writer),
        })
    }

    fn frame(bytes: &[u8]) -> io::Result<Vec<u8>> {
        if bytes.len() > MAX_MESSAGE {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "message too long"));
        }
        let mut v = Vec::with_capacity(4 + bytes.len());
        v.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
        v.extend_from_slice(bytes);
        Ok(v)
    }

    fn check(&self) -> io::Result<&SyncSender<Vec<u8>>> {
        match &self.tx {
            Some(tx) if !self.broken.load(Ordering::Acquire) => Ok(tx),
            _ => Err(io::ErrorKind::BrokenPipe.into()),
        }
    }

    /// Sends one message. Blocks only if the queue to the peer is full.
    pub fn send(&self, bytes: &[u8], attachments: &[RawAttachment]) -> io::Result<()> {
        // Uninhabited: there are never any.
        let _ = attachments;
        self.check()?.send(Self::frame(bytes)?).map_err(|_| io::ErrorKind::BrokenPipe.into())
    }

    /// Like [`Self::send`] but fails with `WouldBlock` instead of waiting for a full queue.
    pub fn try_send(&self, bytes: &[u8]) -> io::Result<()> {
        match self.check()?.try_send(Self::frame(bytes)?) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => Err(io::ErrorKind::WouldBlock.into()),
            Err(TrySendError::Disconnected(_)) => Err(io::ErrorKind::BrokenPipe.into()),
        }
    }

    /// Receives one message into `buf`. `Ok(None)` on timeout; `Err(UnexpectedEof)` when the
    /// peer has gone.
    pub fn recv(&self, buf: &mut [u8], timeout_ms: i32) -> io::Result<Option<(usize, Vec<Attachment>)>> {
        let mut r = self.reader.lock().unwrap();
        let deadline = Instant::now() + Duration::from_millis(timeout_ms.max(0) as u64);
        let mut chunk = [0u8; 8192];
        loop {
            if let Some(n) = r.take(buf)? {
                return Ok(Some((n, Vec::new())));
            }
            let left = deadline.saturating_duration_since(Instant::now()).as_millis() as i32;
            if !poll_readable(r.stream.as_raw_socket(), left)? {
                return Ok(None);
            }
            let n = match r.stream.read(&mut chunk) {
                Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
                Ok(n) => n,
                // A reset peer is a gone peer, as on Linux.
                Err(e) if matches!(e.kind(), io::ErrorKind::ConnectionReset | io::ErrorKind::ConnectionAborted) => {
                    return Err(io::ErrorKind::UnexpectedEof.into())
                }
                Err(e) => return Err(e),
            };
            r.pending.extend_from_slice(&chunk[..n]);
        }
    }
}

impl Drop for Channel {
    fn drop(&mut self) {
        // Unblocks a writer stuck on a peer that stopped reading, then ends it.
        let _ = self.stream.shutdown(Shutdown::Both);
        self.tx.take();
        if let Some(t) = self.writer.take() {
            let _ = t.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::*;
    use super::*;

    #[test]
    fn passes_messages_in_order_and_reports_eof() {
        // An ephemeral port instead of VIDEO_PORT, so tests never clash with a running server.
        let listener = Listener::bind_port(0).unwrap();
        let port = listener.listener.local_addr().unwrap().port();
        let a = Channel::connect_port(port).unwrap();
        let b = listener.accept(1000).unwrap().expect("connection");

        let mut buf = [0u8; MAX_MESSAGE];
        assert!(b.recv(&mut buf, 0).unwrap().is_none(), "nothing sent yet");

        let frame = |id| DriverVideoMsg::Frame {
            frame_id: id,
            eyes: [EyeLayer { texture: 1, bounds: [0.0, 0.0, 1.0, 1.0] }; 2],
            overlay: None,
            hmd_pose: [[0.0; 4]; 3],
            present_ns: 99,
        };
        let set = DriverVideoMsg::TextureSet(TextureSet {
            render_node: 42,
            width: 2,
            height: 2,
            drm_format: 0,
            modifier: DRM_FORMAT_MOD_INVALID,
            vk_format: 28,
            textures: vec![DmabufTexture { handle: 0xC000_0042, planes: vec![] }],
        });
        a.send(&crate::encode(&set), &[]).unwrap();
        for id in 0..10 {
            a.try_send(&crate::encode(&frame(id))).unwrap();
        }

        let (n, att) = b.recv(&mut buf, 1000).unwrap().unwrap();
        assert!(att.is_empty());
        assert_eq!(crate::decode::<DriverVideoMsg>(&buf[..n]).unwrap(), set);
        for id in 0..10 {
            let (n, _) = b.recv(&mut buf, 1000).unwrap().unwrap();
            assert_eq!(crate::decode::<DriverVideoMsg>(&buf[..n]).unwrap(), frame(id));
        }
        // And back.
        let release = ServerVideoMsg::Release { frame_id: 9 };
        b.send(&crate::encode(&release), &[]).unwrap();
        let (n, _) = a.recv(&mut buf, 1000).unwrap().unwrap();
        assert_eq!(crate::decode::<ServerVideoMsg>(&buf[..n]).unwrap(), release);

        drop(a);
        assert_eq!(b.recv(&mut buf, 1000).err().map(|e| e.kind()), Some(io::ErrorKind::UnexpectedEof));
    }
}
