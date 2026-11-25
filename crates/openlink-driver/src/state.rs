//! Process-wide driver state shared between SteamVR's threads and our link thread.

use crate::ovr::{
    k_unTrackedDeviceIndexInvalid, HmdRect2_t, HmdVector2_t, VRInputComponentHandle_t,
};
use openlink_protocol::{Fov, HeadsetInfo};
use openlink_tracking::{ControllerModel, PredictionConfig};
use std::sync::atomic::AtomicU32;
use std::sync::{Mutex, OnceLock};

#[derive(Debug, Clone)]
pub struct DisplayConfig {
    pub model: String,
    pub eye_width: u32,
    pub eye_height: u32,
    pub refresh_rate: f32,
    pub ipd_m: f32,
    pub fov: [Fov; 2],
}

impl Default for DisplayConfig {
    /// Rough Quest 3 numbers used until a headset reports its real ones; SteamVR is updated
    /// at runtime when it does.
    fn default() -> Self {
        let left = Fov {
            angle_left: -0.90,
            angle_right: 0.75,
            angle_up: 0.82,
            angle_down: -0.88,
        };
        let right = Fov {
            angle_left: -left.angle_right,
            angle_right: -left.angle_left,
            ..left
        };
        Self {
            model: "Meta Quest 3".into(),
            eye_width: 2064,
            eye_height: 2208,
            refresh_rate: 90.0,
            ipd_m: 0.063,
            fov: [left, right],
        }
    }
}

impl DisplayConfig {
    pub fn from_headset(info: &HeadsetInfo) -> Self {
        Self {
            model: info.model.clone(),
            eye_width: info.eye_width,
            eye_height: info.eye_height,
            refresh_rate: info.current_refresh_rate,
            ipd_m: info.ipd_m,
            fov: info.fov,
        }
    }

    /// OpenVR raw projection: tangents, with *top negative* (OpenVR's Y points down here).
    pub fn projection(&self, eye: usize) -> HmdRect2_t {
        let f = self.fov[eye];
        HmdRect2_t {
            vTopLeft: HmdVector2_t {
                v: [f.angle_left.tan(), -f.angle_up.tan()],
            },
            vBottomRight: HmdVector2_t {
                v: [f.angle_right.tan(), -f.angle_down.tan()],
            },
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct ControllerHandles {
    pub ax_click: VRInputComponentHandle_t,
    pub ax_touch: VRInputComponentHandle_t,
    pub by_click: VRInputComponentHandle_t,
    pub by_touch: VRInputComponentHandle_t,
    pub menu_click: Option<VRInputComponentHandle_t>,
    pub stick_click: VRInputComponentHandle_t,
    pub stick_touch: VRInputComponentHandle_t,
    pub stick_x: VRInputComponentHandle_t,
    pub stick_y: VRInputComponentHandle_t,
    pub trigger: VRInputComponentHandle_t,
    pub trigger_touch: VRInputComponentHandle_t,
    pub grip: VRInputComponentHandle_t,
    pub grip_touch: VRInputComponentHandle_t,
    pub thumbrest_touch: VRInputComponentHandle_t,
    pub haptic: VRInputComponentHandle_t,
}

pub struct ControllerSlot {
    pub id: AtomicU32,
    pub handles: Mutex<Option<ControllerHandles>>,
}

pub struct Shared {
    pub hmd_id: AtomicU32,
    pub controllers: [ControllerSlot; 2],
    pub display: Mutex<DisplayConfig>,
    pub controller_model: Mutex<ControllerModel>,
    pub prediction: PredictionConfig,
}

static SHARED: OnceLock<Shared> = OnceLock::new();

pub fn shared() -> &'static Shared {
    SHARED.get_or_init(|| {
        let slot = || ControllerSlot {
            id: AtomicU32::new(k_unTrackedDeviceIndexInvalid),
            handles: Mutex::new(None),
        };
        Shared {
            hmd_id: AtomicU32::new(k_unTrackedDeviceIndexInvalid),
            controllers: [slot(), slot()],
            display: Mutex::new(DisplayConfig::default()),
            controller_model: Mutex::new(ControllerModel::QuestPlus),
            prediction: PredictionConfig::default(),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn projection_follows_openvr_sign_convention() {
        let r = DisplayConfig::default().projection(0);
        assert!(r.vTopLeft.v[0] < 0.0 && r.vBottomRight.v[0] > 0.0);
        assert!(r.vTopLeft.v[1] < 0.0 && r.vBottomRight.v[1] > 0.0);
    }
}
