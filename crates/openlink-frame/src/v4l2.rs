//! The slice of the V4L2 UAPI (`linux/videodev2.h`) a stateful memory-to-memory decoder needs.
//!
//! Written by hand rather than generated: the structs are small, and the layout tests below pin
//! them to the kernel's (sizes and offsets read from the C headers; arm64 and x86-64 are both
//! LP64 and lay these out identically).

#![allow(dead_code)]

use std::ffi::c_void;
use std::io;
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd};

pub const fn fourcc(s: &[u8; 4]) -> u32 {
    (s[0] as u32) | (s[1] as u32) << 8 | (s[2] as u32) << 16 | (s[3] as u32) << 24
}

pub const PIX_FMT_H264: u32 = fourcc(b"H264");
pub const PIX_FMT_HEVC: u32 = fourcc(b"HEVC");
pub const PIX_FMT_AV1_FRAME: u32 = fourcc(b"AV1F");
/// Y plane, then interleaved CbCr, in one memory plane.
pub const PIX_FMT_NV12: u32 = fourcc(b"NV12");
/// The same planes in two memory planes.
pub const PIX_FMT_NV12M: u32 = fourcc(b"NM12");

pub const BUF_TYPE_VIDEO_CAPTURE: u32 = 1;
pub const BUF_TYPE_VIDEO_CAPTURE_MPLANE: u32 = 9;
pub const BUF_TYPE_VIDEO_OUTPUT_MPLANE: u32 = 10;
pub const MEMORY_MMAP: u32 = 1;
pub const FIELD_NONE: u32 = 1;

pub const CAP_VIDEO_M2M_MPLANE: u32 = 0x0000_4000;
pub const CAP_STREAMING: u32 = 0x0400_0000;
pub const CAP_DEVICE_CAPS: u32 = 0x8000_0000;

pub const BUF_FLAG_ERROR: u32 = 0x0000_0040;
pub const BUF_FLAG_LAST: u32 = 0x0010_0000;

pub const EVENT_EOS: u32 = 2;
pub const EVENT_SOURCE_CHANGE: u32 = 5;
pub const EVENT_SRC_CH_RESOLUTION: u32 = 1;

pub const SEL_TGT_COMPOSE: u32 = 0x0100;

pub const CID_MIN_BUFFERS_FOR_CAPTURE: u32 = 0x0098_0927;
pub const CID_MPEG_VIDEO_DEC_DISPLAY_DELAY: u32 = 0x0099_0b8d;
pub const CID_MPEG_VIDEO_DEC_DISPLAY_DELAY_ENABLE: u32 = 0x0099_0b8e;

pub const VIDEO_MAX_PLANES: usize = 8;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct Capability {
    pub driver: [u8; 16],
    pub card: [u8; 32],
    pub bus_info: [u8; 32],
    pub version: u32,
    pub capabilities: u32,
    pub device_caps: u32,
    pub reserved: [u32; 3],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct FmtDesc {
    pub index: u32,
    pub ty: u32,
    pub flags: u32,
    pub description: [u8; 32],
    pub pixelformat: u32,
    pub mbus_code: u32,
    pub reserved: [u32; 3],
}

#[repr(C, packed)]
#[derive(Clone, Copy)]
pub struct PlanePixFormat {
    pub sizeimage: u32,
    pub bytesperline: u32,
    pub reserved: [u16; 6],
}

#[repr(C, packed)]
#[derive(Clone, Copy)]
pub struct PixFormatMplane {
    pub width: u32,
    pub height: u32,
    pub pixelformat: u32,
    pub field: u32,
    pub colorspace: u32,
    pub plane_fmt: [PlanePixFormat; VIDEO_MAX_PLANES],
    pub num_planes: u8,
    pub flags: u8,
    pub ycbcr_enc: u8,
    pub quantization: u8,
    pub xfer_func: u8,
    pub reserved: [u8; 7],
}

/// `struct v4l2_format` with its union as the multi-planar member (the only one used here),
/// padded to the union's 200 bytes.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Format {
    pub ty: u32,
    // The union holds pointers, so it starts 8-byte aligned.
    _pad0: u32,
    pub pix_mp: PixFormatMplane,
    _pad: [u8; 200 - std::mem::size_of::<PixFormatMplane>()],
    _align: [u64; 0],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct RequestBuffers {
    pub count: u32,
    pub ty: u32,
    pub memory: u32,
    pub capabilities: u32,
    pub flags: u8,
    pub reserved: [u8; 3],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct Plane {
    pub bytesused: u32,
    pub length: u32,
    /// Union of `mem_offset` (u32), `userptr` (unsigned long) and `fd` (i32).
    pub m: u64,
    pub data_offset: u32,
    pub reserved: [u32; 11],
}

impl Plane {
    pub fn mem_offset(&self) -> u32 {
        // The u32 members sit at the start of the union (little-endian on both targets).
        self.m as u32
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct Timecode {
    pub ty: u32,
    pub flags: u32,
    pub frames: u8,
    pub seconds: u8,
    pub minutes: u8,
    pub hours: u8,
    pub userbits: [u8; 4],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct Buffer {
    pub index: u32,
    pub ty: u32,
    pub bytesused: u32,
    pub flags: u32,
    pub field: u32,
    pub timestamp: libc::timeval,
    pub timecode: Timecode,
    pub sequence: u32,
    pub memory: u32,
    /// Union; for multi-planar buffers a pointer to `length` [`Plane`]s.
    pub m: *mut Plane,
    pub length: u32,
    pub reserved2: u32,
    pub request_fd: i32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct ExportBuffer {
    pub ty: u32,
    pub index: u32,
    pub plane: u32,
    pub flags: u32,
    pub fd: i32,
    pub reserved: [u32; 11],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct Control {
    pub id: u32,
    pub value: i32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rect {
    pub left: i32,
    pub top: i32,
    pub width: u32,
    pub height: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct Selection {
    pub ty: u32,
    pub target: u32,
    pub flags: u32,
    pub r: Rect,
    pub reserved: [u32; 9],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct EventSubscription {
    pub ty: u32,
    pub id: u32,
    pub flags: u32,
    pub reserved: [u32; 5],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct Event {
    pub ty: u32,
    /// Union; for `V4L2_EVENT_SOURCE_CHANGE` its first u32 is `changes`.
    pub u: [u64; 8],
    pub pending: u32,
    pub sequence: u32,
    pub timestamp: libc::timespec,
    pub id: u32,
    pub reserved: [u32; 8],
}

macro_rules! zeroed {
    ($t:ty) => {
        impl $t {
            pub fn zeroed() -> Self {
                // SAFETY: plain C data (integers, arrays and a raw pointer); all-zero is valid.
                unsafe { std::mem::zeroed() }
            }
        }
    };
}
zeroed!(Capability);
zeroed!(FmtDesc);
zeroed!(Format);
zeroed!(RequestBuffers);
zeroed!(Plane);
zeroed!(Buffer);
zeroed!(ExportBuffer);
zeroed!(Selection);
zeroed!(EventSubscription);
zeroed!(Event);

// `_IOC(dir, 'V', nr, size)`; the generic encoding, which arm64 and x86-64 share.
const fn ioc(dir: u32, nr: u32, size: usize) -> u32 {
    (dir << 30) | ((size as u32) << 16) | ((b'V' as u32) << 8) | nr
}
const W: u32 = 1;
const R: u32 = 2;
const RW: u32 = 3;
const fn sz<T>() -> usize {
    std::mem::size_of::<T>()
}

pub const VIDIOC_QUERYCAP: u32 = ioc(R, 0, sz::<Capability>());
pub const VIDIOC_ENUM_FMT: u32 = ioc(RW, 2, sz::<FmtDesc>());
pub const VIDIOC_G_FMT: u32 = ioc(RW, 4, sz::<Format>());
pub const VIDIOC_S_FMT: u32 = ioc(RW, 5, sz::<Format>());
pub const VIDIOC_REQBUFS: u32 = ioc(RW, 8, sz::<RequestBuffers>());
pub const VIDIOC_QUERYBUF: u32 = ioc(RW, 9, sz::<Buffer>());
pub const VIDIOC_QBUF: u32 = ioc(RW, 15, sz::<Buffer>());
pub const VIDIOC_EXPBUF: u32 = ioc(RW, 16, sz::<ExportBuffer>());
pub const VIDIOC_DQBUF: u32 = ioc(RW, 17, sz::<Buffer>());
pub const VIDIOC_STREAMON: u32 = ioc(W, 18, sz::<i32>());
pub const VIDIOC_STREAMOFF: u32 = ioc(W, 19, sz::<i32>());
pub const VIDIOC_G_CTRL: u32 = ioc(RW, 27, sz::<Control>());
pub const VIDIOC_S_CTRL: u32 = ioc(RW, 28, sz::<Control>());
pub const VIDIOC_DQEVENT: u32 = ioc(R, 89, sz::<Event>());
pub const VIDIOC_SUBSCRIBE_EVENT: u32 = ioc(W, 90, sz::<EventSubscription>());
pub const VIDIOC_G_SELECTION: u32 = ioc(RW, 94, sz::<Selection>());

/// One ioctl, retried on `EINTR`.
///
/// # Safety
/// `arg` must point to the struct `request` expects, valid for reads and writes.
pub unsafe fn ioctl<T>(fd: BorrowedFd, request: u32, arg: &mut T) -> io::Result<()> {
    loop {
        // SAFETY: per the caller. The request parameter's C type differs between libcs.
        let r = unsafe { libc::ioctl(fd.as_raw_fd(), request as _, arg as *mut T as *mut c_void) };
        if r >= 0 {
            return Ok(());
        }
        let err = io::Error::last_os_error();
        if err.raw_os_error() != Some(libc::EINTR) {
            return Err(err);
        }
    }
}

pub fn open(path: &std::path::Path) -> io::Result<OwnedFd> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes())?;
    // SAFETY: a valid C string.
    let fd = unsafe { libc::open(c.as_ptr(), libc::O_RDWR | libc::O_NONBLOCK | libc::O_CLOEXEC) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a descriptor we just opened and own.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

pub fn query_cap(fd: BorrowedFd) -> io::Result<Capability> {
    let mut cap = Capability::zeroed();
    // SAFETY: matching struct.
    unsafe { ioctl(fd, VIDIOC_QUERYCAP, &mut cap)? };
    Ok(cap)
}

impl Capability {
    /// The capabilities of this device node (rather than of the whole driver).
    pub fn caps(&self) -> u32 {
        if self.capabilities & CAP_DEVICE_CAPS != 0 {
            self.device_caps
        } else {
            self.capabilities
        }
    }

    pub fn driver(&self) -> String {
        c_str(&self.driver)
    }

    pub fn card(&self) -> String {
        c_str(&self.card)
    }
}

pub fn c_str(b: &[u8]) -> String {
    let n = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    String::from_utf8_lossy(&b[..n]).into_owned()
}

/// Pixel formats a queue accepts.
pub fn formats(fd: BorrowedFd, ty: u32) -> Vec<u32> {
    let mut out = Vec::new();
    for index in 0..64 {
        let mut d = FmtDesc::zeroed();
        d.index = index;
        d.ty = ty;
        // SAFETY: matching struct.
        if unsafe { ioctl(fd, VIDIOC_ENUM_FMT, &mut d) }.is_err() {
            break;
        }
        out.push(d.pixelformat);
    }
    out
}

pub fn fourcc_str(f: u32) -> String {
    f.to_le_bytes().iter().map(|&b| if b.is_ascii_graphic() { b as char } else { '?' }).collect()
}

/// A buffer's memory plane mapped into this process. Unmapped on drop; the kernel keeps the
/// memory alive while it is mapped, even after the device is closed.
pub struct Mapping {
    pub ptr: *mut u8,
    pub len: usize,
}

// SAFETY: plain memory; who may touch it when is arranged by the buffer queue protocol.
unsafe impl Send for Mapping {}
unsafe impl Sync for Mapping {}

impl Mapping {
    pub fn new(fd: BorrowedFd, offset: u32, len: u32) -> io::Result<Self> {
        // SAFETY: mapping a buffer the device exported at this offset.
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len as usize,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd.as_raw_fd(),
                offset as libc::off_t,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { ptr: ptr.cast(), len: len as usize })
    }

    /// # Safety
    /// The device must not be writing this buffer (it is dequeued).
    pub unsafe fn bytes(&self) -> &[u8] {
        // SAFETY: per the caller; the mapping is `len` bytes long.
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        // SAFETY: our own mapping.
        unsafe { libc::munmap(self.ptr.cast(), self.len) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::{offset_of, size_of};

    /// Sizes and offsets from `linux/videodev2.h`, printed by a C program with `sizeof` and
    /// `offsetof`; ioctl numbers likewise.
    #[test]
    fn layouts_match_the_kernel() {
        assert_eq!(size_of::<Capability>(), 104);
        assert_eq!(size_of::<FmtDesc>(), 64);
        assert_eq!(size_of::<Format>(), 208);
        assert_eq!(offset_of!(Format, pix_mp), 8);
        assert_eq!(size_of::<PixFormatMplane>(), 192);
        assert_eq!(offset_of!(PixFormatMplane, plane_fmt), 20);
        assert_eq!(offset_of!(PixFormatMplane, num_planes), 180);
        assert_eq!(size_of::<PlanePixFormat>(), 20);
        assert_eq!(size_of::<RequestBuffers>(), 20);
        assert_eq!(size_of::<Plane>(), 64);
        assert_eq!(offset_of!(Plane, m), 8);
        assert_eq!(offset_of!(Plane, data_offset), 16);
        assert_eq!(size_of::<Buffer>(), 88);
        assert_eq!(offset_of!(Buffer, timestamp), 24);
        assert_eq!(offset_of!(Buffer, timecode), 40);
        assert_eq!(offset_of!(Buffer, sequence), 56);
        assert_eq!(offset_of!(Buffer, memory), 60);
        assert_eq!(offset_of!(Buffer, m), 64);
        assert_eq!(offset_of!(Buffer, length), 72);
        assert_eq!(offset_of!(Buffer, request_fd), 80);
        assert_eq!(size_of::<ExportBuffer>(), 64);
        assert_eq!(offset_of!(ExportBuffer, fd), 16);
        assert_eq!(size_of::<Control>(), 8);
        assert_eq!(size_of::<Selection>(), 64);
        assert_eq!(offset_of!(Selection, r), 12);
        assert_eq!(size_of::<EventSubscription>(), 32);
        assert_eq!(size_of::<Event>(), 136);
        assert_eq!(offset_of!(Event, u), 8);
        assert_eq!(offset_of!(Event, pending), 72);
        assert_eq!(offset_of!(Event, timestamp), 80);
        assert_eq!(offset_of!(Event, id), 96);
    }

    #[test]
    fn ioctl_numbers_match_the_kernel() {
        assert_eq!(VIDIOC_QUERYCAP, 0x8068_5600);
        assert_eq!(VIDIOC_ENUM_FMT, 0xc040_5602);
        assert_eq!(VIDIOC_G_FMT, 0xc0d0_5604);
        assert_eq!(VIDIOC_S_FMT, 0xc0d0_5605);
        assert_eq!(VIDIOC_REQBUFS, 0xc014_5608);
        assert_eq!(VIDIOC_QUERYBUF, 0xc058_5609);
        assert_eq!(VIDIOC_QBUF, 0xc058_560f);
        assert_eq!(VIDIOC_EXPBUF, 0xc040_5610);
        assert_eq!(VIDIOC_DQBUF, 0xc058_5611);
        assert_eq!(VIDIOC_STREAMON, 0x4004_5612);
        assert_eq!(VIDIOC_STREAMOFF, 0x4004_5613);
        assert_eq!(VIDIOC_G_CTRL, 0xc008_561b);
        assert_eq!(VIDIOC_S_CTRL, 0xc008_561c);
        assert_eq!(VIDIOC_G_SELECTION, 0xc040_565e);
        assert_eq!(VIDIOC_DQEVENT, 0x8088_5659);
        assert_eq!(VIDIOC_SUBSCRIBE_EVENT, 0x4020_565a);
    }

    #[test]
    fn fourccs_match_the_kernel() {
        assert_eq!(PIX_FMT_H264, 0x3436_3248);
        assert_eq!(PIX_FMT_HEVC, 0x4356_4548);
        assert_eq!(PIX_FMT_AV1_FRAME, 0x4631_5641);
        assert_eq!(PIX_FMT_NV12, 0x3231_564e);
        assert_eq!(PIX_FMT_NV12M, 0x3231_4d4e);
        assert_eq!(fourcc_str(PIX_FMT_HEVC), "HEVC");
    }
}
