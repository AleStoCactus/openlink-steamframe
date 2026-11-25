//! Hardware video decode: `AMediaCodec` (low-latency mode) into an `AImageReader`, whose
//! images are GPU buffers the renderer samples directly.
//!
//! The frame number travels through the codec as the presentation timestamp, so the
//! renderer can look up each picture's [`FrameInfo`] (render pose).

use ndk::hardware_buffer::HardwareBufferUsage;
use ndk::media::image_reader::{AcquireResult, Image, ImageFormat, ImageReader};
use ndk::media::media_codec::{DequeuedInputBufferResult, DequeuedOutputBufferInfoResult, MediaCodec, MediaCodecDirection};
use ndk::media::media_format::MediaFormat;
use openlink_protocol::video::{Frame, FrameInfo};
use openlink_client::ClientState;
use openlink_protocol::Codec;
use std::collections::VecDeque;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Images the reader may hand out at once: the backlog, the one on screen and the ones the GPU
/// may still be reading (see `render::KEEP`), and the decoder's own.
const MAX_IMAGES: i32 = 12;
/// Upper bound on one compressed frame.
const MAX_INPUT_SIZE: i32 = 8 << 20;

/// A decoded picture's metadata and timeline (`time::monotonic_ns`).
#[derive(Debug, Clone, Copy)]
pub struct Picture {
    pub info: FrameInfo,
    pub received_ns: i64,
    pub queued_ns: i64,
    pub decoded_ns: i64,
}

/// What the renderer reads: decoded pictures and their metadata.
#[derive(Default)]
pub struct Output {
    reader: Option<ImageReader>,
    /// Frames queued into the decoder, oldest first.
    infos: VecDeque<(u32, Picture)>,
    /// Pictures taken from the reader but not shown yet, oldest first.
    pending: VecDeque<(Image, Picture)>,
    /// Since the last [`Output::take_counters`]: pictures dropped from the backlog, and
    /// pictures with no matching frame info.
    pub trimmed: u32,
    pub unmatched: u32,
    /// Pictures the codec released to the reader, and pictures collected from it.
    pub released: u32,
    pub collected: u32,
    /// Consecutive display frames with more than one picture waiting.
    behind_frames: u32,
}

/// Decoded pictures allowed to wait for display. One picture per display frame keeps motion
/// smooth when decode times jitter; a longer backlog is latency, so the oldest are dropped.
const MAX_BACKLOG: usize = 2;
/// Display frames a standing backlog is tolerated before a picture is skipped.
const CATCH_UP_AFTER: u32 = 4;
/// Frame numbers further apart than this belong to different server sessions.
const RESTART_DISTANCE: i32 = 300;

// SAFETY: AImageReader is thread-safe; access is serialised by the Mutex anyway.
unsafe impl Send for Output {}

impl Output {
    /// The next decoded picture to show, if one is waiting: the oldest not yet shown, so each
    /// display frame gets a new picture, skipping ahead only when the backlog grows.
    pub fn next(&mut self) -> Option<(Image, Picture)> {
        self.collect();
        // A backlog absorbs decode jitter, but once one forms it never drains by itself (one
        // picture in, one out per frame), adding a frame of latency per waiting picture. If
        // pictures have been waiting for several frames in a row, skip one to catch up.
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

    /// Takes every picture the reader has. Called often (from the decoder thread, about every
    /// millisecond): the codec's output surface keeps only one picture waiting and silently
    /// replaces it with the next, so collecting only once per display frame lost a picture
    /// whenever two finished within one frame.
    pub fn collect(&mut self) {
        while let Some(reader) = self.reader.as_ref() {
            let image = match reader.acquire_next_image() {
                Ok(AcquireResult::Image(i)) => i,
                Ok(_) => break,
                Err(e) => {
                    log::warn!("acquire image: {e:?}");
                    break;
                }
            };
            self.collected += 1;
            match self.picture_for(&image) {
                Some(p) => self.pending.push_back((image, p)),
                None => self.unmatched += 1,
            }
        }
        // One more than the display backlog: the renderer pops one right after.
        while self.pending.len() > MAX_BACKLOG + 1 {
            self.pending.pop_front();
            self.trimmed += 1;
        }
    }

    fn picture_for(&mut self, image: &Image) -> Option<Picture> {
        let number = (image.timestamp().ok()? / 1000) as u32;
        // Entries far ahead of this picture are from before a server restart (frame numbers
        // start over): they would block every match.
        self.infos.retain(|(n, _)| (n.wrapping_sub(number) as i32) < RESTART_DISTANCE);
        while self.infos.front().is_some_and(|(n, _)| (n.wrapping_sub(number) as i32) < 0) {
            self.infos.pop_front();
        }
        let mut picture = match self.infos.front() {
            Some(&(n, p)) if n == number => p,
            _ => return None,
        };
        if picture.decoded_ns == 0 {
            picture.decoded_ns = openlink_protocol::time::monotonic_ns();
        }
        Some(picture)
    }
}

pub fn spawn(frames: Receiver<Frame>, client: Arc<Mutex<ClientState>>, output: Arc<Mutex<Output>>) {
    std::thread::Builder::new()
        .name("decoder".into())
        .spawn(move || {
            let mut decoder: Option<(MediaCodec, (u32, u32, Codec))> = None;
            loop {
                // Short timeout: this loop also releases finished pictures to the renderer.
                let frame = match frames.recv_timeout(Duration::from_millis(1)) {
                    Ok(f) => Some(f),
                    Err(RecvTimeoutError::Timeout) => None,
                    Err(RecvTimeoutError::Disconnected) => return,
                };
                if let Some(frame) = frame {
                    let Some(cfg) = client.lock().unwrap().welcome.clone() else { continue };
                    let want = (2 * cfg.eye_width, cfg.eye_height, cfg.codec);
                    if decoder.as_ref().is_none_or(|(_, have)| *have != want) {
                        if !frame.info.idr {
                            continue; // a new decoder must start at an IDR
                        }
                        decoder = None;
                        {
                            let mut out = output.lock().unwrap();
                            out.pending.clear();
                            out.reader = None;
                        }
                        match create(want, &output) {
                            Ok(c) => decoder = Some((c, want)),
                            Err(e) => {
                                log::error!("decoder: {e}");
                                continue;
                            }
                        }
                    }
                    let (codec, _) = decoder.as_ref().unwrap();
                    let queued_ns = openlink_protocol::time::monotonic_ns();
                    if let Err(e) = queue(codec, &frame) {
                        log::warn!("decoder input: {e}");
                    } else {
                        let picture =
                            Picture { info: frame.info, received_ns: frame.received_ns, queued_ns, decoded_ns: 0 };
                        output.lock().unwrap().infos.push_back((frame.number, picture));
                    }
                }
                if let Some((codec, _)) = &decoder {
                    drain(codec, &output);
                    output.lock().unwrap().collect();
                }
            }
        })
        .expect("spawn decoder thread");
}

fn mime(codec: Codec) -> &'static str {
    match codec {
        Codec::H264 => "video/avc",
        Codec::H265 => "video/hevc",
        Codec::Av1 => "video/av01",
    }
}

/// Codecs this headset can decode in hardware.
pub fn supported() -> Vec<Codec> {
    [Codec::H265, Codec::H264, Codec::Av1]
        .into_iter()
        .filter(|&c| MediaCodec::from_decoder_type(mime(c)).is_some())
        .collect()
}

fn create((width, height, codec): (u32, u32, Codec), output: &Mutex<Output>) -> Result<MediaCodec, String> {
    let reader = ImageReader::new_with_usage(
        width as i32,
        height as i32,
        ImageFormat::PRIVATE,
        HardwareBufferUsage::GPU_SAMPLED_IMAGE,
        MAX_IMAGES,
    )
    .map_err(|e| format!("AImageReader: {e:?}"))?;
    let window = reader.window().map_err(|e| format!("AImageReader window: {e:?}"))?;
    let decoder = MediaCodec::from_decoder_type(mime(codec)).ok_or_else(|| format!("no {} decoder", mime(codec)))?;
    let mut format = MediaFormat::new();
    format.set_str("mime", mime(codec));
    format.set_i32("width", width as i32);
    format.set_i32("height", height as i32);
    format.set_i32("max-input-size", MAX_INPUT_SIZE);
    format.set_i32("low-latency", 1);
    // Qualcomm's own switches: low latency, and output each picture as soon as it is decoded
    // (decode order) instead of holding one back for reordering we never use. Without the
    // latter the decoder adds a frame of latency.
    format.set_i32("vendor.qti-ext-dec-low-latency.enable", 1);
    format.set_i32("vendor.qti-ext-dec-picture-order.enable", 1);
    format.set_i32("priority", 0);
    format.set_i32("operating-rate", i16::MAX as i32);
    decoder
        .configure(&format, Some(&window), MediaCodecDirection::Decoder)
        .map_err(|e| format!("configure: {e:?}"))?;
    decoder.start().map_err(|e| format!("start: {e:?}"))?;
    log::info!(
        "decoder: {} {width}x{height} ({})",
        mime(codec),
        decoder.name().unwrap_or_default()
    );
    output.lock().unwrap().reader = Some(reader);
    Ok(decoder)
}

fn queue(codec: &MediaCodec, frame: &Frame) -> Result<(), String> {
    const BUFFER_FLAG_KEY_FRAME: u32 = 1;
    let mut input = match codec.dequeue_input_buffer(Duration::from_millis(20)) {
        Ok(DequeuedInputBufferResult::Buffer(b)) => b,
        Ok(DequeuedInputBufferResult::TryAgainLater) => return Err("no input buffer free; frame dropped".into()),
        Err(e) => return Err(format!("{e:?}")),
    };
    let dst = input.buffer_mut();
    let n = frame.bitstream.len();
    if n > dst.len() {
        return Err(format!("frame of {n} bytes exceeds the {} byte input buffer", dst.len()));
    }
    // SAFETY: MaybeUninit<u8> and u8 share a layout; we only write.
    unsafe { std::ptr::copy_nonoverlapping(frame.bitstream.as_ptr(), dst.as_mut_ptr().cast::<u8>(), n) };
    let flags = if frame.info.idr { BUFFER_FLAG_KEY_FRAME } else { 0 };
    codec
        .queue_input_buffer(input, 0, n, frame.number as u64, flags)
        .map_err(|e| format!("{e:?}"))
}

/// Sends every finished picture to the image reader.
fn drain(codec: &MediaCodec, output: &Mutex<Output>) {
    loop {
        match codec.dequeue_output_buffer(Duration::ZERO) {
            Ok(DequeuedOutputBufferInfoResult::Buffer(b)) => {
                let number = b.info().presentation_time_us() as u32;
                let now = openlink_protocol::time::monotonic_ns();
                let mut out = output.lock().unwrap();
                out.released += 1;
                if let Some((_, p)) = out.infos.iter_mut().find(|(n, _)| *n == number) {
                    p.decoded_ns = now;
                }
                drop(out);
                if let Err(e) = codec.release_output_buffer(b, true) {
                    log::warn!("release output: {e:?}");
                }
            }
            Ok(DequeuedOutputBufferInfoResult::TryAgainLater) => return,
            Ok(_) => {} // format/buffers changed: nothing to do with surface output
            Err(e) => {
                log::warn!("decoder output: {e:?}");
                return;
            }
        }
    }
}
