//! Audio streams: game audio server -> headset, microphone headset -> server.
//!
//! Uncompressed 16-bit PCM at [`SAMPLE_RATE`]: stereo game audio is 1.5 Mbit/s and the mono
//! microphone 0.8 Mbit/s, small next to the video, and it needs no codec on either side.
//! Each datagram carries [`PACKET_FRAMES`] frames (5 ms):
//!
//! ```text
//! [AUDIO_MAGIC: 4][PROTOCOL_VERSION: u16][stream: u8][seq: u32][interleaved s16 samples]
//! ```
//!
//! All integers little-endian. `seq` counts packets per stream, so the receiver can fill a lost
//! packet with silence and keep the timing. The two ends' audio clocks drift apart slowly (tens
//! of ppm); [`JitterBuffer`] absorbs both network jitter and that drift.

use crate::PROTOCOL_VERSION;
use std::collections::VecDeque;

pub const AUDIO_MAGIC: [u8; 4] = *b"OLAU";
const HEADER_LEN: usize = 11;

pub const SAMPLE_RATE: u32 = 48_000;
/// Frames per datagram: 5 ms.
pub const PACKET_FRAMES: usize = 240;
/// Game audio, server -> headset.
pub const SPEAKER_CHANNELS: usize = 2;
/// Microphone, headset -> server.
pub const MIC_CHANNELS: usize = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Stream {
    Speakers = 0,
    Microphone = 1,
}

impl Stream {
    pub fn channels(self) -> usize {
        match self {
            Stream::Speakers => SPEAKER_CHANNELS,
            Stream::Microphone => MIC_CHANNELS,
        }
    }
}

/// Whether a datagram belongs to an audio stream.
pub fn is_audio(datagram: &[u8]) -> bool {
    datagram.len() >= HEADER_LEN && datagram[..4] == AUDIO_MAGIC
}

/// A received audio datagram.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Packet<'a> {
    pub stream: Stream,
    pub seq: u32,
    /// Interleaved s16le samples.
    pub pcm: &'a [u8],
}

pub fn parse(datagram: &[u8]) -> Option<Packet<'_>> {
    if !is_audio(datagram) || u16::from_le_bytes([datagram[4], datagram[5]]) != PROTOCOL_VERSION {
        return None;
    }
    let stream = match datagram[6] {
        0 => Stream::Speakers,
        1 => Stream::Microphone,
        _ => return None,
    };
    let seq = u32::from_le_bytes(datagram[7..11].try_into().unwrap());
    let pcm = &datagram[HEADER_LEN..];
    if pcm.is_empty() || !pcm.len().is_multiple_of(2 * stream.channels()) {
        return None;
    }
    Some(Packet { stream, seq, pcm })
}

/// Cuts a continuous stream of samples, delivered in whatever block sizes the audio system
/// uses, into [`PACKET_FRAMES`] datagrams.
pub struct Packetizer {
    stream: Stream,
    seq: u32,
    dgram: Vec<u8>,
}

impl Packetizer {
    pub fn new(stream: Stream) -> Self {
        let mut p = Self { stream, seq: 0, dgram: Vec::with_capacity(HEADER_LEN + PACKET_FRAMES * 2 * stream.channels()) };
        p.start();
        p
    }

    fn start(&mut self) {
        self.dgram.clear();
        self.dgram.extend_from_slice(&AUDIO_MAGIC);
        self.dgram.extend_from_slice(&PROTOCOL_VERSION.to_le_bytes());
        self.dgram.push(self.stream as u8);
        self.dgram.extend_from_slice(&self.seq.to_le_bytes());
    }

    fn full(&self) -> bool {
        self.dgram.len() == HEADER_LEN + PACKET_FRAMES * 2 * self.stream.channels()
    }

    /// Appends interleaved samples; calls `send` with each datagram that fills up.
    pub fn push(&mut self, samples: impl IntoIterator<Item = i16>, mut send: impl FnMut(&[u8])) {
        for s in samples {
            self.dgram.extend_from_slice(&s.to_le_bytes());
            if self.full() {
                send(&self.dgram);
                self.seq = self.seq.wrapping_add(1);
                self.start();
            }
        }
    }

    /// Drops a partly filled packet (the stream paused); the next one continues the count.
    pub fn flush(&mut self) {
        self.start();
    }
}

/// Converts a float sample (-1..1) to s16.
pub fn f32_to_i16(s: f32) -> i16 {
    (s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct JitterStats {
    pub packets: u64,
    /// Packets that never arrived (filled with silence).
    pub lost: u64,
    /// Packets that arrived after their slot was played, or duplicates.
    pub late: u64,
    /// Times playback ran dry and waited to refill.
    pub underruns: u64,
    /// Frames dropped or repeated to follow the sender's clock.
    pub dropped_frames: u64,
    pub repeated_frames: u64,
}

/// Holds received audio until it is played, a little ahead of need so that late packets still
/// arrive in time.
///
/// How far ahead adapts: the buffer watches the lowest level it reaches over a few seconds
/// (the margin it really had against jitter) and steers it towards `margin` by dropping or
/// repeating single frames, at most one per [`pop`](Self::pop). That also follows the slow
/// drift between the sender's and the player's clocks, without audible steps. A large excess
/// (a burst, or a player that stopped pulling for a while) is dropped in one go instead.
pub struct JitterBuffer {
    channels: usize,
    samples: VecDeque<i16>,
    next_seq: Option<u32>,
    /// Frames to keep in reserve at the lowest point.
    margin: usize,
    /// Never hold more than this many frames (stale audio is worse than a gap).
    max: usize,
    /// After an underrun (or at start): play silence until this much is buffered.
    priming: bool,
    window_min: usize,
    window_frames: usize,
    /// Frames still to drop (+) or repeat (-).
    correction: i64,
    last_frame: Vec<i16>,
    pub stats: JitterStats,
}

/// How often the margin is measured and corrected.
const WINDOW_FRAMES: usize = 2 * SAMPLE_RATE as usize;
/// Gaps longer than this many packets are a restarted sender, not loss.
const MAX_GAP: u32 = 50;

impl JitterBuffer {
    /// `margin_ms`: reserve to keep against jitter, on top of whatever the jitter needs.
    pub fn new(channels: usize, margin_ms: u32) -> Self {
        let margin = (SAMPLE_RATE * margin_ms / 1000) as usize;
        Self {
            channels,
            samples: VecDeque::new(),
            next_seq: None,
            margin,
            max: margin + SAMPLE_RATE as usize / 5,
            priming: true,
            window_min: usize::MAX,
            window_frames: 0,
            correction: 0,
            last_frame: vec![0; channels],
            stats: JitterStats::default(),
        }
    }

    pub fn channels(&self) -> usize {
        self.channels
    }

    /// Buffered audio, in frames.
    pub fn level(&self) -> usize {
        self.samples.len() / self.channels
    }

    /// Forgets everything buffered (the stream stopped); stats are kept.
    pub fn reset(&mut self) {
        self.samples.clear();
        self.next_seq = None;
        self.priming = true;
        self.window_min = usize::MAX;
        self.window_frames = 0;
        self.correction = 0;
    }

    /// Adds a packet of interleaved s16le samples.
    pub fn push(&mut self, seq: u32, pcm: &[u8]) {
        let frames = pcm.len() / (2 * self.channels);
        if let Some(next) = self.next_seq {
            let d = seq.wrapping_sub(next) as i32;
            if d < 0 && d > -(MAX_GAP as i32) {
                self.stats.late += 1;
                return;
            }
            if d > 0 && (d as u32) <= MAX_GAP {
                self.stats.lost += d as u64;
                // Keep the timing: what was lost would have played here.
                if !self.priming {
                    self.samples.extend(std::iter::repeat_n(0, d as usize * frames * self.channels));
                }
            }
        }
        self.next_seq = Some(seq.wrapping_add(1));
        self.stats.packets += 1;
        self.samples.extend(pcm.as_chunks::<2>().0.iter().map(|b| i16::from_le_bytes(*b)));
        let excess = self.level().saturating_sub(self.max);
        if excess > 0 {
            self.samples.drain(..excess * self.channels);
            self.stats.dropped_frames += excess as u64;
        }
    }

    /// Fills `out` (interleaved) with the next samples, or silence where there are none.
    pub fn pop(&mut self, out: &mut [i16]) {
        let ch = self.channels;
        let frames = out.len() / ch;
        if self.priming {
            out.fill(0);
            // Start with twice the margin; the correction trims it to what the jitter needs.
            if self.level() >= frames + 2 * self.margin {
                self.priming = false;
                self.window_min = usize::MAX;
                self.window_frames = 0;
                self.correction = 0;
            }
            return;
        }
        if self.level() < frames {
            let n = self.samples.len();
            for (o, s) in out.iter_mut().zip(self.samples.drain(..)) {
                *o = s;
            }
            out[n..].fill(0);
            self.priming = true;
            self.stats.underruns += 1;
            return;
        }

        if self.correction > 0 && self.level() > frames {
            self.samples.drain(..ch);
            self.correction -= 1;
            self.stats.dropped_frames += 1;
        }
        let repeat = self.correction < 0 && frames > 1;
        let take = if repeat { frames - 1 } else { frames };
        for (o, s) in out.iter_mut().zip(self.samples.drain(..take * ch)) {
            *o = s;
        }
        if frames > 0 {
            self.last_frame.copy_from_slice(&out[(take - 1) * ch..take * ch]);
        }
        if repeat {
            out[take * ch..frames * ch].copy_from_slice(&self.last_frame);
            self.correction += 1;
            self.stats.repeated_frames += 1;
        }

        self.window_min = self.window_min.min(self.level());
        self.window_frames += frames;
        if self.window_frames >= WINDOW_FRAMES {
            let error = self.window_min as i64 - self.margin as i64;
            if error > 4 * self.margin as i64 {
                // Far too much (a burst, or the player paused): one jump beats seconds of
                // trimming a frame at a time.
                self.samples.drain(..error as usize * ch);
                self.stats.dropped_frames += error as u64;
                self.correction = 0;
            } else {
                // A dead band of a fifth of the margin keeps it from hunting.
                self.correction = if error.unsigned_abs() as usize > self.margin / 5 { error } else { 0 };
            }
            self.window_min = usize::MAX;
            self.window_frames = 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packets(stream: Stream, samples: &[i16]) -> Vec<Vec<u8>> {
        let mut p = Packetizer::new(stream);
        let mut out = Vec::new();
        p.push(samples.iter().copied(), |d| out.push(d.to_vec()));
        out
    }

    #[test]
    fn packetize_and_parse() {
        // Two and a half packets of stereo: two datagrams, the rest held back.
        let samples: Vec<i16> = (0..(PACKET_FRAMES * 5) as i16).collect();
        let d = packets(Stream::Speakers, &samples);
        assert_eq!(d.len(), 2);
        assert!(d[0].len() <= crate::MAX_DATAGRAM);
        let p = parse(&d[1]).unwrap();
        assert_eq!((p.stream, p.seq), (Stream::Speakers, 1));
        assert_eq!(p.pcm.len(), PACKET_FRAMES * 4);
        assert_eq!(i16::from_le_bytes([p.pcm[0], p.pcm[1]]), (PACKET_FRAMES * 2) as i16);
        assert!(parse(&crate::encode(&crate::FromDriver::Heartbeat)).is_none());
    }

    /// Plays `seconds` of a sender running `ppm` fast against the player, with packets delivered
    /// in bursts, and returns the buffer.
    fn simulate(ppm: f64, seconds: f64) -> JitterBuffer {
        let mut jb = JitterBuffer::new(1, 10);
        let mut p = Packetizer::new(Stream::Microphone);
        let mut queue: Vec<Vec<u8>> = Vec::new();
        let block = 128;
        let (mut sent, mut played) = (0f64, 0usize);
        let mut out = vec![0i16; block];
        let mut value = 0i16;
        while (played as f64) < seconds * SAMPLE_RATE as f64 {
            // The sender produces at its own rate; the network delivers every 4th block in a burst.
            let due = (played as f64 + block as f64) * (1.0 + ppm * 1e-6);
            while sent < due {
                value = value.wrapping_add(1);
                p.push([value], |d| queue.push(d.to_vec()));
                sent += 1.0;
            }
            if (played / block) % 4 == 0 {
                for d in queue.drain(..) {
                    let pk = parse(&d).unwrap();
                    jb.push(pk.seq, pk.pcm);
                }
            }
            jb.pop(&mut out);
            played += block;
        }
        jb
    }

    #[test]
    fn follows_clock_drift_without_underruns() {
        for ppm in [-300.0, 0.0, 300.0] {
            let jb = simulate(ppm, 120.0);
            assert_eq!(jb.stats.underruns, 0, "{ppm} ppm: {:?}", jb.stats);
            // Latency stays bounded: about the burst size plus the margin.
            assert!(jb.level() < 2000, "{ppm} ppm: level {}", jb.level());
            // 300 ppm over 120 s is ~1700 frames to make up, not much more.
            let corrected = jb.stats.dropped_frames + jb.stats.repeated_frames;
            assert!(corrected < 4000, "{ppm} ppm: {:?}", jb.stats);
        }
    }

    #[test]
    fn fills_lost_packets_with_silence_and_ignores_late_ones() {
        let mut jb = JitterBuffer::new(1, 5);
        let pcm = vec![1u8; PACKET_FRAMES * 2];
        for seq in 0..10 {
            jb.push(seq, &pcm);
        }
        let mut out = vec![0i16; 64];
        jb.pop(&mut out); // primed, playing from now on
        let before = jb.level();
        jb.push(12, &pcm); // 10 and 11 lost
        assert_eq!(jb.level(), before + 3 * PACKET_FRAMES);
        assert_eq!(jb.stats.lost, 2);
        jb.push(11, &pcm);
        assert_eq!(jb.stats.late, 1);
        assert_eq!(jb.level(), before + 3 * PACKET_FRAMES);
    }

    #[test]
    fn underrun_reprimes() {
        let mut jb = JitterBuffer::new(2, 5);
        let pcm = vec![0u8; PACKET_FRAMES * 4];
        for seq in 0..4 {
            jb.push(seq, &pcm);
        }
        let mut out = vec![0i16; 256];
        for _ in 0..20 {
            jb.pop(&mut out);
        }
        assert_eq!(jb.stats.underruns, 1);
        assert_eq!(jb.level(), 0);
    }
}
