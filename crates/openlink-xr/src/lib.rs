//! OpenXR pieces shared by the headset apps (Quest on Android, Steam Frame on Linux): tracking
//! sampled at "now" with input and haptics, time conversion, and the statistics that pace
//! SteamVR to the headset's display and calibrate head prediction.
//!
//! Nothing here depends on the graphics API; each app brings its own swapchains and renderer.
#![cfg(any(target_os = "android", target_os = "linux"))]

pub mod tracking;

use glam::{Quat, Vec3};
use openlink_client::ClientState;
use openlink_protocol::Pose;
use openxr as xr;
use std::sync::{Arc, Mutex};

/// `XrTime` -> `CLOCK_MONOTONIC` nanoseconds (the clock the protocol uses).
pub fn monotonic_ns(instance: &xr::Instance, t: xr::Time) -> i64 {
    let Some(ext) = instance.exts().khr_convert_timespec_time else { return t.as_nanos() };
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: valid instance and out pointer.
    let r = unsafe { (ext.convert_time_to_timespec_time)(instance.as_raw(), t, &mut ts) };
    if r.into_raw() < 0 {
        return t.as_nanos();
    }
    ts.tv_sec * 1_000_000_000 + ts.tv_nsec
}

/// Asks for sustained high CPU and GPU clocks. The app's own load is light, so by default the
/// runtime lowered them (CPU often at 1.4 GHz), and the decoder and the threads feeding it slowed
/// and grew jittery: decode time rose from 6 to 10 ms and pictures started missing frames.
pub fn request_sustained_performance<G: xr::Graphics>(instance: &xr::Instance, session: &xr::Session<G>) {
    let Some(ext) = instance.exts().ext_performance_settings else {
        log::warn!("XR_EXT_performance_settings unavailable; clocks stay dynamic");
        return;
    };
    for domain in [xr::PerfSettingsDomainEXT::CPU, xr::PerfSettingsDomainEXT::GPU] {
        // SAFETY: valid session and enum values.
        let r = unsafe {
            (ext.perf_settings_set_performance_level)(session.as_raw(), domain, xr::PerfSettingsLevelEXT::SUSTAINED_HIGH)
        };
        if r.into_raw() < 0 {
            log::warn!("cannot raise {domain:?} performance level: {r:?}");
        }
    }
    log::info!("requested sustained high CPU and GPU performance");
}

pub fn current_rate<G: xr::Graphics>(session: &xr::Session<G>, ext: bool) -> f32 {
    if ext {
        session.get_display_refresh_rate().unwrap_or(72.0)
    } else {
        72.0
    }
}

/// SteamVR renders each eye at the head pose offset by half the IPD along the head's X axis.
pub fn eye_poses(head: &Pose, ipd: f32) -> [xr::Posef; 2] {
    let q = Quat::from_array(head.orientation);
    let p = Vec3::from_array(head.position);
    [-0.5f32, 0.5].map(|side| {
        let e = p + q * Vec3::new(side * ipd, 0.0, 0.0);
        xr::Posef {
            orientation: xr::Quaternionf { x: q.x, y: q.y, z: q.z, w: q.w },
            position: xr::Vector3f { x: e.x, y: e.y, z: e.z },
        }
    })
}

/// Logged every few seconds: what reached the screen, and what the network lost.
pub struct Stats {
    pub since: std::time::Instant,
    pub displayed: u32,
    pub new_pictures: u32,
    /// Per new picture: degrees and millimetres between the pose it was rendered for and the
    /// head pose when it is displayed (what reprojection has to make up).
    pub errors: Vec<(f32, f32)>,
    /// Per new picture with the head turning: how far ahead (ms, + = too far) of the actual
    /// head pose the render pose was, along the motion. Calibrates SteamVR's prediction.
    pub lead_ms: Vec<f32>,
    /// Per shown picture: ms between decode finishing and the render loop picking it up.
    pub slack_ms: Vec<f32>,
    /// Display frame period (ms), for the phase of the slack.
    pub period_ms: f32,
}

impl Default for Stats {
    fn default() -> Self {
        Self { since: std::time::Instant::now(), displayed: 0, new_pictures: 0, errors: Vec::new(), lead_ms: Vec::new(), slack_ms: Vec::new(), period_ms: 11.1 }
    }
}

impl Stats {
    pub fn pose_error(&mut self, rendered: &Pose, actual: &xr::Posef, omega: &xr::Vector3f) {
        let a = Quat::from_array(rendered.orientation);
        let o = actual.orientation;
        let b = Quat::from_xyzw(o.x, o.y, o.z, o.w);
        let degrees = a.angle_between(b).to_degrees();
        // World-frame rotation from the actual pose to the rendered one, as a rotation vector.
        let (axis, angle) = (a * b.inverse()).normalize().to_axis_angle();
        let angle = if angle > std::f32::consts::PI { angle - std::f32::consts::TAU } else { angle };
        let w = Vec3::new(omega.x, omega.y, omega.z);
        if w.length() > 30f32.to_radians() {
            self.lead_ms.push((axis * angle).dot(w) / w.length_squared() * 1e3);
        }
        let p = actual.position;
        let mm = (Vec3::from_array(rendered.position) - Vec3::new(p.x, p.y, p.z)).length() * 1e3;
        self.errors.push((degrees, mm));
    }

    /// Every few seconds: logs, and returns a vsync shift (ns) that moves pictures' arrival
    /// to `TARGET_SLACK_MS` before they are needed.
    pub fn report(&mut self, client: Option<&Arc<Mutex<ClientState>>>) -> Option<i64> {
        const TARGET_SLACK_MS: f32 = 6.0;
        let elapsed = self.since.elapsed().as_secs_f32();
        if elapsed < 5.0 {
            return None;
        }
        // Slack is circular in the frame period: 0.5 ms and 10.6 ms (at 11.1) are the same
        // phase, right at the boundary. Its circular mean is where pictures arrive.
        let mut shift = None;
        let p = self.period_ms.max(1.0);
        if self.slack_ms.len() >= 20 {
            let (mut sx, mut sy) = (0.0f32, 0.0f32);
            for s in &self.slack_ms {
                let a = s / p * std::f32::consts::TAU;
                sx += a.cos();
                sy += a.sin();
            }
            let mean = (sy.atan2(sx) / std::f32::consts::TAU * p).rem_euclid(p);
            let mut error = mean - TARGET_SLACK_MS;
            if error > p / 2.0 {
                error -= p;
            } else if error < -p / 2.0 {
                error += p;
            }
            log::info!("picture phase: arrive {mean:.1} ms before pickup (target {TARGET_SLACK_MS}); shifting vsync by {:.1} ms", error * 0.5);
            shift = Some((error * 0.5 * 1e6) as i64);
        }
        if !self.errors.is_empty() {
            let pct = |v: &mut Vec<f32>, q: f32| {
                v.sort_by(f32::total_cmp);
                v[((v.len() - 1) as f32 * q) as usize]
            };
            let mut deg: Vec<f32> = self.errors.iter().map(|e| e.0).collect();
            let mut mm: Vec<f32> = self.errors.iter().map(|e| e.1).collect();
            log::info!(
                "render pose vs head at display: {:.2} deg median, {:.2} p95, {:.2} max; {:.1} mm median, {:.1} p95",
                pct(&mut deg, 0.5),
                pct(&mut deg, 0.95),
                pct(&mut deg, 1.0),
                pct(&mut mm, 0.5),
                pct(&mut mm, 0.95)
            );
        }
        if self.slack_ms.len() >= 20 {
            let mut v = std::mem::take(&mut self.slack_ms);
            v.sort_by(f32::total_cmp);
            let q = |f: f32| v[((v.len() - 1) as f32 * f) as usize];
            log::info!(
                "picture slack (decoded -> picked up): p5 {:.1} ms, p25 {:.1}, median {:.1}, p75 {:.1}, p95 {:.1}",
                q(0.05),
                q(0.25),
                q(0.5),
                q(0.75),
                q(0.95)
            );
        }
        if self.lead_ms.len() >= 20 {
            let mut v = std::mem::take(&mut self.lead_ms);
            v.sort_by(f32::total_cmp);
            let median = v[v.len() / 2];
            // Close the loop on a slight under-prediction: falling short is corrected by the
            // display's reprojection, overshooting magnifies every small head movement.
            const TARGET_LEAD_MS: f32 = -8.0;
            let old = tracking::HEAD_HORIZON_US.load(std::sync::atomic::Ordering::Relaxed);
            let new = (old as f32 - (median - TARGET_LEAD_MS) * 1000.0 * 0.5).clamp(0.0, 100_000.0) as i32;
            tracking::HEAD_HORIZON_US.store(new, std::sync::atomic::Ordering::Relaxed);
            log::info!(
                "head prediction lead over {} turning frames: {:.1} ms median (p10 {:.1}, p90 {:.1}); + = predicts too far; horizon {:.1} -> {:.1} ms",
                v.len(),
                median,
                v[v.len() / 10],
                v[v.len() * 9 / 10],
                old as f32 / 1000.0,
                new as f32 / 1000.0
            );
        }
        if let Some(c) = client {
            let c = c.lock().unwrap();
            log::info!(
                "display {:.1} fps, new pictures {:.1} fps; video total {} frames, {} lost, {} dropped waiting for IDR, {} decoder overflow",
                self.displayed as f32 / elapsed,
                self.new_pictures as f32 / elapsed,
                c.video.frames,
                c.video.lost_frames,
                c.video.dropped_frames,
                c.video_overflow
            );
        }
        let period_ms = self.period_ms;
        *self = Self { period_ms, ..Self::default() };
        shift
    }
}
