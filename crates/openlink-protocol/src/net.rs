//! Sockets that behave the same on every platform.

use std::io;
use std::net::{ToSocketAddrs, UdpSocket};

/// Binds a UDP socket. On Windows it also stops the socket from failing its next receive with
/// `WSAECONNRESET` after a datagram it sent was refused (an ICMP port unreachable): that is how
/// Windows reports sending to a peer that is not running yet, e.g. the server before SteamVR
/// starts, and on Linux an unconnected socket never sees it.
pub fn bind_udp(addr: impl ToSocketAddrs) -> io::Result<UdpSocket> {
    let sock = UdpSocket::bind(addr)?;
    #[cfg(windows)]
    ignore_connection_reset(&sock)?;
    Ok(sock)
}

#[cfg(windows)]
fn ignore_connection_reset(sock: &UdpSocket) -> io::Result<()> {
    use std::os::windows::io::AsRawSocket;
    use windows_sys::Win32::Networking::WinSock::{WSAGetLastError, WSAIoctl};
    /// `_WSAIOW(IOC_VENDOR, 12)`.
    const SIO_UDP_CONNRESET: u32 = 0x9800_000C;
    let off: u32 = 0;
    let mut returned = 0u32;
    // SAFETY: a live socket; the input buffer is a BOOL and there is no output buffer.
    let r = unsafe {
        WSAIoctl(
            sock.as_raw_socket() as usize,
            SIO_UDP_CONNRESET,
            (&raw const off).cast(),
            std::mem::size_of::<u32>() as u32,
            std::ptr::null_mut(),
            0,
            &mut returned,
            std::ptr::null_mut(),
            None,
        )
    };
    if r != 0 {
        // SAFETY: plain call.
        return Err(io::Error::from_raw_os_error(unsafe { WSAGetLastError() }));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;
    use std::time::Duration;

    #[test]
    fn a_refused_datagram_does_not_fail_the_next_receive() {
        let a = bind_udp((Ipv4Addr::LOCALHOST, 0)).unwrap();
        // A port nobody listens on (bound, then closed).
        let closed = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap().local_addr().unwrap();
        a.send_to(b"x", closed).unwrap();
        std::thread::sleep(Duration::from_millis(50));
        let b = bind_udp((Ipv4Addr::LOCALHOST, 0)).unwrap();
        b.send_to(b"y", a.local_addr().unwrap()).unwrap();
        a.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
        let mut buf = [0u8; 4];
        let (n, _) = a.recv_from(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"y");
    }
}
