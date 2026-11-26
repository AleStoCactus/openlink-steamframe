//! Hardware video decode through a V4L2 stateful memory-to-memory decoder: on the Steam Frame,
//! Qualcomm's Iris video engine at `/dev/video-dec0`.
//!
//! Compressed frames go into the OUTPUT queue; decoded NV12 pictures come back on the CAPTURE
//! queue. Each capture buffer is mapped (for the upload fallback) and, where the driver allows
//! it, exported as a dmabuf the renderer imports into GL without a copy. A picture's buffer is
//! lent to the renderer as a [`Decoded`] and goes back to the decoder when that is dropped.
//!
//! The frame number travels through the decoder as the buffer timestamp, so each picture can be
//! matched to its [`FrameInfo`] (render pose).

use crate::v4l2::{self, Mapping};
use openlink_client::ClientState;
use openlink_protocol::video::{Frame, FrameInfo};
use openlink_protocol::Codec;
use std::collections::VecDeque;
use std::io;
use std::os::fd::{AsFd, BorrowedFd, FromRawFd, OwnedFd};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Upper bound on one compressed frame.
const MAX_INPUT_SIZE: u32 = 8 << 20;
/// Compressed frames the decoder may hold at once.
const INPUT_BUFFERS: u32 = 8;
/// Capture buffers beyond the decoder's own minimum: the backlog, the pictures the renderer
/// holds while the GPU may still read them (`render::KEEP`), and some slack.
const EXTRA_CAPTURE_BUFFERS: u32 = 8;
/// Decoded pictures allowed to wait for display (see the Quest decoder for why).
const MAX_BACKLOG: usize = 2;
/// Display frames a standing backlog is tolerated before a picture is skipped.
const CATCH_UP_AFTER: u32 = 1;
/// Frame numbers further apart than this belong to different server sessions.
const RESTART_DISTANCE: i32 = 300;
/// `OPENLINK_V4L2_DECODER=/dev/videoN` picks the device instead of searching.
const DEVICE_VARIABLE: &str = "OPENLINK_V4L2_DECODER";

/// A decoded picture's metadata and timeline (`time::monotonic_ns`).
#[derive(Debug, Clone, Copy)]
pub struct Picture {
    pub info: FrameInfo,
    pub received_ns: i64,
    pub queued_ns: i64,
    pub decoded_ns: i64,
}

/// Where one of the two NV12 planes lies within a capture buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlaneRef {
    /// Memory plane of the V4L2 buffer.
    pub memory: usize,
    pub offset: usize,
    /// Bytes per row.
    pub pitch: usize,
}

/// The decoder's picture layout: buffer size in pixels (coded, aligned), the visible part, and
/// where the luma (R8) and chroma (interleaved CbCr, RG8 at half resolution) planes are.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Layout {
    pub width: u32,
    pub height: u32,
    pub visible: v4l2::Rect,
    pub luma: PlaneRef,
    pub chroma: PlaneRef,
}

/// One capture buffer's memory planes: mapped, and exported as dmabufs where possible.
pub struct CaptureBuffer {
    pub maps: Vec<Mapping>,
    pub dmabufs: Option<Vec<OwnedFd>>,
}

/// A picture lent to the renderer. Dropping it hands the buffer back to the decoder.
pub struct Decoded {
    pub picture: Picture,
    pub layout: Layout,
    /// Changes whenever the decoder is recreated; with `index` it names the buffer.
    pub generation: u64,
    pub index: u32,
    pub buffers: Arc<Vec<CaptureBuffer>>,
    release: Sender<(u64, u32)>,
}

impl Decoded {
    pub fn buffer(&self) -> &CaptureBuffer {
        &self.buffers[self.index as usize]
    }

    /// The bytes of one plane, from its first row on.
    pub fn plane(&self, plane: PlaneRef) -> &[u8] {
        let map = &self.buffer().maps[plane.memory];
        // SAFETY: the buffer is dequeued (the decoder does not write it until it is released).
        let bytes = unsafe { map.bytes() };
        &bytes[plane.offset.min(bytes.len())..]
    }
}

#[cfg(test)]
impl Decoded {
    /// A picture backed by any mappable memory, for renderer tests.
    pub fn for_test(picture: Picture, layout: Layout, buffers: Vec<CaptureBuffer>) -> (Self, Receiver<(u64, u32)>) {
        let (release, released) = mpsc::channel();
        (Self { picture, layout, generation: 1, index: 0, buffers: Arc::new(buffers), release }, released)
    }
}

impl Drop for Decoded {
    fn drop(&mut self) {
        let _ = self.release.send((self.generation, self.index));
    }
}

/// What the renderer reads: decoded pictures, oldest first.
#[derive(Default)]
pub struct Output {
    pending: VecDeque<Decoded>,
    /// Since the counters were last read: pictures dropped from the backlog, pictures without
    /// a matching frame info, and pictures decoded.
    pub trimmed: u32,
    pub unmatched: u32,
    pub decoded: u32,
    /// Consecutive display frames with more than one picture waiting.
    behind_frames: u32,
}

impl Output {
    /// The next decoded picture to show, if one is waiting: the oldest not yet shown, so each
    /// display frame gets a new picture, skipping ahead only when a backlog persists.
    pub fn next(&mut self) -> Option<Decoded> {
        if self.pending.len() > 1 {
            self.behind_frames += 1;
            if self.behind_frames > CATCH_UP_AFTER {
                self.pending.pop_front();
                self.trimmed += 1;
                self.behind_frames = 0;
            }
        } else {
            self.behind_frames = 0;
        }
        self.pending.pop_front()
    }

    fn push(&mut self, d: Decoded) {
        self.decoded += 1;
        self.pending.push_back(d);
        // One more than the display backlog: the renderer pops one right after.
        while self.pending.len() > MAX_BACKLOG + 1 {
            self.pending.pop_front();
            self.trimmed += 1;
        }
    }
}

pub fn spawn(frames: Receiver<Frame>, client: Arc<Mutex<ClientState>>, output: Arc<Mutex<Output>>) {
    std::thread::Builder::new()
        .name("decoder".into())
        .spawn(move || {
            let (release_tx, release_rx) = mpsc::channel();
            let mut decoder: Option<(Decoder, (u32, u32, Codec))> = None;
            let mut generation = 0u64;
            loop {
                // Short timeout: this loop also collects finished pictures and recycles buffers.
                let frame = match frames.recv_timeout(Duration::from_millis(1)) {
                    Ok(f) => Some(f),
                    Err(RecvTimeoutError::Timeout) => None,
                    Err(RecvTimeoutError::Disconnected) => return,
                };
                if let Some(frame) = frame {
                    let Some(cfg) = client.lock().unwrap().welcome.clone() else { continue };
                    // The coded picture: smaller than the eyes are shown at when foveated.
                    let (eye_w, eye_h) = cfg.coded_eye_size();
                    let want = (2 * eye_w, eye_h, cfg.codec);
                    if decoder.as_ref().is_none_or(|(_, have)| *have != want) {
                        if !frame.info.idr {
                            continue; // a new decoder must start at an IDR
                        }
                        decoder = None;
                        output.lock().unwrap().pending.clear();
                        generation += 1;
                        match Decoder::open(want, generation, release_tx.clone()) {
                            Ok(d) => decoder = Some((d, want)),
                            Err(e) => {
                                log::error!("decoder: {e}");
                                continue;
                            }
                        }
                    }
                    let (d, _) = decoder.as_mut().unwrap();
                    if let Err(e) = d.queue(&frame) {
                        log::warn!("decoder input: {e}");
                    }
                }
                let Some((d, _)) = decoder.as_mut() else {
                    while release_rx.try_recv().is_ok() {}
                    continue;
                };
                while let Ok((g, index)) = release_rx.try_recv() {
                    // Buffers of an earlier decoder simply unmap once the last picture goes.
                    if g == d.generation {
                        if let Err(e) = d.requeue(index) {
                            log::warn!("decoder: cannot recycle buffer {index}: {e}");
                        }
                    }
                }
                if let Err(e) = d.poll(&output) {
                    // Start over at the next IDR.
                    log::error!("decoder: {e}; restarting");
                    decoder = None;
                    output.lock().unwrap().pending.clear();
                }
            }
        })
        .expect("spawn decoder thread");
}

fn pixel_format(codec: Codec) -> u32 {
    match codec {
        Codec::H264 => v4l2::PIX_FMT_H264,
        Codec::H265 => v4l2::PIX_FMT_HEVC,
        Codec::Av1 => v4l2::PIX_FMT_AV1_FRAME,
    }
}

/// Stateful decoders on this system and the codecs each accepts. `/dev/video-dec0` (the Steam
/// Frame's name for its decoder) first, then every `/dev/videoN`.
fn decoders() -> Vec<(PathBuf, String, Vec<u32>)> {
    let mut paths: Vec<PathBuf> = match std::env::var_os(DEVICE_VARIABLE) {
        Some(p) => vec![PathBuf::from(p)],
        None => {
            let mut v = vec![PathBuf::from("/dev/video-dec0")];
            v.extend((0..64).map(|i| PathBuf::from(format!("/dev/video{i}"))));
            v
        }
    };
    paths.dedup();
    let mut out: Vec<(PathBuf, String, Vec<u32>)> = Vec::new();
    for path in paths {
        let Ok(fd) = v4l2::open(&path) else { continue };
        let Ok(cap) = v4l2::query_cap(fd.as_fd()) else { continue };
        if cap.caps() & v4l2::CAP_VIDEO_M2M_MPLANE == 0 || cap.caps() & v4l2::CAP_STREAMING == 0 {
            continue;
        }
        let inputs = v4l2::formats(fd.as_fd(), v4l2::BUF_TYPE_VIDEO_OUTPUT_MPLANE);
        if !inputs.iter().any(|&f| [Codec::H264, Codec::H265, Codec::Av1].iter().any(|&c| pixel_format(c) == f)) {
            continue; // an encoder, scaler or other converter
        }
        // `/dev/video-dec0` is usually a link to one of the numbered nodes.
        let real = std::fs::canonicalize(&path).unwrap_or(path.clone());
        if out.iter().any(|(p, _, _)| std::fs::canonicalize(p).unwrap_or(p.clone()) == real) {
            continue;
        }
        out.push((path, cap.driver(), inputs));
    }
    out
}

/// Codecs this headset can decode in hardware.
pub fn supported() -> Vec<Codec> {
    let found = decoders();
    for (path, driver, formats) in &found {
        log::info!(
            "V4L2 decoder {} ({driver}): {}",
            path.display(),
            formats.iter().map(|&f| v4l2::fourcc_str(f)).collect::<Vec<_>>().join(", ")
        );
    }
    if found.is_empty() {
        log::error!("no V4L2 stateful video decoder found (set {DEVICE_VARIABLE} to its /dev/videoN)");
    }
    [Codec::H265, Codec::H264, Codec::Av1]
        .into_iter()
        .filter(|&c| found.iter().any(|(_, _, f)| f.contains(&pixel_format(c))))
        .collect()
}

struct Capture {
    buffers: Arc<Vec<CaptureBuffer>>,
    layout: Layout,
    planes: u32,
}

struct Decoder {
    fd: OwnedFd,
    driver: String,
    generation: u64,
    release: Sender<(u64, u32)>,
    inputs: Vec<Mapping>,
    free_inputs: Vec<u32>,
    capture: Option<Capture>,
    /// Frames queued for decoding, oldest first.
    infos: VecDeque<(u32, Picture)>,
    /// Whether exporting capture buffers as dmabufs failed (then only mapping is used).
    no_dmabuf: bool,
}

impl Decoder {
    fn open((width, height, codec): (u32, u32, Codec), generation: u64, release: Sender<(u64, u32)>) -> Result<Self, String> {
        let format = pixel_format(codec);
        let (path, driver, _) = decoders()
            .into_iter()
            .find(|(_, _, f)| f.contains(&format))
            .ok_or_else(|| format!("no V4L2 decoder for {}", v4l2::fourcc_str(format)))?;
        let fd = v4l2::open(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        let f = fd.as_fd();

        let mut fmt = v4l2::Format::zeroed();
        fmt.ty = v4l2::BUF_TYPE_VIDEO_OUTPUT_MPLANE;
        fmt.pix_mp.width = width;
        fmt.pix_mp.height = height;
        fmt.pix_mp.pixelformat = format;
        fmt.pix_mp.field = v4l2::FIELD_NONE;
        fmt.pix_mp.num_planes = 1;
        fmt.pix_mp.plane_fmt[0].sizeimage = MAX_INPUT_SIZE;
        // SAFETY: matching struct.
        unsafe { v4l2::ioctl(f, v4l2::VIDIOC_S_FMT, &mut fmt) }.map_err(|e| format!("set input format: {e}"))?;

        // Ask for NV12 now; the decoder confirms (or overrides) it once it has parsed the headers.
        let mut out = v4l2::Format::zeroed();
        out.ty = v4l2::BUF_TYPE_VIDEO_CAPTURE_MPLANE;
        out.pix_mp.width = width;
        out.pix_mp.height = height;
        out.pix_mp.pixelformat = v4l2::PIX_FMT_NV12;
        out.pix_mp.num_planes = 1;
        // SAFETY: matching struct.
        if let Err(e) = unsafe { v4l2::ioctl(f, v4l2::VIDIOC_S_FMT, &mut out) } {
            log::debug!("decoder: early NV12 request refused ({e}); retrying after the headers");
        }

        // Show each picture as soon as it is decoded: the stream has no reordering.
        for (id, value) in [(v4l2::CID_MPEG_VIDEO_DEC_DISPLAY_DELAY_ENABLE, 1), (v4l2::CID_MPEG_VIDEO_DEC_DISPLAY_DELAY, 0)] {
            if let Err(e) = set_control(f, id, value) {
                log::info!("decoder: control {id:#x} not available ({e})");
            }
        }

        subscribe(f, v4l2::EVENT_SOURCE_CHANGE).map_err(|e| format!("subscribe to source changes: {e}"))?;
        let _ = subscribe(f, v4l2::EVENT_EOS);

        let count = request_buffers(f, v4l2::BUF_TYPE_VIDEO_OUTPUT_MPLANE, INPUT_BUFFERS)
            .map_err(|e| format!("input buffers: {e}"))?;
        let mut inputs = Vec::new();
        for index in 0..count {
            let mut planes = [v4l2::Plane::zeroed(); 1];
            query_buffer(f, v4l2::BUF_TYPE_VIDEO_OUTPUT_MPLANE, index, &mut planes)
                .map_err(|e| format!("input buffer {index}: {e}"))?;
            inputs.push(Mapping::new(f, planes[0].mem_offset(), planes[0].length).map_err(|e| format!("map input buffer: {e}"))?);
        }
        stream(f, v4l2::BUF_TYPE_VIDEO_OUTPUT_MPLANE, true).map_err(|e| format!("start input: {e}"))?;
        log::info!(
            "decoder: {} {width}x{height} on {} ({driver}), {count} input buffers",
            v4l2::fourcc_str(format),
            path.display()
        );
        Ok(Self {
            fd,
            driver,
            generation,
            release,
            free_inputs: (0..count).rev().collect(),
            inputs,
            capture: None,
            infos: VecDeque::new(),
            no_dmabuf: false,
        })
    }

    fn queue(&mut self, frame: &Frame) -> Result<(), String> {
        let queued_ns = openlink_protocol::time::monotonic_ns();
        // Wait a little for an input buffer to come back, as the Android decoder does.
        let deadline = Instant::now() + Duration::from_millis(20);
        let index = loop {
            self.reclaim_inputs()?;
            if let Some(i) = self.free_inputs.pop() {
                break i;
            }
            if Instant::now() > deadline {
                return Err("no input buffer free; frame dropped".into());
            }
            wait(self.fd.as_fd(), libc::POLLOUT, 2);
        };
        let map = &self.inputs[index as usize];
        let n = frame.bitstream.len();
        if n > map.len {
            self.free_inputs.push(index);
            return Err(format!("frame of {n} bytes exceeds the {} byte input buffer", map.len));
        }
        // SAFETY: the buffer is ours (dequeued) and at least `n` bytes long.
        unsafe { std::ptr::copy_nonoverlapping(frame.bitstream.as_ptr(), map.ptr, n) };
        let mut planes = [v4l2::Plane::zeroed(); 1];
        planes[0].bytesused = n as u32;
        planes[0].length = map.len as u32;
        let mut buf = v4l2::Buffer::zeroed();
        buf.index = index;
        buf.ty = v4l2::BUF_TYPE_VIDEO_OUTPUT_MPLANE;
        buf.memory = v4l2::MEMORY_MMAP;
        buf.field = v4l2::FIELD_NONE;
        buf.timestamp = frame_timestamp(frame.number);
        buf.m = planes.as_mut_ptr();
        buf.length = 1;
        // SAFETY: matching struct; `planes` outlives the call.
        if let Err(e) = unsafe { v4l2::ioctl(self.fd.as_fd(), v4l2::VIDIOC_QBUF, &mut buf) } {
            self.free_inputs.push(index);
            return Err(format!("queue: {e}"));
        }
        let picture = Picture { info: frame.info, received_ns: frame.received_ns, queued_ns, decoded_ns: 0 };
        self.infos.push_back((frame.number, picture));
        // Pictures that never came out (dropped by the decoder) must not pile up.
        while self.infos.len() > 64 {
            self.infos.pop_front();
        }
        Ok(())
    }

    /// Input buffers the decoder has consumed.
    fn reclaim_inputs(&mut self) -> Result<(), String> {
        loop {
            let mut planes = [v4l2::Plane::zeroed(); 1];
            match dequeue(self.fd.as_fd(), v4l2::BUF_TYPE_VIDEO_OUTPUT_MPLANE, &mut planes) {
                Ok(Some(buf)) => self.free_inputs.push(buf.index),
                Ok(None) => return Ok(()),
                Err(e) => return Err(format!("dequeue input: {e}")),
            }
        }
    }

    /// Handles decoder events and moves finished pictures to `output`.
    fn poll(&mut self, output: &Mutex<Output>) -> Result<(), String> {
        while let Some(event) = dequeue_event(self.fd.as_fd()) {
            if event.ty == v4l2::EVENT_SOURCE_CHANGE && (event.u[0] as u32) & v4l2::EVENT_SRC_CH_RESOLUTION != 0 {
                if self.capture.is_some() {
                    // The stream's size is fixed per session; a new size comes with a new
                    // StreamConfig, for which the whole decoder is recreated.
                    return Err("the stream changed resolution mid-session".into());
                }
                self.setup_capture()?;
            } else if event.ty == v4l2::EVENT_EOS {
                log::debug!("decoder: end of stream");
            }
        }
        let Some(count) = self.capture.as_ref().map(|c| c.planes as usize) else { return Ok(()) };
        loop {
            let mut planes = [v4l2::Plane::zeroed(); v4l2::VIDEO_MAX_PLANES];
            let buf = match dequeue(self.fd.as_fd(), v4l2::BUF_TYPE_VIDEO_CAPTURE_MPLANE, &mut planes[..count]) {
                Ok(Some(b)) => b,
                Ok(None) => return Ok(()),
                // EPIPE: the last buffer was dequeued (drain); nothing more will come.
                Err(e) if e.raw_os_error() == Some(libc::EPIPE) => return Ok(()),
                Err(e) => return Err(format!("dequeue picture: {e}")),
            };
            let empty = planes[0].bytesused == 0;
            if buf.flags & v4l2::BUF_FLAG_ERROR != 0 || empty {
                if buf.flags & v4l2::BUF_FLAG_LAST == 0 {
                    self.requeue(buf.index).map_err(|e| format!("requeue: {e}"))?;
                }
                continue;
            }
            let number = timestamp_frame(&buf.timestamp);
            let Some(mut picture) = self.picture_for(number) else {
                output.lock().unwrap().unmatched += 1;
                self.requeue(buf.index).map_err(|e| format!("requeue: {e}"))?;
                continue;
            };
            picture.decoded_ns = openlink_protocol::time::monotonic_ns();
            let capture = self.capture.as_ref().unwrap();
            output.lock().unwrap().push(Decoded {
                picture,
                layout: capture.layout,
                generation: self.generation,
                index: buf.index,
                buffers: capture.buffers.clone(),
                release: self.release.clone(),
            });
        }
    }

    fn picture_for(&mut self, number: u32) -> Option<Picture> {
        // Entries far ahead of this picture are from before a server restart (frame numbers
        // start over): they would block every match.
        self.infos.retain(|(n, _)| (n.wrapping_sub(number) as i32) < RESTART_DISTANCE);
        while self.infos.front().is_some_and(|(n, _)| (n.wrapping_sub(number) as i32) < 0) {
            self.infos.pop_front();
        }
        match self.infos.front() {
            Some(&(n, p)) if n == number => {
                self.infos.pop_front();
                Some(p)
            }
            _ => None,
        }
    }

    /// The decoder has parsed the stream's headers: set up the picture buffers.
    fn setup_capture(&mut self) -> Result<(), String> {
        let f = self.fd.as_fd();
        let mut fmt = get_format(f, v4l2::BUF_TYPE_VIDEO_CAPTURE_MPLANE).map_err(|e| format!("picture format: {e}"))?;
        if !matches!(fmt.pix_mp.pixelformat, v4l2::PIX_FMT_NV12 | v4l2::PIX_FMT_NV12M) {
            let offered = fmt.pix_mp.pixelformat;
            fmt.pix_mp.pixelformat = v4l2::PIX_FMT_NV12;
            // SAFETY: matching struct.
            unsafe { v4l2::ioctl(f, v4l2::VIDIOC_S_FMT, &mut fmt) }.map_err(|e| format!("set NV12 output: {e}"))?;
            fmt = get_format(f, v4l2::BUF_TYPE_VIDEO_CAPTURE_MPLANE).map_err(|e| format!("picture format: {e}"))?;
            if !matches!(fmt.pix_mp.pixelformat, v4l2::PIX_FMT_NV12 | v4l2::PIX_FMT_NV12M) {
                return Err(format!(
                    "the decoder outputs {} (wanted NV12; available: {})",
                    v4l2::fourcc_str(offered),
                    v4l2::formats(f, v4l2::BUF_TYPE_VIDEO_CAPTURE_MPLANE)
                        .iter()
                        .map(|&x| v4l2::fourcc_str(x))
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
        }
        let p = fmt.pix_mp;
        let (width, height, pixelformat, planes) = (p.width, p.height, p.pixelformat, p.num_planes as u32);
        let plane_fmt = p.plane_fmt;
        let pitches: Vec<u32> = (0..planes as usize).map(|i| plane_fmt[i].bytesperline).collect();
        let visible = visible_rect(f).unwrap_or(v4l2::Rect { left: 0, top: 0, width, height });
        let layout = layout(pixelformat, width, height, &pitches, visible, &self.driver)?;

        let min = get_control(f, v4l2::CID_MIN_BUFFERS_FOR_CAPTURE).unwrap_or(4).max(1) as u32;
        let count = request_buffers(f, v4l2::BUF_TYPE_VIDEO_CAPTURE_MPLANE, min + EXTRA_CAPTURE_BUFFERS)
            .map_err(|e| format!("picture buffers: {e}"))?;
        let mut buffers = Vec::new();
        for index in 0..count {
            let mut info = [v4l2::Plane::zeroed(); v4l2::VIDEO_MAX_PLANES];
            query_buffer(f, v4l2::BUF_TYPE_VIDEO_CAPTURE_MPLANE, index, &mut info[..planes as usize])
                .map_err(|e| format!("picture buffer {index}: {e}"))?;
            let mut maps = Vec::new();
            for plane in &info[..planes as usize] {
                maps.push(Mapping::new(f, plane.mem_offset(), plane.length).map_err(|e| format!("map picture buffer: {e}"))?);
            }
            let dmabufs = if self.no_dmabuf {
                None
            } else {
                match (0..planes).map(|plane| export(f, index, plane)).collect::<io::Result<Vec<_>>>() {
                    Ok(fds) => Some(fds),
                    Err(e) => {
                        log::warn!("decoder: cannot export picture buffers as dmabufs ({e}); pictures will be copied");
                        self.no_dmabuf = true;
                        None
                    }
                }
            };
            buffers.push(CaptureBuffer { maps, dmabufs });
        }
        self.capture = Some(Capture { buffers: Arc::new(buffers), layout, planes });
        for index in 0..count {
            self.requeue(index).map_err(|e| format!("queue picture buffer: {e}"))?;
        }
        stream(f, v4l2::BUF_TYPE_VIDEO_CAPTURE_MPLANE, true).map_err(|e| format!("start output: {e}"))?;
        log::info!(
            "decoder: pictures {} {width}x{height} (visible {}x{} at {},{}), pitch {:?}, {count} buffers (decoder needs {min}), {}",
            v4l2::fourcc_str(pixelformat),
            visible.width,
            visible.height,
            visible.left,
            visible.top,
            pitches,
            if self.no_dmabuf { "mapped" } else { "exported as dmabufs" }
        );
        Ok(())
    }

    fn requeue(&self, index: u32) -> io::Result<()> {
        let Some(capture) = &self.capture else { return Ok(()) };
        let mut planes = [v4l2::Plane::zeroed(); v4l2::VIDEO_MAX_PLANES];
        let mut buf = v4l2::Buffer::zeroed();
        buf.index = index;
        buf.ty = v4l2::BUF_TYPE_VIDEO_CAPTURE_MPLANE;
        buf.memory = v4l2::MEMORY_MMAP;
        buf.m = planes.as_mut_ptr();
        buf.length = capture.planes;
        // SAFETY: matching struct; `planes` outlives the call.
        unsafe { v4l2::ioctl(self.fd.as_fd(), v4l2::VIDIOC_QBUF, &mut buf) }
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        let f = self.fd.as_fd();
        let _ = stream(f, v4l2::BUF_TYPE_VIDEO_CAPTURE_MPLANE, false);
        let _ = stream(f, v4l2::BUF_TYPE_VIDEO_OUTPUT_MPLANE, false);
        // Mapped buffers (here and with the renderer) stay valid until unmapped.
    }
}

/// Where the planes are. With one memory plane (NV12) chroma follows the luma rows; Qualcomm's
/// drivers align that luma height to 32 rows without saying so in the format.
fn layout(pixelformat: u32, width: u32, height: u32, pitches: &[u32], visible: v4l2::Rect, driver: &str) -> Result<Layout, String> {
    let luma_pitch = *pitches.first().ok_or("no planes")? as usize;
    let chroma = if pixelformat == v4l2::PIX_FMT_NV12M {
        let pitch = *pitches.get(1).ok_or("NV12M with one plane")? as usize;
        PlaneRef { memory: 1, offset: 0, pitch }
    } else {
        let qualcomm = driver.contains("iris") || driver.contains("venus");
        let rows = if qualcomm { height.next_multiple_of(32) } else { height };
        PlaneRef { memory: 0, offset: luma_pitch * rows as usize, pitch: luma_pitch }
    };
    if luma_pitch < width as usize || chroma.pitch < width as usize {
        return Err(format!("pitch {luma_pitch}/{} narrower than {width} pixels", chroma.pitch));
    }
    Ok(Layout { width, height, visible, luma: PlaneRef { memory: 0, offset: 0, pitch: luma_pitch }, chroma })
}

fn frame_timestamp(number: u32) -> libc::timeval {
    libc::timeval { tv_sec: number as libc::time_t, tv_usec: 0 }
}

fn timestamp_frame(t: &libc::timeval) -> u32 {
    t.tv_sec as u32
}

fn set_control(fd: BorrowedFd, id: u32, value: i32) -> io::Result<()> {
    let mut c = v4l2::Control { id, value };
    // SAFETY: matching struct.
    unsafe { v4l2::ioctl(fd, v4l2::VIDIOC_S_CTRL, &mut c) }
}

fn get_control(fd: BorrowedFd, id: u32) -> io::Result<i32> {
    let mut c = v4l2::Control { id, value: 0 };
    // SAFETY: matching struct.
    unsafe { v4l2::ioctl(fd, v4l2::VIDIOC_G_CTRL, &mut c)? };
    Ok(c.value)
}

fn subscribe(fd: BorrowedFd, ty: u32) -> io::Result<()> {
    let mut s = v4l2::EventSubscription::zeroed();
    s.ty = ty;
    // SAFETY: matching struct.
    unsafe { v4l2::ioctl(fd, v4l2::VIDIOC_SUBSCRIBE_EVENT, &mut s) }
}

fn dequeue_event(fd: BorrowedFd) -> Option<v4l2::Event> {
    let mut e = v4l2::Event::zeroed();
    // SAFETY: matching struct. ENOENT: no event pending.
    unsafe { v4l2::ioctl(fd, v4l2::VIDIOC_DQEVENT, &mut e) }.ok().map(|()| e)
}

fn get_format(fd: BorrowedFd, ty: u32) -> io::Result<v4l2::Format> {
    let mut fmt = v4l2::Format::zeroed();
    fmt.ty = ty;
    // SAFETY: matching struct.
    unsafe { v4l2::ioctl(fd, v4l2::VIDIOC_G_FMT, &mut fmt)? };
    Ok(fmt)
}

/// The part of the decoded picture that holds the image (the stream's conformance window).
fn visible_rect(fd: BorrowedFd) -> Option<v4l2::Rect> {
    // The selection API takes the single-planar type; some drivers want the multi-planar one.
    for ty in [v4l2::BUF_TYPE_VIDEO_CAPTURE, v4l2::BUF_TYPE_VIDEO_CAPTURE_MPLANE] {
        let mut s = v4l2::Selection::zeroed();
        s.ty = ty;
        s.target = v4l2::SEL_TGT_COMPOSE;
        // SAFETY: matching struct.
        if unsafe { v4l2::ioctl(fd, v4l2::VIDIOC_G_SELECTION, &mut s) }.is_ok() && s.r.width > 0 && s.r.height > 0 {
            return Some(s.r);
        }
    }
    None
}

fn request_buffers(fd: BorrowedFd, ty: u32, count: u32) -> io::Result<u32> {
    let mut r = v4l2::RequestBuffers::zeroed();
    r.count = count;
    r.ty = ty;
    r.memory = v4l2::MEMORY_MMAP;
    // SAFETY: matching struct.
    unsafe { v4l2::ioctl(fd, v4l2::VIDIOC_REQBUFS, &mut r)? };
    if r.count == 0 {
        return Err(io::Error::other("the driver allocated no buffers"));
    }
    Ok(r.count)
}

fn query_buffer(fd: BorrowedFd, ty: u32, index: u32, planes: &mut [v4l2::Plane]) -> io::Result<()> {
    let mut buf = v4l2::Buffer::zeroed();
    buf.index = index;
    buf.ty = ty;
    buf.memory = v4l2::MEMORY_MMAP;
    buf.m = planes.as_mut_ptr();
    buf.length = planes.len() as u32;
    // SAFETY: matching struct; `planes` outlives the call.
    unsafe { v4l2::ioctl(fd, v4l2::VIDIOC_QUERYBUF, &mut buf) }
}

/// A finished buffer, or `None` if there is none yet (non-blocking descriptor).
fn dequeue(fd: BorrowedFd, ty: u32, planes: &mut [v4l2::Plane]) -> io::Result<Option<v4l2::Buffer>> {
    let mut buf = v4l2::Buffer::zeroed();
    buf.ty = ty;
    buf.memory = v4l2::MEMORY_MMAP;
    buf.m = planes.as_mut_ptr();
    buf.length = planes.len() as u32;
    // SAFETY: matching struct; `planes` outlives the call.
    match unsafe { v4l2::ioctl(fd, v4l2::VIDIOC_DQBUF, &mut buf) } {
        Ok(()) => Ok(Some(buf)),
        Err(e) if e.kind() == io::ErrorKind::WouldBlock => Ok(None),
        Err(e) => Err(e),
    }
}

fn export(fd: BorrowedFd, index: u32, plane: u32) -> io::Result<OwnedFd> {
    let mut e = v4l2::ExportBuffer::zeroed();
    e.ty = v4l2::BUF_TYPE_VIDEO_CAPTURE_MPLANE;
    e.index = index;
    e.plane = plane;
    e.flags = (libc::O_RDONLY | libc::O_CLOEXEC) as u32;
    // SAFETY: matching struct.
    unsafe { v4l2::ioctl(fd, v4l2::VIDIOC_EXPBUF, &mut e)? };
    // SAFETY: the kernel handed us this new descriptor.
    Ok(unsafe { OwnedFd::from_raw_fd(e.fd) })
}

fn stream(fd: BorrowedFd, ty: u32, on: bool) -> io::Result<()> {
    let mut t = ty as i32;
    let request = if on { v4l2::VIDIOC_STREAMON } else { v4l2::VIDIOC_STREAMOFF };
    // SAFETY: the argument is the buffer type as an int.
    unsafe { v4l2::ioctl(fd, request, &mut t) }
}

fn wait(fd: BorrowedFd, events: i16, timeout_ms: i32) {
    use std::os::fd::AsRawFd;
    let mut p = libc::pollfd { fd: fd.as_raw_fd(), events, revents: 0 };
    // SAFETY: one valid pollfd.
    unsafe { libc::poll(&mut p, 1, timeout_ms) };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_numbers_survive_the_timestamp() {
        for n in [0u32, 1, 89, 1 << 20, u32::MAX] {
            assert_eq!(timestamp_frame(&frame_timestamp(n)), n);
        }
    }

    #[test]
    fn nv12_chroma_follows_aligned_luma_on_qualcomm() {
        let visible = v4l2::Rect { left: 0, top: 0, width: 4128, height: 2200 };
        let l = layout(v4l2::PIX_FMT_NV12, 4128, 2200, &[4224], visible, "qcom-iris").unwrap();
        assert_eq!(l.luma, PlaneRef { memory: 0, offset: 0, pitch: 4224 });
        assert_eq!(l.chroma, PlaneRef { memory: 0, offset: 4224 * 2208, pitch: 4224 });
        let l = layout(v4l2::PIX_FMT_NV12, 4128, 2200, &[4224], visible, "other").unwrap();
        assert_eq!(l.chroma.offset, 4224 * 2200);
        let l = layout(v4l2::PIX_FMT_NV12M, 4128, 2200, &[4224, 4224], visible, "qcom-iris").unwrap();
        assert_eq!(l.chroma, PlaneRef { memory: 1, offset: 0, pitch: 4224 });
        assert!(layout(v4l2::PIX_FMT_NV12, 4128, 2200, &[2048], visible, "x").is_err());
    }
}
