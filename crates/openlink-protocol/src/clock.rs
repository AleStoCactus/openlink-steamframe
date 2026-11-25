//! Headset <-> PC clock synchronisation.
//!
//! Poses are only useful to SteamVR if we know *when* they were true in PC time, so we estimate
//! `offset = headset_clock - pc_clock` NTP-style. Wi-Fi latency is very asymmetric and spiky,
//! so instead of averaging we keep a sliding window and trust the samples with the lowest
//! round-trip time (they had the least queueing, so their midpoint estimate is the tightest).

use std::collections::VecDeque;

const WARMUP_SAMPLES: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ClockSample {
    /// Estimated `headset - pc`, ns.
    pub offset_ns: i64,
    pub rtt_ns: i64,
}

impl ClockSample {
    /// `t0` PC send, `t1` headset receive, `t2` headset send, `t3` PC receive.
    pub fn from_exchange(t0: i64, t1: i64, t2: i64, t3: i64) -> Option<Self> {
        let rtt_ns = (t3 - t0) - (t2 - t1);
        if rtt_ns < 0 || t3 < t0 || t2 < t1 {
            return None;
        }
        let offset_ns = ((t1 - t0) + (t2 - t3)) / 2;
        Some(Self { offset_ns, rtt_ns })
    }
}

#[derive(Debug)]
pub struct ClockSync {
    window: VecDeque<ClockSample>,
    capacity: usize,
    estimate: Option<i64>,
}

impl Default for ClockSync {
    fn default() -> Self {
        Self::new(64)
    }
}

impl ClockSync {
    pub fn new(capacity: usize) -> Self {
        Self {
            window: VecDeque::with_capacity(capacity),
            capacity: capacity.max(1),
            estimate: None,
        }
    }

    pub fn push(&mut self, sample: ClockSample) {
        if self.window.len() == self.capacity {
            self.window.pop_front();
        }
        self.window.push_back(sample);

        // Median of the offsets of the lowest-RTT quarter of the window.
        let mut best: Vec<ClockSample> = self.window.iter().copied().collect();
        best.sort_by_key(|s| s.rtt_ns);
        best.truncate((best.len() / 4).max(1));
        let mut offsets: Vec<i64> = best.iter().map(|s| s.offset_ns).collect();
        offsets.sort_unstable();
        let target = offsets[offsets.len() / 2];

        // Step while warming up (the first exchanges may well be spiky), then slew so pose
        // timestamps never jump mid-session.
        self.estimate = Some(match self.estimate {
            Some(prev) if self.window.len() >= WARMUP_SAMPLES => prev + (target - prev) / 8,
            _ => target,
        });
    }

    /// `headset - pc` in ns, once at least one exchange has completed.
    pub fn offset_ns(&self) -> Option<i64> {
        self.estimate
    }

    pub fn min_rtt_ns(&self) -> Option<i64> {
        self.window.iter().map(|s| s.rtt_ns).min()
    }

    pub fn headset_to_pc(&self, headset_ns: i64) -> Option<i64> {
        self.estimate.map(|o| headset_ns - o)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn symmetric_exchange_recovers_offset() {
        // headset is 5 s ahead, 2 ms each way, 100 us processing.
        let off = 5_000_000_000;
        let t0 = 1_000;
        let t1 = t0 + 2_000_000 + off;
        let t2 = t1 + 100_000;
        let t3 = t2 - off + 2_000_000;
        let s = ClockSample::from_exchange(t0, t1, t2, t3).unwrap();
        assert_eq!(s.offset_ns, off);
        assert_eq!(s.rtt_ns, 4_000_000);
    }

    #[test]
    fn spiky_samples_do_not_drag_estimate() {
        let mut sync = ClockSync::new(32);
        for i in 0..32 {
            // Every third exchange hits a 40 ms one-way Wi-Fi spike on the return path.
            let spike = if i % 3 == 0 { 40_000_000 } else { 0 };
            let t0 = i * 10_000_000;
            let t1 = t0 + 1_500_000 + 777;
            let t2 = t1 + 50_000;
            let t3 = t2 - 777 + 1_500_000 + spike;
            sync.push(ClockSample::from_exchange(t0, t1, t2, t3).unwrap());
        }
        assert!((sync.offset_ns().unwrap() - 777).abs() < 1_000);
    }
}
