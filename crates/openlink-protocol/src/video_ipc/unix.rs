//! Linux transport: a `SOCK_SEQPACKET` Unix socket in the abstract namespace, fds as
//! `SCM_RIGHTS`.

use super::{MAX_FDS, SOCKET_NAME};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

/// What rides along with a message: a file descriptor.
pub type Attachment = OwnedFd;
pub type RawAttachment = RawFd;

/// A connected `SOCK_SEQPACKET` socket.
pub struct Channel {
    fd: OwnedFd,
}

fn abstract_addr() -> (libc::sockaddr_un, libc::socklen_t) {
    // SAFETY: sockaddr_un is plain data; all-zero is valid.
    let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
    // sun_path[0] stays 0: abstract namespace.
    for (dst, &src) in addr.sun_path[1..].iter_mut().zip(SOCKET_NAME) {
        *dst = src as libc::c_char;
    }
    let len = std::mem::size_of::<libc::sa_family_t>() + 1 + SOCKET_NAME.len();
    (addr, len as libc::socklen_t)
}

fn seqpacket() -> io::Result<OwnedFd> {
    // SAFETY: plain syscall; the fd is owned on success.
    let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: fresh fd we own.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn cvt(r: libc::c_int) -> io::Result<libc::c_int> {
    if r < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(r)
    }
}

/// Server side: the listening socket.
pub struct Listener {
    fd: OwnedFd,
}

impl Listener {
    pub fn bind() -> io::Result<Self> {
        let fd = seqpacket()?;
        let (addr, len) = abstract_addr();
        // SAFETY: valid socket and address.
        unsafe {
            cvt(libc::bind(fd.as_raw_fd(), (&raw const addr).cast(), len))?;
            cvt(libc::listen(fd.as_raw_fd(), 1))?;
        }
        Ok(Self { fd })
    }

    /// Waits up to `timeout_ms` for the driver to connect.
    pub fn accept(&self, timeout_ms: i32) -> io::Result<Option<Channel>> {
        if !poll_readable(self.fd.as_raw_fd(), timeout_ms)? {
            return Ok(None);
        }
        // SAFETY: valid listening socket; null address is allowed.
        let fd = unsafe {
            cvt(libc::accept4(
                self.fd.as_raw_fd(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                libc::SOCK_CLOEXEC,
            ))?
        };
        // SAFETY: fresh fd we own.
        Ok(Some(Channel { fd: unsafe { OwnedFd::from_raw_fd(fd) } }))
    }
}

fn poll_readable(fd: RawFd, timeout_ms: i32) -> io::Result<bool> {
    let mut p = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
    // SAFETY: one valid pollfd.
    let n = unsafe { libc::poll(&mut p, 1, timeout_ms) };
    match n {
        n if n < 0 => {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                Ok(false)
            } else {
                Err(e)
            }
        }
        0 => Ok(false),
        _ => Ok(true),
    }
}

impl Channel {
    /// Driver side: connect to the server, if it is listening.
    pub fn connect() -> io::Result<Self> {
        let fd = seqpacket()?;
        let (addr, len) = abstract_addr();
        // SAFETY: valid socket and address.
        unsafe { cvt(libc::connect(fd.as_raw_fd(), (&raw const addr).cast(), len))? };
        Ok(Self { fd })
    }

    /// Sends one packet with `fds` attached. Blocks only if the peer's queue is full.
    pub fn send(&self, bytes: &[u8], fds: &[RawFd]) -> io::Result<()> {
        self.send_flags(bytes, fds, 0)
    }

    /// Like [`Self::send`] but fails with `WouldBlock` instead of waiting for a full queue.
    pub fn try_send(&self, bytes: &[u8]) -> io::Result<()> {
        self.send_flags(bytes, &[], libc::MSG_DONTWAIT)
    }

    fn send_flags(&self, bytes: &[u8], fds: &[RawFd], flags: libc::c_int) -> io::Result<()> {
        assert!(fds.len() <= MAX_FDS);
        let mut iov = libc::iovec { iov_base: bytes.as_ptr() as *mut _, iov_len: bytes.len() };
        let mut cmsg_buf = [0u64; 16]; // >= CMSG_SPACE(4 * MAX_FDS), u64 for alignment
        // SAFETY: msghdr is plain data.
        let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        if !fds.is_empty() {
            let data_len = std::mem::size_of_val(fds) as u32;
            // SAFETY: the buffer is large enough for one cmsg with MAX_FDS fds.
            unsafe {
                msg.msg_control = cmsg_buf.as_mut_ptr().cast();
                msg.msg_controllen = libc::CMSG_SPACE(data_len) as _;
                let c = libc::CMSG_FIRSTHDR(&msg);
                (*c).cmsg_level = libc::SOL_SOCKET;
                (*c).cmsg_type = libc::SCM_RIGHTS;
                (*c).cmsg_len = libc::CMSG_LEN(data_len) as _;
                std::ptr::copy_nonoverlapping(fds.as_ptr(), libc::CMSG_DATA(c).cast(), fds.len());
            }
        }
        // SAFETY: valid socket and msghdr.
        let n = unsafe { libc::sendmsg(self.fd.as_raw_fd(), &msg, libc::MSG_NOSIGNAL | flags) };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Receives one packet into `buf`. `Ok(None)` on timeout; `Err(UnexpectedEof)` when the peer
    /// has gone.
    pub fn recv(&self, buf: &mut [u8], timeout_ms: i32) -> io::Result<Option<(usize, Vec<OwnedFd>)>> {
        if !poll_readable(self.fd.as_raw_fd(), timeout_ms)? {
            return Ok(None);
        }
        let mut iov = libc::iovec { iov_base: buf.as_mut_ptr().cast(), iov_len: buf.len() };
        let mut cmsg_buf = [0u64; 16];
        // SAFETY: msghdr is plain data.
        let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = cmsg_buf.as_mut_ptr().cast();
        msg.msg_controllen = std::mem::size_of_val(&cmsg_buf) as _;
        // SAFETY: valid socket and msghdr pointing at live buffers.
        let n = unsafe { libc::recvmsg(self.fd.as_raw_fd(), &mut msg, libc::MSG_CMSG_CLOEXEC) };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        let mut fds = Vec::new();
        // SAFETY: walking the control buffer the kernel filled in.
        unsafe {
            let mut c = libc::CMSG_FIRSTHDR(&msg);
            while !c.is_null() {
                if (*c).cmsg_level == libc::SOL_SOCKET && (*c).cmsg_type == libc::SCM_RIGHTS {
                    let count = ((*c).cmsg_len as usize - libc::CMSG_LEN(0) as usize) / 4;
                    let data = libc::CMSG_DATA(c).cast::<RawFd>();
                    for i in 0..count {
                        fds.push(OwnedFd::from_raw_fd(data.add(i).read_unaligned()));
                    }
                }
                c = libc::CMSG_NXTHDR(&msg, c);
            }
        }
        if n == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        if msg.msg_flags & libc::MSG_TRUNC != 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "message truncated"));
        }
        Ok(Some((n as usize, fds)))
    }
}

#[cfg(test)]
mod tests {
    use super::super::*;
    use super::*;
    use std::io::{Read, Seek, Write};

    #[test]
    fn passes_messages_and_fds() {
        // A private socketpair instead of the abstract name, so tests never clash with a
        // running server.
        let mut sv = [0; 2];
        // SAFETY: valid out array.
        unsafe { cvt(libc::socketpair(libc::AF_UNIX, libc::SOCK_SEQPACKET, 0, sv.as_mut_ptr())).unwrap() };
        // SAFETY: fresh fds we own.
        let (a, b) = unsafe {
            (
                Channel { fd: OwnedFd::from_raw_fd(sv[0]) },
                Channel { fd: OwnedFd::from_raw_fd(sv[1]) },
            )
        };

        let mut file = tempfile();
        file.write_all(b"dmabuf stand-in").unwrap();
        let msg = DriverVideoMsg::Frame {
            frame_id: 7,
            eyes: [EyeLayer { texture: 1, bounds: [0.0, 0.0, 1.0, 1.0] }; 2],
            overlay: None,
            hmd_pose: [[0.0; 4]; 3],
            present_ns: 99,
        };
        a.send(&crate::encode(&msg), &[file.as_raw_fd(), file.as_raw_fd()]).unwrap();

        let mut buf = [0u8; MAX_MESSAGE];
        let (n, fds) = b.recv(&mut buf, 1000).unwrap().unwrap();
        assert_eq!(crate::decode::<DriverVideoMsg>(&buf[..n]).unwrap(), msg);
        assert_eq!(fds.len(), 2);
        let mut got = std::fs::File::from(fds.into_iter().next().unwrap());
        got.rewind().unwrap();
        let mut s = String::new();
        got.read_to_string(&mut s).unwrap();
        assert_eq!(s, "dmabuf stand-in");

        drop(a);
        assert_eq!(b.recv(&mut buf, 1000).err().map(|e| e.kind()), Some(io::ErrorKind::UnexpectedEof));
    }

    fn tempfile() -> std::fs::File {
        // SAFETY: memfd_create with a valid name.
        let fd = unsafe { libc::memfd_create(c"openlink-test".as_ptr(), 0) };
        assert!(fd >= 0);
        // SAFETY: fresh fd we own.
        unsafe { std::fs::File::from_raw_fd(fd) }
    }
}
