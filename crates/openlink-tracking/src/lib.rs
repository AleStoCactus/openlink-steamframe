//! Tracking policy: how headset samples become SteamVR `DriverPose_t`s.
//!
//! # Why controllers "flop around" in other streamers
//!
//! A standalone OpenXR app asks the Quest runtime for controller poses *predicted to the display
//! time of the frame it is about to render*. Streamers that copy that model (VD, ALVR) ask the
//! runtime to predict 40-80 ms into the future to cover encode + network + decode. The runtime's
//! controller extrapolator is tuned for ~15 ms; stretched that far, every fast swing overshoots
//! and snaps back.
//!
//! # What OpenLink does instead (the Link / Steam Link / Lighthouse model)
//!
//! 1. The headset samples every device at **`now`** (zero prediction) on a dedicated
//!    high-rate thread and sends pose + linear/angular velocity + the sample timestamp.
//! 2. The server converts the timestamp into PC time using [`openlink_protocol::clock`].
//! 3. The driver hands SteamVR the *measured* state with `poseTimeOffset = sample - now`
//!    (negative: "this pose is N ms old") and SteamVR's own predictor carries it to the exact
//!    photon time it needs, exactly as it does for Index controllers.
//!
//! The head pose works the same way. Any residual head error is removed on the headset by
//! reprojecting each frame from the pose it was rendered with to the actual display pose.

use glam::{DQuat, DVec3, EulerRot};
use openlink_protocol::{Hand, Motion, MotionFlags};

/// A rigid transform.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Isometry {
    pub translation: DVec3,
    pub rotation: DQuat,
}

impl Isometry {
    pub const IDENTITY: Self = Self {
        translation: DVec3::ZERO,
        rotation: DQuat::IDENTITY,
    };

    /// From a SteamVR render model `component_local` entry (`origin` in metres, `rotate_xyz` in
    /// degrees, X applied first).
    pub fn from_render_model(origin: [f64; 3], rotate_xyz_deg: [f64; 3]) -> Self {
        let [x, y, z] = rotate_xyz_deg.map(f64::to_radians);
        Self {
            translation: DVec3::from(origin),
            rotation: DQuat::from_euler(EulerRot::ZYX, z, y, x),
        }
    }

    pub fn inverse(self) -> Self {
        let rotation = self.rotation.inverse();
        Self {
            translation: -(rotation * self.translation),
            rotation,
        }
    }
}

impl std::ops::Mul for Isometry {
    type Output = Isometry;

    /// `self * rhs`: apply `rhs` first, then `self`.
    fn mul(self, rhs: Self) -> Self {
        Self {
            translation: self.translation + self.rotation * rhs.translation,
            rotation: (self.rotation * rhs.rotation).normalize(),
        }
    }
}

/// Which frame SteamVR interprets `DriverPose_t::vecAngularVelocity` in.
///
/// The OpenVR headers do not say. Measured (SteamVR 2.19, `mock-headset --spin-tilted`, reading
/// SteamVR's own prediction back with `GetRawTrackedDevicePoses`): **local**. SteamVR's 100 ms
/// prediction matched `q * exp(w dt)` to 0.02 deg and missed `exp(w dt) * q` by 37 deg. With
/// world-space velocities every swing was predicted about the wrong axis, so a controller
/// swung from its butt end moved as if shoved rather than rotated about the hand.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AngularVelocitySpace {
    World,
    #[default]
    Local,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PredictionConfig {
    pub angular_velocity_space: AngularVelocitySpace,
    /// Samples older than this are considered stale: velocities are zeroed so SteamVR holds the
    /// last pose rather than extrapolating a controller across the room during a Wi-Fi stall.
    pub stale_after_s: f64,
}

impl Default for PredictionConfig {
    fn default() -> Self {
        Self {
            angular_velocity_space: AngularVelocitySpace::Local,
            stale_after_s: 0.1,
        }
    }
}

/// Everything SteamVR needs for one `DriverPose_t`, in driver space.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DriverPoseState {
    pub position: DVec3,
    pub rotation: DQuat,
    pub velocity: DVec3,
    pub angular_velocity: DVec3,
    /// `sample_time - now`, seconds; normally slightly negative.
    pub time_offset_s: f64,
    pub valid: bool,
    pub tracked: bool,
}

/// Convert a headset sample into SteamVR state.
///
/// * `local_offset` is the device-frame transform from the reported frame to the frame SteamVR
///   expects (see [`controller_raw_from_grip`]); `None` for the HMD.
/// * `sample_pc_ns` / `now_pc_ns` are both on the PC monotonic clock.
pub fn to_driver_pose(
    motion: &Motion,
    local_offset: Option<Isometry>,
    sample_pc_ns: i64,
    now_pc_ns: i64,
    cfg: &PredictionConfig,
) -> DriverPoseState {
    let p = motion.pose;
    let mut position = DVec3::from(p.position.map(f64::from));
    let mut rotation = DQuat::from_array(p.orientation.map(f64::from)).normalize();
    let mut velocity = DVec3::from(motion.linear_velocity.map(f64::from));
    let omega_world = DVec3::from(motion.angular_velocity.map(f64::from));

    if !motion.flags.has(MotionFlags::LINEAR_VELOCITY_VALID) {
        velocity = DVec3::ZERO;
    }
    let mut omega_world = if motion.flags.has(MotionFlags::ANGULAR_VELOCITY_VALID) {
        omega_world
    } else {
        DVec3::ZERO
    };

    if let Some(off) = local_offset {
        let world = Isometry {
            translation: position,
            rotation,
        } * off;
        // A point rigidly attached at lever arm r moves with v + w x r.
        velocity += omega_world.cross(world.translation - position);
        position = world.translation;
        rotation = world.rotation;
    }

    let time_offset_s = (sample_pc_ns - now_pc_ns) as f64 * 1e-9;
    if -time_offset_s > cfg.stale_after_s {
        velocity = DVec3::ZERO;
        omega_world = DVec3::ZERO;
    }

    let angular_velocity = match cfg.angular_velocity_space {
        AngularVelocitySpace::World => omega_world,
        AngularVelocitySpace::Local => rotation.inverse() * omega_world,
    };

    DriverPoseState {
        position,
        rotation,
        velocity,
        angular_velocity,
        time_offset_s,
        valid: motion.flags.is_valid(),
        tracked: motion.flags.is_tracked(),
    }
}

/// SteamVR controller model a headset's controllers should appear as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControllerModel {
    /// Quest 2 Touch.
    Quest2,
    /// Quest 3 / 3S Touch Plus.
    QuestPlus,
    /// Touch Pro.
    QuestPro,
}

impl ControllerModel {
    pub fn render_model(self, hand: Hand) -> &'static str {
        match (self, hand) {
            (Self::Quest2, Hand::Left) => "oculus_quest2_controller_left",
            (Self::Quest2, Hand::Right) => "oculus_quest2_controller_right",
            (Self::QuestPlus, Hand::Left) => "oculus_quest_plus_controller_left",
            (Self::QuestPlus, Hand::Right) => "oculus_quest_plus_controller_right",
            (Self::QuestPro, Hand::Left) => "oculus_quest_pro_controller_left",
            (Self::QuestPro, Hand::Right) => "oculus_quest_pro_controller_right",
        }
    }

    pub fn from_headset_model(model: &str) -> Self {
        let m = model.to_ascii_lowercase();
        if m.contains("pro") {
            Self::QuestPro
        } else if m.contains("quest 3") || m.contains("quest3") || m.contains("steam frame") {
            // The Steam Frame's controllers reach the PC as Touch (SteamVR on the headset maps
            // them); the current Touch Plus model is the closest match.
            Self::QuestPlus
        } else {
            Self::Quest2
        }
    }
}

/// Transform from the OpenXR grip pose to the controller's SteamVR "raw" pose.
///
/// We report controllers as `oculus_touch` devices so every game's existing Touch bindings and
/// pose offsets apply unchanged. Those offsets are relative to the *raw* pose. SteamVR's own
/// render models record where `/pose/openxr_grip` sits relative to raw (the `openxr_grip`
/// component), identical for Quest 2, Touch Plus and Touch Pro:
///
/// ```text
/// origin [±0.007, -0.00182941, 0.1019482], rotate_xyz [20.6, 0, 0]   (left +, right -)
/// ```
///
/// so `world_from_raw = world_from_grip * inverse(raw_from_grip)`.
pub fn controller_raw_from_grip(_model: ControllerModel, hand: Hand) -> Isometry {
    let x = match hand {
        Hand::Left => 0.007,
        Hand::Right => -0.007,
    };
    Isometry::from_render_model([x, -0.001_829_41, 0.101_948_2], [20.6, 0.0, 0.0]).inverse()
}

#[cfg(test)]
mod tests {
    use super::*;
    use openlink_protocol::Pose;

    fn motion(pos: [f32; 3], rot: DQuat, v: [f32; 3], w: [f32; 3]) -> Motion {
        let q = rot.as_quat();
        Motion {
            pose: Pose {
                position: pos,
                orientation: [q.x, q.y, q.z, q.w],
            },
            linear_velocity: v,
            angular_velocity: w,
            flags: MotionFlags::ALL,
        }
    }

    #[test]
    fn controller_models_follow_the_headset() {
        assert_eq!(ControllerModel::from_headset_model("Meta Quest Pro"), ControllerModel::QuestPro);
        assert_eq!(ControllerModel::from_headset_model("Meta Quest 3S"), ControllerModel::QuestPlus);
        assert_eq!(ControllerModel::from_headset_model("Steam Frame"), ControllerModel::QuestPlus);
        assert_eq!(ControllerModel::from_headset_model("Oculus Quest2"), ControllerModel::Quest2);
    }

    #[test]
    fn hmd_passes_through_with_negative_age() {
        let m = motion(
            [0.0, 1.7, 0.0],
            DQuat::IDENTITY,
            [0.5, 0.0, 0.0],
            [0.0, 1.0, 0.0],
        );
        let s = to_driver_pose(
            &m,
            None,
            1_000_000_000,
            1_004_000_000,
            &PredictionConfig::default(),
        );
        assert!((s.time_offset_s + 0.004).abs() < 1e-12);
        assert!((s.position - DVec3::new(0.0, 1.7, 0.0)).length() < 1e-6);
        assert!((s.velocity - DVec3::new(0.5, 0.0, 0.0)).length() < 1e-6);
    }

    #[test]
    fn grip_to_raw_round_trips_through_render_model_component() {
        let grip_in_raw =
            Isometry::from_render_model([0.007, -0.00182941, 0.1019482], [20.6, 0.0, 0.0]);
        let world_from_grip = Isometry {
            translation: DVec3::new(0.3, 1.2, -0.4),
            rotation: DQuat::from_euler(EulerRot::YXZ, 0.7, -0.3, 0.2),
        };
        let off = controller_raw_from_grip(ControllerModel::QuestPlus, Hand::Left);
        let world_from_raw = world_from_grip * off;
        // Re-applying the render model component must land back on the grip pose.
        let back = world_from_raw * grip_in_raw;
        assert!((back.translation - world_from_grip.translation).length() < 1e-9);
        assert!(back.rotation.angle_between(world_from_grip.rotation) < 1e-9);
    }

    #[test]
    fn lever_arm_adds_tangential_velocity() {
        // Spinning about +Y at 10 rad/s; raw origin sits ~10 cm from the grip.
        let m = motion([0.0, 1.0, 0.0], DQuat::IDENTITY, [0.0; 3], [0.0, 10.0, 0.0]);
        let off = controller_raw_from_grip(ControllerModel::QuestPlus, Hand::Right);
        let s = to_driver_pose(&m, Some(off), 0, 0, &PredictionConfig::default());
        let r = s.position - DVec3::new(0.0, 1.0, 0.0);
        let expected = DVec3::new(0.0, 10.0, 0.0).cross(r);
        assert!((s.velocity - expected).length() < 1e-6);
        assert!(s.velocity.length() > 0.5);
    }

    #[test]
    fn stale_samples_stop_extrapolating() {
        let m = motion([0.0; 3], DQuat::IDENTITY, [3.0, 0.0, 0.0], [0.0, 20.0, 0.0]);
        let s = to_driver_pose(&m, None, 0, 500_000_000, &PredictionConfig::default());
        assert_eq!(s.velocity, DVec3::ZERO);
        assert_eq!(s.angular_velocity, DVec3::ZERO);
    }

    #[test]
    fn local_angular_velocity_is_rotated_into_body_frame() {
        let rot = DQuat::from_rotation_x(std::f64::consts::FRAC_PI_2);
        let m = motion([0.0; 3], rot, [0.0; 3], [0.0, 1.0, 0.0]);
        let cfg = PredictionConfig {
            angular_velocity_space: AngularVelocitySpace::Local,
            ..Default::default()
        };
        let s = to_driver_pose(&m, None, 0, 0, &cfg);
        assert!((s.rotation * s.angular_velocity - DVec3::new(0.0, 1.0, 0.0)).length() < 1e-9);
    }
}
