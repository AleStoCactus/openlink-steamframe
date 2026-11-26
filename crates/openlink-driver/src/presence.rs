//! Keeps controllers connected in SteamVR while the headset briefly stops reporting them, and
//! decides when they go to standby.
//!
//! The Quest stops reporting a controller about a second after it is put down (its OpenXR action
//! goes inactive, e.g. when the runtime switches to hand tracking). Passing that on as
//! "disconnected" made SteamVR show "controller not detected" whenever a controller rested on a
//! desk. Instead the controller stays where it was last seen, and SteamVR's standby takes over:
//! SteamVR shows a device as standby when its orientation stays nearly constant and its
//! `/proximity` input is false, so `/proximity` reports whether the controller was moved or
//! touched within [`STANDBY_AFTER`]. Only losing the headset itself disconnects the controllers.

use glam::{DQuat, DVec3};
use openlink_protocol::ControllerInput;
use openlink_tracking::DriverPoseState;
use std::time::{Duration, Instant};

/// A controller neither moved nor touched for this long goes to standby.
pub const STANDBY_AFTER: Duration = Duration::from_secs(15);
/// Movement smaller than this is tracking noise on a resting controller (Quest: well under 1 mm
/// and 0.1° at rest).
const MOVED_M: f64 = 0.01;
const MOVED_RAD: f64 = 2.0 * std::f64::consts::PI / 180.0;
/// Trigger or grip pressure above this counts as the controller being held.
const PRESSED: f32 = 0.05;

#[derive(Default)]
pub struct Presence {
    /// The last valid pose, held while the headset reports none.
    last: Option<DriverPoseState>,
    /// Where the controller was when it last moved, and when that was.
    anchor: Option<(DVec3, DQuat, Instant)>,
}

impl Presence {
    /// A tracked sample arrived (`pose` before any prediction, in SteamVR's frame).
    pub fn seen(&mut self, pose: &DriverPoseState, input: &ControllerInput, now: Instant) {
        if !pose.valid {
            return;
        }
        self.last = Some(*pose);
        let moved = match self.anchor {
            Some((p, q, _)) => p.distance(pose.position) > MOVED_M || q.angle_between(pose.rotation) > MOVED_RAD,
            None => true,
        };
        if moved || touched(input) {
            self.anchor = Some((pose.position, pose.rotation, now));
        }
    }

    /// The pose to report while the headset sends none: the last one, at rest. `None` if the
    /// controller has not been seen since the headset connected.
    pub fn held(&self) -> Option<DriverPoseState> {
        self.last.map(|p| DriverPoseState {
            velocity: DVec3::ZERO,
            angular_velocity: DVec3::ZERO,
            time_offset_s: 0.0,
            valid: true,
            tracked: true,
            ..p
        })
    }

    /// The `/proximity` input: false once the controller has rested untouched for
    /// [`STANDBY_AFTER`].
    pub fn in_use(&self, now: Instant) -> bool {
        self.anchor.is_some_and(|(_, _, t)| now.duration_since(t) < STANDBY_AFTER)
    }

    /// The headset is gone: the controllers go with it.
    pub fn reset(&mut self) {
        *self = Self::default();
    }
}

/// Any touch sensor, button or analog input: someone is holding the controller.
fn touched(i: &ControllerInput) -> bool {
    i.buttons != 0 || i.trigger > PRESSED || i.grip > PRESSED || i.thumbstick != [0.0, 0.0]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pose(x: f64, yaw_deg: f64) -> DriverPoseState {
        DriverPoseState {
            position: DVec3::new(x, 1.0, 0.0),
            rotation: DQuat::from_rotation_y(yaw_deg.to_radians()),
            velocity: DVec3::X,
            angular_velocity: DVec3::Y,
            time_offset_s: -0.01,
            valid: true,
            tracked: true,
        }
    }

    #[test]
    fn rests_into_standby_and_wakes_on_movement() {
        let t0 = Instant::now();
        let idle = ControllerInput::default();
        let mut p = Presence::default();
        assert!(!p.in_use(t0));
        assert!(p.held().is_none());

        p.seen(&pose(0.0, 0.0), &idle, t0);
        // Noise while resting does not count as movement.
        p.seen(&pose(0.001, 0.05), &idle, t0 + Duration::from_secs(10));
        assert!(p.in_use(t0 + Duration::from_secs(14)));
        assert!(!p.in_use(t0 + STANDBY_AFTER));

        p.seen(&pose(0.05, 0.0), &idle, t0 + Duration::from_secs(20));
        assert!(p.in_use(t0 + Duration::from_secs(21)));
        p.seen(&pose(0.05, 5.0), &idle, t0 + Duration::from_secs(40));
        assert!(p.in_use(t0 + Duration::from_secs(41)));
    }

    #[test]
    fn touch_keeps_it_awake() {
        let t0 = Instant::now();
        let mut p = Presence::default();
        p.seen(&pose(0.0, 0.0), &ControllerInput::default(), t0);
        let thumb = ControllerInput { buttons: 1, ..Default::default() };
        p.seen(&pose(0.0, 0.0), &thumb, t0 + Duration::from_secs(30));
        assert!(p.in_use(t0 + Duration::from_secs(31)));
    }

    #[test]
    fn holds_the_last_pose_at_rest() {
        let mut p = Presence::default();
        p.seen(&pose(0.3, 10.0), &ControllerInput::default(), Instant::now());
        let mut invalid = pose(9.0, 0.0);
        invalid.valid = false;
        p.seen(&invalid, &ControllerInput::default(), Instant::now());
        let h = p.held().unwrap();
        assert_eq!(h.position, DVec3::new(0.3, 1.0, 0.0));
        assert_eq!((h.velocity, h.angular_velocity, h.time_offset_s), (DVec3::ZERO, DVec3::ZERO, 0.0));
        assert!(h.valid && h.tracked);
        p.reset();
        assert!(p.held().is_none());
    }
}
