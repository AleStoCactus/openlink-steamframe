//! The pose each frame was really rendered with.
//!
//! SteamVR's compositor reports one pose per submitted frame (`mHmdPose`), but for its own
//! layer measurements show it is one frame stale: frame N's content was rendered with the pose
//! it reports for frame N+1. (An application's layer reports its pose correctly and is left
//! alone: correcting it made games' frames a frame ahead of their content.) The headset reprojects every frame from its reported pose, so a stale pose turns
//! head motion into world jitter.
//!
//! The head is reported to SteamVR without velocity (see `link`), so `mHmdPose` is exactly
//! one of the samples we handed over. We find it in a short history of those samples and
//! label the frame with the sample handed over one frame period later instead.

use glam::{Mat3, Quat, Vec3};
use std::collections::VecDeque;
use std::sync::Mutex;

/// (time handed to SteamVR, position, orientation), oldest first.
static HISTORY: Mutex<VecDeque<(i64, Vec3, Quat)>> = Mutex::new(VecDeque::new());
/// About 200 ms at 500 Hz.
const CAPACITY: usize = 100;
/// A reported pose further than this (radians + metres) from the sample track is not one of
/// ours (e.g. history empty): it is left alone. SteamVR's pose falls between samples, so the
/// track is interpolated.
const MATCH_TOLERANCE: f32 = 0.01;
/// Interpolation steps between neighbouring samples when matching.
const STEPS: usize = 8;

static STATS: Mutex<(u32, u32, f32)> = Mutex::new((0, 0, 0.0));

/// Records a head pose as handed to SteamVR at `now_ns`.
pub fn record(now_ns: i64, position: Vec3, orientation: Quat) {
    let mut h = HISTORY.lock().unwrap();
    h.push_back((now_ns, position, orientation));
    while h.len() > CAPACITY {
        h.pop_front();
    }
}

fn to_pose(m: &[[f32; 4]; 3]) -> (Vec3, Quat) {
    let rot = Mat3::from_cols(
        Vec3::new(m[0][0], m[1][0], m[2][0]),
        Vec3::new(m[0][1], m[1][1], m[2][1]),
        Vec3::new(m[0][2], m[1][2], m[2][2]),
    );
    (Vec3::new(m[0][3], m[1][3], m[2][3]), Quat::from_mat3(&rot).normalize())
}

fn to_matrix(p: Vec3, q: Quat) -> [[f32; 4]; 3] {
    let r = Mat3::from_quat(q);
    let (c0, c1, c2) = (r.x_axis, r.y_axis, r.z_axis);
    [[c0.x, c1.x, c2.x, p.x], [c0.y, c1.y, c2.y, p.y], [c0.z, c1.z, c2.z, p.z]]
}

/// The pose frame content was rendered with, given the pose SteamVR reported for it.
/// `period_ns` is the frame period.
pub fn correct(reported: &[[f32; 4]; 3], period_ns: i64) -> [[f32; 4]; 3] {
    let (pos, rot) = to_pose(reported);
    let h = HISTORY.lock().unwrap();
    let error = |p: Vec3, q: Quat| q.angle_between(rot) + (p - pos).length();
    // Best time on the interpolated track; later wins ties (a head holding still).
    let mut best: Option<(f32, i64)> = None;
    for (a, b) in h.iter().zip(h.iter().skip(1)) {
        for k in 0..=STEPS {
            let t = k as f32 / STEPS as f32;
            let e = error(a.1.lerp(b.1, t), a.2.slerp(b.2, t));
            if best.is_none_or(|(be, _)| e <= be + 1e-6) {
                best = Some((e, a.0 + ((b.0 - a.0) as f32 * t) as i64));
            }
        }
    }
    let mut stats = STATS.lock().unwrap();
    stats.0 += 1;
    let out = match best {
        Some((e, matched)) if e <= MATCH_TOLERANCE && period_ns > 0 => {
            let (p, q) = at(&h, matched + period_ns);
            stats.1 += 1;
            stats.2 += q.angle_between(rot).to_degrees();
            to_matrix(p, q)
        }
        _ => *reported,
    };
    if stats.0 >= 900 {
        log::info!(
            "render pose: {} of {} frames matched the sample track; corrected by {:.3} deg on average",
            stats.1,
            stats.0,
            stats.2 / stats.1.max(1) as f32
        );
        *stats = (0, 0, 0.0);
    }
    out
}

/// The sample track at `time`, interpolated; clamped to the newest sample.
fn at(h: &VecDeque<(i64, Vec3, Quat)>, time: i64) -> (Vec3, Quat) {
    for (a, b) in h.iter().zip(h.iter().skip(1)) {
        if a.0 <= time && time <= b.0 {
            let t = if b.0 > a.0 { (time - a.0) as f32 / (b.0 - a.0) as f32 } else { 0.0 };
            return (a.1.lerp(b.1, t), a.2.slerp(b.2, t));
        }
    }
    let last = h.back().unwrap();
    (last.1, last.2)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matrix_roundtrip() {
        let q = Quat::from_euler(glam::EulerRot::YXZ, 0.3, -0.2, 0.1);
        let p = Vec3::new(0.1, 1.6, -0.3);
        let (p2, q2) = to_pose(&to_matrix(p, q));
        assert!((p2 - p).length() < 1e-6 && q2.angle_between(q) < 1e-5);
    }

    #[test]
    fn labels_with_the_sample_one_period_later() {
        let period = 11_111_111;
        for i in 0..50 {
            record(i * 2_000_000, Vec3::Y, Quat::from_rotation_y(i as f32 * 0.01));
        }
        // Reported: the sample at 40 ms (i = 20). Rendered: ~51 ms later sample (i = 25 or 26).
        let reported = to_matrix(Vec3::Y, Quat::from_rotation_y(0.20));
        let (_, q) = to_pose(&correct(&reported, period));
        let yaw = q.to_euler(glam::EulerRot::YXZ).0;
        assert!((yaw - 0.26).abs() < 0.011, "corrected yaw {yaw}");
    }
}
