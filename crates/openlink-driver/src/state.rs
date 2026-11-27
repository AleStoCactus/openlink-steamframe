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

    /// Whether SteamVR, started with `self`, renders correctly for a headset that wants `other`.
    ///
    /// SteamVR reads the render target size and projection once, when its compositor starts:
    /// `SetRecommendedRenderTargetSize` and `SetDisplayProjectionRaw` at runtime are accepted
    /// but the compositor keeps its swap textures and frustum (seen on Linux: textures stayed
    /// at the placeholder's 2876x3080 after a Quest 3 reported 1680x1760). Games then render
    /// another FOV and aspect than the headset shows them with: severe, nauseating distortion.
    /// A small FOV tolerance absorbs float noise between sessions of the same headset.
    pub fn renders_like(&self, other: &Self) -> bool {
        const FOV_TOLERANCE_RAD: f32 = 0.005;
        let close = |a: &Fov, b: &Fov| {
            [
                a.angle_left - b.angle_left,
                a.angle_right - b.angle_right,
                a.angle_up - b.angle_up,
                a.angle_down - b.angle_down,
            ]
            .iter()
            .all(|d| d.abs() <= FOV_TOLERANCE_RAD)
        };
        self.eye_width == other.eye_width
            && self.eye_height == other.eye_height
            && close(&self.fov[0], &other.fov[0])
            && close(&self.fov[1], &other.fov[1])
    }

    /// OpenVR raw projection: tangents of the four half-angles.
    ///
    /// OpenVR's "top" is the tangent of the **downward** extent (negative) and "bottom" that of
    /// the upward one: SteamVR builds `m[1][2] = (bottom + top) / (bottom - top)`, which only
    /// maps both vertical edges to NDC +-1 this way round. Swapping them mirrors an asymmetric
    /// frustum (the Quest looks further down than up): the picture stretches at one end and
    /// squashes at the other.
    pub fn projection(&self, eye: usize) -> HmdRect2_t {
        let f = self.fov[eye];
        HmdRect2_t {
            vTopLeft: HmdVector2_t {
                v: [f.angle_left.tan(), f.angle_down.tan()],
            },
            vBottomRight: HmdVector2_t {
                v: [f.angle_right.tan(), f.angle_up.tan()],
            },
        }
    }
}

/// The last connected headset's info, kept on disk so the next SteamVR start describes that
/// headset's display from the outset (see [`DisplayConfig::renders_like`]).
pub mod display_cache {
    use openlink_protocol::{self as proto, HeadsetInfo};
    use std::path::PathBuf;

    fn path() -> Option<PathBuf> {
        dirs::config_dir().map(|d| d.join("openlink").join("last-headset.bin"))
    }

    /// `None` when there is no cache or it is from another protocol version.
    pub fn load() -> Option<HeadsetInfo> {
        proto::decode(&std::fs::read(path()?).ok()?).ok()
    }

    pub fn save(info: &HeadsetInfo) -> std::io::Result<()> {
        let path = path().ok_or_else(|| std::io::Error::other("no config directory"))?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        // Write then rename, so a crash mid-write never leaves a truncated cache.
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, proto::encode(info))?;
        std::fs::rename(tmp, path)
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
    /// False while the controller rests untouched, which SteamVR shows as standby.
    pub proximity: VRInputComponentHandle_t,
}

pub struct ControllerSlot {
    pub id: AtomicU32,
    pub handles: Mutex<Option<ControllerHandles>>,
}

pub struct Shared {
    pub hmd_id: AtomicU32,
    pub controllers: [ControllerSlot; 2],
    pub display: Mutex<DisplayConfig>,
    /// The display SteamVR was started with, which its compositor keeps for the whole session.
    pub startup_display: DisplayConfig,
    /// A SteamVR restart was requested for a headset whose display differs from the startup one.
    pub restart_requested: std::sync::atomic::AtomicBool,
    /// Per hand (left, right): what the controllers are shown as.
    pub controller_model: Mutex<[ControllerModel; 2]>,
    pub prediction: PredictionConfig,
    /// The headset display's measured frame period, once known (0 until then): the vsync we
    /// give SteamVR runs at exactly this rate, not the nominal refresh rate.
    pub vsync_period_ns: std::sync::atomic::AtomicI64,
    /// Measured vsync-to-photon latency (microseconds) that controller poses are predicted over.
    pub photon_latency_us: AtomicU32,
    /// A headset is streaming tracking (blocks SteamVR's standby).
    pub tracking_active: std::sync::atomic::AtomicBool,
}

static SHARED: OnceLock<Shared> = OnceLock::new();

pub fn shared() -> &'static Shared {
    SHARED.get_or_init(|| {
        let slot = || ControllerSlot {
            id: AtomicU32::new(k_unTrackedDeviceIndexInvalid),
            handles: Mutex::new(None),
        };
        let display = match display_cache::load() {
            Some(info) => DisplayConfig::from_headset(&info),
            None => DisplayConfig::default(),
        };
        let controller_model = ControllerModel::from_headset_model(&display.model);
        Shared {
            startup_display: display.clone(),
            restart_requested: std::sync::atomic::AtomicBool::new(false),
            hmd_id: AtomicU32::new(k_unTrackedDeviceIndexInvalid),
            controllers: [slot(), slot()],
            display: Mutex::new(display),
            controller_model: Mutex::new([controller_model; 2]),
            prediction: PredictionConfig::default(),
            vsync_period_ns: std::sync::atomic::AtomicI64::new(0),
            photon_latency_us: AtomicU32::new(0),
            tracking_active: std::sync::atomic::AtomicBool::new(false),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// SteamVR's projection (OpenVR's `ComposeProjection`) applied to a direction with vertical
    /// tangent `tan_y`, at z = -1: NDC y.
    fn ndc_y(r: &HmdRect2_t, tan_y: f32) -> f32 {
        let (top, bottom) = (r.vTopLeft.v[1], r.vBottomRight.v[1]);
        let idy = 1.0 / (bottom - top);
        // y_clip = 2*idy*y + (bottom+top)*idy*z, w = -z.
        2.0 * idy * tan_y - (bottom + top) * idy
    }

    #[test]
    fn vertical_fov_edges_map_to_ndc_bounds() {
        // Quest 3: looks further down than up, which exposes a swapped top/bottom.
        let mut d = DisplayConfig::default();
        d.fov[0].angle_up = 0.768;
        d.fov[0].angle_down = -0.960;
        let r = d.projection(0);
        assert!((ndc_y(&r, 0.768f32.tan()) - 1.0).abs() < 1e-5);
        assert!((ndc_y(&r, (-0.960f32).tan()) + 1.0).abs() < 1e-5);
        assert!(r.vTopLeft.v[0] < 0.0 && r.vBottomRight.v[0] > 0.0);
    }

    #[test]
    fn render_setup_comparison() {
        let a = DisplayConfig::default();
        let mut b = a.clone();
        b.refresh_rate = 120.0;
        b.ipd_m = 0.07;
        b.fov[0].angle_up += 0.001;
        assert!(a.renders_like(&b), "refresh rate, IPD and float noise need no restart");
        b.eye_width = 1680;
        assert!(!a.renders_like(&b));
        let mut c = a.clone();
        c.fov[1].angle_down -= 0.05;
        assert!(!a.renders_like(&c));
    }
}
