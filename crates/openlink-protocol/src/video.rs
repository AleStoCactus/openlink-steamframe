//! Video stream, server -> headset.
//!
//! Each encoded frame is split into datagrams of at most [`MAX_DATAGRAM`] bytes:
//!
//! ```text
//! [VIDEO_MAGIC: 4][PROTOCOL_VERSION: u16][frame: u32][chunk: u16][chunks: u16][payload]
//! ```
//!
//! Chunk 0's payload starts with `[info_len: u16][postcard FrameInfo]`; the rest of the
//! payloads, in chunk order, are the frame's Annex B bitstream. All integers little-endian.
//!
//! Frames are numbered consecutively, so a gap tells the headset a frame was lost. It then
//! drops frames until the next IDR and sends [`crate::HeadsetMessage::VideoLoss`].

use crate::{Pose, MAX_DATAGRAM, PROTOCOL_VERSION};
use serde::{Deserialize, Serialize};

pub const VIDEO_MAGIC: [u8; 4] = *b"OLVD";
const HEADER_LEN: usize = 14;
const PAYLOAD_LEN: usize = MAX_DATAGRAM - HEADER_LEN;

/// Per-frame metadata, carried in chunk 0.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct FrameInfo {
    /// Independently decodable (IDR, with parameter sets).
    pub idr: bool,
    /// HMD pose the frame was rendered with, in the headset's tracking space. The headset
    /// submits the frame with this pose so its compositor reprojects it to the current one.
    pub render_pose: Pose,
    /// When SteamVR presented the frame, server clock (`time::monotonic_ns`).
    pub present_ns: i64,
}

/// Whether a datagram belongs to the video stream (rather than [`crate::ServerMessage`]).
pub fn is_video(datagram: &[u8]) -> bool {
    datagram.len() >= HEADER_LEN && datagram[..4] == VIDEO_MAGIC
}

/// Splits frames into datagrams.
#[derive(Default)]
pub struct Packetizer {
    next_frame: u32,
}

impl Packetizer {
    /// Calls `send` with each datagram of the frame, in order. Returns the frame number.
    pub fn packetize(&mut self, info: &FrameInfo, bitstream: &[u8], mut send: impl FnMut(&[u8])) -> u32 {
        let frame = self.next_frame;
        self.next_frame = self.next_frame.wrapping_add(1);

        let info = postcard::to_allocvec(info).expect("postcard serialisation into Vec");
        let mut data = Vec::with_capacity(2 + info.len() + bitstream.len());
        data.extend_from_slice(&(info.len() as u16).to_le_bytes());
        data.extend_from_slice(&info);
        data.extend_from_slice(bitstream);

        let chunks = data.len().div_ceil(PAYLOAD_LEN);
        assert!(chunks <= u16::MAX as usize, "frame of {} bytes is too large", bitstream.len());
        let mut dgram = Vec::with_capacity(MAX_DATAGRAM);
        for (i, payload) in data.chunks(PAYLOAD_LEN).enumerate() {
            dgram.clear();
            dgram.extend_from_slice(&VIDEO_MAGIC);
            dgram.extend_from_slice(&PROTOCOL_VERSION.to_le_bytes());
            dgram.extend_from_slice(&frame.to_le_bytes());
            dgram.extend_from_slice(&(i as u16).to_le_bytes());
            dgram.extend_from_slice(&(chunks as u16).to_le_bytes());
            dgram.extend_from_slice(payload);
            send(&dgram);
        }
        frame
    }
}

/// A complete frame, ready for the decoder.
#[derive(Debug, Clone, PartialEq)]
pub struct Frame {
    pub number: u32,
    pub info: FrameInfo,
    pub bitstream: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    /// Nothing complete yet.
    Pending,
    /// One or more frames before `next` were lost. Frames are dropped until the next IDR;
    /// the headset should tell the server (once per `Loss`).
    Loss,
}

/// Rebuilds frames from datagrams that may arrive lost or reordered (within a frame).
///
/// Only one frame is assembled at a time: datagrams of an older frame are ignored, and the
/// first datagram of a newer one abandons the current frame (counted as a loss).
pub struct Reassembler {
    current: Option<Partial>,
    /// Number of the last frame handed out or abandoned.
    last: Option<u32>,
    /// After a loss, P frames reference something the decoder never saw.
    need_idr: bool,
    pub stats: ReassemblyStats,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReassemblyStats {
    pub frames: u64,
    pub lost_frames: u64,
    /// Complete frames dropped while waiting for an IDR.
    pub dropped_frames: u64,
}

struct Partial {
    number: u32,
    chunks: Vec<Option<Vec<u8>>>,
    received: usize,
}

impl Default for Reassembler {
    fn default() -> Self {
        Self { current: None, last: None, need_idr: true, stats: ReassemblyStats::default() }
    }
}

impl Reassembler {
    /// Drops frames until the next IDR (e.g. the decoder could not keep up).
    pub fn require_idr(&mut self) {
        self.need_idr = true;
    }

    /// Feeds one datagram. Returns a complete, decodable frame, or what happened instead.
    pub fn push(&mut self, datagram: &[u8]) -> Result<Frame, Event> {
        if !is_video(datagram) || u16::from_le_bytes([datagram[4], datagram[5]]) != PROTOCOL_VERSION {
            return Err(Event::Pending);
        }
        let number = u32::from_le_bytes(datagram[6..10].try_into().unwrap());
        let chunk = u16::from_le_bytes([datagram[10], datagram[11]]) as usize;
        let chunks = u16::from_le_bytes([datagram[12], datagram[13]]) as usize;
        if chunk >= chunks {
            return Err(Event::Pending);
        }
        // Ignore anything not newer than the last finished frame (wrapping compare).
        if self.last.is_some_and(|l| (number.wrapping_sub(l) as i32) <= 0) {
            return Err(Event::Pending);
        }

        let mut missed = 0u64;
        match &self.current {
            Some(p) if p.number == number => {}
            Some(p) if (number.wrapping_sub(p.number) as i32) < 0 => return Err(Event::Pending),
            current => {
                // A newer frame: the one in progress and any skipped entirely are lost.
                missed = match (current, self.last) {
                    (Some(p), _) => number.wrapping_sub(p.number) as u64,
                    (None, Some(l)) => number.wrapping_sub(l) as u64 - 1,
                    (None, None) => 0,
                };
                self.current = Some(Partial { number, chunks: vec![None; chunks], received: 0 });
            }
        }
        let report = missed > 0 && !self.need_idr;
        if missed > 0 {
            self.stats.lost_frames += missed;
            self.need_idr = true;
        }
        let r = self.add(chunk, chunks, datagram);
        if report && r.is_err() {
            return Err(Event::Loss);
        }
        r
    }

    fn add(&mut self, chunk: usize, chunks: usize, datagram: &[u8]) -> Result<Frame, Event> {
        let p = self.current.as_mut().unwrap();
        if p.chunks.len() != chunks || p.chunks[chunk].is_some() {
            return Err(Event::Pending);
        }
        p.chunks[chunk] = Some(datagram[HEADER_LEN..].to_vec());
        p.received += 1;
        if p.received < chunks {
            return Err(Event::Pending);
        }

        let p = self.current.take().unwrap();
        self.last = Some(p.number);
        let data: Vec<u8> = p.chunks.into_iter().flatten().flatten().collect();
        let parsed = (|| {
            let len = u16::from_le_bytes(data.get(..2)?.try_into().ok()?) as usize;
            let info: FrameInfo = postcard::from_bytes(data.get(2..2 + len)?).ok()?;
            Some((info, 2 + len))
        })();
        let Some((info, start)) = parsed else { return Err(Event::Pending) };
        if self.need_idr && !info.idr {
            self.stats.dropped_frames += 1;
            return Err(Event::Pending);
        }
        self.need_idr = false;
        self.stats.frames += 1;
        Ok(Frame { number: p.number, info, bitstream: data[start..].to_vec() })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(idr: bool) -> FrameInfo {
        FrameInfo { idr, render_pose: Pose::IDENTITY, present_ns: 5 }
    }

    fn packets(p: &mut Packetizer, idr: bool, len: usize) -> Vec<Vec<u8>> {
        let bitstream: Vec<u8> = (0..len).map(|i| i as u8).collect();
        let mut out = Vec::new();
        p.packetize(&info(idr), &bitstream, |d| {
            assert!(d.len() <= MAX_DATAGRAM);
            out.push(d.to_vec())
        });
        out
    }

    #[test]
    fn roundtrip_with_reordering() {
        let mut p = Packetizer::default();
        let mut r = Reassembler::default();
        let mut dg = packets(&mut p, true, 100_000);
        dg.reverse();
        let mut got = None;
        for d in &dg {
            if let Ok(f) = r.push(d) {
                got = Some(f);
            }
        }
        let f = got.expect("frame");
        assert_eq!(f.bitstream.len(), 100_000);
        assert_eq!(f.bitstream[1234], (1234 % 256) as u8);
        assert!(f.info.idr);
    }

    #[test]
    fn waits_for_idr_and_reports_loss_once() {
        let mut p = Packetizer::default();
        let mut r = Reassembler::default();
        // A P frame before any IDR is dropped.
        for d in packets(&mut p, false, 3000) {
            assert!(r.push(&d).is_err());
        }
        let idr = packets(&mut p, true, 3000);
        assert!(idr.iter().map(|d| r.push(d)).any(|x| x.is_ok()));
        // Frame 2 loses a chunk; frame 3 reveals the loss.
        let mut f2 = packets(&mut p, false, 3000);
        f2.remove(1);
        for d in &f2 {
            assert_eq!(r.push(d), Err(Event::Pending));
        }
        let f3 = packets(&mut p, false, 3000);
        let events: Vec<_> = f3.iter().map(|d| r.push(d)).collect();
        assert_eq!(events[0], Err(Event::Loss));
        assert!(events.iter().all(|e| e.is_err()), "P frames are dropped until an IDR");
        assert_eq!(r.stats.lost_frames, 1);
        // The P frame before the first IDR, and frame 3.
        assert_eq!(r.stats.dropped_frames, 2);
        // Recovers on the next IDR.
        let f4 = packets(&mut p, true, 3000);
        assert!(f4.iter().map(|d| r.push(d)).any(|x| x.is_ok()));
        assert_eq!(r.stats.frames, 2);
    }

    #[test]
    fn whole_frame_gap_is_a_loss() {
        let mut p = Packetizer::default();
        let mut r = Reassembler::default();
        assert!(packets(&mut p, true, 10).iter().map(|d| r.push(d)).any(|x| x.is_ok()));
        let _lost = packets(&mut p, false, 10);
        assert_eq!(r.push(&packets(&mut p, false, 10)[0]), Err(Event::Loss));
        assert_eq!(r.stats.lost_frames, 1);
    }
}
