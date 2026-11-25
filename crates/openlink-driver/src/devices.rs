//! The tracked devices we expose: one HMD and two `oculus_touch` controllers.

use crate::host::host;
use crate::ovr::*;
use crate::state::{shared, ControllerHandles, DisplayConfig};
use openlink_protocol::{buttons, ControllerState, Hand};
use openlink_tracking::{ControllerModel, DriverPoseState};
use std::ffi::{c_char, c_void, CStr};
use std::sync::atomic::Ordering;

pub const HMD_SERIAL: &str = "OpenLink-HMD";

pub fn controller_serial(hand: Hand) -> &'static str {
    match hand {
        Hand::Left => "OpenLink-Controller-Left",
        Hand::Right => "OpenLink-Controller-Right",
    }
}

/// Our `DriverPoseState` -> the ABI struct.
pub fn to_ovr_pose(s: &DriverPoseState) -> DriverPose_t {
    let mut p = DriverPose_t::disconnected();
    p.poseTimeOffset = s.time_offset_s;
    p.vecPosition = s.position.to_array();
    p.vecVelocity = s.velocity.to_array();
    p.qRotation = HmdQuaternion_t {
        w: s.rotation.w,
        x: s.rotation.x,
        y: s.rotation.y,
        z: s.rotation.z,
    };
    p.vecAngularVelocity = s.angular_velocity.to_array();
    p.poseIsValid = s.valid;
    p.deviceIsConnected = true;
    p.result = if s.tracked {
        TrackingResult_Running_OK
    } else {
        TrackingResult_Running_OutOfRange
    };
    p
}

unsafe fn component_is(name: *const c_char, version: &[u8]) -> bool {
    !name.is_null() && CStr::from_ptr(name).to_bytes_with_nul() == version
}

unsafe extern "C" fn noop(_: *mut c_void) {}
unsafe extern "C" fn get_pose_unused() {}
unsafe extern "C" fn debug_request(_: *mut c_void, _: *const c_char, resp: *mut c_char, size: u32) {
    if size > 0 {
        *resp = 0;
    }
}

// ------------------------------------------------------------------------------------------------
// HMD
// ------------------------------------------------------------------------------------------------

/// `ITrackedDeviceServerDriver` + `IVRDisplayComponent`. C++ would use multiple inheritance; we
/// embed the display sub-object and hand out a pointer to it from `GetComponent`.
#[repr(C)]
pub struct Hmd {
    vtable: &'static ITrackedDeviceServerDriver_Vtbl,
    display: DisplayObj,
    #[cfg(target_os = "linux")]
    direct_mode: crate::direct_mode::DirectModeObj,
}

/// Whether the HMD exposes `IVRDriverDirectModeComponent` (compositor renders into our
/// textures) instead of the "display on desktop" path. Linux allocates the textures through
/// SteamVR's IPC resource manager; Windows support comes with the D3D11 texture path.
pub const fn use_direct_mode() -> bool {
    cfg!(target_os = "linux")
}

#[repr(C)]
struct DisplayObj {
    vtable: &'static IVRDisplayComponent_Vtbl,
}

impl Default for Hmd {
    fn default() -> Self {
        Self {
            vtable: &HMD_VTBL,
            display: DisplayObj {
                vtable: &DISPLAY_VTBL,
            },
            #[cfg(target_os = "linux")]
            direct_mode: crate::direct_mode::DirectModeObj::default(),
        }
    }
}

static HMD_VTBL: ITrackedDeviceServerDriver_Vtbl = ITrackedDeviceServerDriver_Vtbl {
    Activate: hmd_activate,
    Deactivate: hmd_deactivate,
    EnterStandby: noop,
    GetComponent: hmd_get_component,
    DebugRequest: debug_request,
    GetPose: get_pose_unused,
};

unsafe extern "C" fn hmd_activate(_this: *mut c_void, id: u32) -> EVRInitError {
    crate::guard(VRInitError_Driver_Failed, || {
        let s = shared();
        s.hmd_id.store(id, Ordering::Release);
        let h = host();
        let c = h.container(id);
        let d = s.display.lock().unwrap().clone();
        h.set_string(c, prop::TrackingSystemName_String, "openlink");
        h.set_string(c, prop::ModelNumber_String, &d.model);
        h.set_string(c, prop::ManufacturerName_String, "Meta");
        h.set_string(c, prop::SerialNumber_String, HMD_SERIAL);
        h.set_string(c, prop::RenderModelName_String, "generic_hmd");
        h.set_f32(c, prop::DisplayFrequency_Float, d.refresh_rate);
        h.set_f32(c, prop::UserIpdMeters_Float, d.ipd_m);
        h.set_f32(c, prop::SecondsFromVsyncToPhotons_Float, 0.0);
        h.set_u64(c, prop::CurrentUniverseId_Uint64, 0x4F4C_4E4B); // "OLNK"
        h.set_bool(c, prop::IsOnDesktop_Bool, false);
        h.set_bool(c, prop::DriverDirectModeSendsVsyncEvents_Bool, use_direct_mode());
        h.set_bool(c, prop::DeviceIsWireless_Bool, true);
        h.set_bool(c, prop::DeviceProvidesBatteryStatus_Bool, true);
        h.pose_updated(id, &DriverPose_t::disconnected());
        #[cfg(target_os = "linux")]
        if use_direct_mode() {
            let vsync = crate::direct_mode::VsyncThread::start(|| {
                let measured = shared().vsync_period_ns.load(Ordering::Acquire);
                if measured > 0 {
                    (1e9 / measured as f64) as f32
                } else {
                    shared().display.lock().unwrap().refresh_rate
                }
            });
            *crate::VSYNC.lock().unwrap() = Some(vsync);
        }
        log::info!("HMD activated as device {id} (direct mode: {})", use_direct_mode());
        VRInitError_None
    })
}

unsafe extern "C" fn hmd_deactivate(_this: *mut c_void) {
    #[cfg(target_os = "linux")]
    crate::VSYNC.lock().unwrap().take();
    shared()
        .hmd_id
        .store(k_unTrackedDeviceIndexInvalid, Ordering::Release);
}

unsafe extern "C" fn hmd_get_component(this: *mut c_void, name: *const c_char) -> *mut c_void {
    if component_is(name, IVRDisplayComponent_Version) {
        let hmd = this.cast::<Hmd>();
        return std::ptr::addr_of_mut!((*hmd).display).cast();
    }
    #[cfg(target_os = "linux")]
    if component_is(name, IVRDriverDirectModeComponent_Version) && use_direct_mode() {
        let hmd = this.cast::<Hmd>();
        return std::ptr::addr_of_mut!((*hmd).direct_mode).cast();
    }
    std::ptr::null_mut()
}

// Display component. With direct mode the compositor renders into our swap textures and only
// uses this for projection/render size. Without it (Windows, for now) SteamVR falls back to
// rendering into a desktop window described here.

const WINDOW_W: u32 = 1920;
const WINDOW_H: u32 = 1080;

static DISPLAY_VTBL: IVRDisplayComponent_Vtbl = IVRDisplayComponent_Vtbl {
    GetWindowBounds: disp_window_bounds,
    IsDisplayOnDesktop: disp_on_desktop,
    IsDisplayRealDisplay: disp_false,
    GetRecommendedRenderTargetSize: disp_render_target,
    GetEyeOutputViewport: disp_eye_viewport,
    GetProjectionRaw: disp_projection,
    ComputeDistortion: disp_distortion,
    ComputeInverseDistortion: disp_inverse_distortion,
};

unsafe extern "C" fn disp_on_desktop(_: *mut c_void) -> bool {
    !use_direct_mode()
}
unsafe extern "C" fn disp_false(_: *mut c_void) -> bool {
    false
}
unsafe extern "C" fn disp_window_bounds(
    _: *mut c_void,
    x: *mut i32,
    y: *mut i32,
    w: *mut u32,
    h: *mut u32,
) {
    (*x, *y, *w, *h) = (0, 0, WINDOW_W, WINDOW_H);
}
unsafe extern "C" fn disp_render_target(_: *mut c_void, w: *mut u32, h: *mut u32) {
    let d = shared().display.lock().unwrap();
    (*w, *h) = (d.eye_width, d.eye_height);
}
unsafe extern "C" fn disp_eye_viewport(
    _: *mut c_void,
    eye: i32,
    x: *mut u32,
    y: *mut u32,
    w: *mut u32,
    h: *mut u32,
) {
    (*x, *y, *w, *h) = (
        if eye == 0 { 0 } else { WINDOW_W / 2 },
        0,
        WINDOW_W / 2,
        WINDOW_H,
    );
}
unsafe extern "C" fn disp_projection(
    _: *mut c_void,
    eye: i32,
    l: *mut f32,
    r: *mut f32,
    t: *mut f32,
    b: *mut f32,
) {
    let rect = shared()
        .display
        .lock()
        .unwrap()
        .projection(eye.clamp(0, 1) as usize);
    (*l, *t) = (rect.vTopLeft.v[0], rect.vTopLeft.v[1]);
    (*r, *b) = (rect.vBottomRight.v[0], rect.vBottomRight.v[1]);
}
#[cfg(not(windows))]
unsafe extern "C" fn disp_distortion(
    _: *mut c_void,
    _eye: i32,
    u: f32,
    v: f32,
) -> DistortionCoordinates_t {
    DistortionCoordinates_t {
        rfRed: [u, v],
        rfGreen: [u, v],
        rfBlue: [u, v],
    }
}
#[cfg(windows)]
unsafe extern "C" fn disp_distortion(
    _: *mut c_void,
    out: *mut DistortionCoordinates_t,
    _eye: i32,
    u: f32,
    v: f32,
) -> *mut DistortionCoordinates_t {
    *out = DistortionCoordinates_t {
        rfRed: [u, v],
        rfGreen: [u, v],
        rfBlue: [u, v],
    };
    out
}
unsafe extern "C" fn disp_inverse_distortion(
    _: *mut c_void,
    out: *mut HmdVector2_t,
    _eye: i32,
    _channel: u32,
    u: f32,
    v: f32,
) -> bool {
    (*out).v = [u, v];
    true
}

/// How far past vsync SteamVR predicts poses: the measured present-to-photon latency.
pub fn set_photon_latency(seconds: f32) {
    let id = shared().hmd_id.load(Ordering::Acquire);
    if id == k_unTrackedDeviceIndexInvalid {
        return;
    }
    let h = host();
    h.set_f32(h.container(id), prop::SecondsFromVsyncToPhotons_Float, seconds.clamp(0.0, 0.2));
}

/// Push new display parameters (from the headset's Hello) to SteamVR at runtime.
pub fn apply_display(d: &DisplayConfig) {
    let id = shared().hmd_id.load(Ordering::Acquire);
    if id == k_unTrackedDeviceIndexInvalid {
        return;
    }
    let h = host();
    let c = h.container(id);
    h.set_string(c, prop::ModelNumber_String, &d.model);
    h.set_f32(c, prop::DisplayFrequency_Float, d.refresh_rate);
    h.set_f32(c, prop::UserIpdMeters_Float, d.ipd_m);
    h.set_render_target_size(id, d.eye_width, d.eye_height);
    h.set_projection(id, d.projection(0), d.projection(1));
}

// ------------------------------------------------------------------------------------------------
// Controllers
// ------------------------------------------------------------------------------------------------

#[repr(C)]
pub struct Controller {
    vtable: &'static ITrackedDeviceServerDriver_Vtbl,
    hand: Hand,
}

impl Controller {
    pub fn new(hand: Hand) -> Self {
        Self {
            vtable: &CONTROLLER_VTBL,
            hand,
        }
    }
}

static CONTROLLER_VTBL: ITrackedDeviceServerDriver_Vtbl = ITrackedDeviceServerDriver_Vtbl {
    Activate: ctrl_activate,
    Deactivate: ctrl_deactivate,
    EnterStandby: noop,
    GetComponent: ctrl_get_component,
    DebugRequest: debug_request,
    GetPose: get_pose_unused,
};

unsafe extern "C" fn ctrl_activate(this: *mut c_void, id: u32) -> EVRInitError {
    let hand = (*this.cast::<Controller>()).hand;
    crate::guard(VRInitError_Driver_Failed, || {
        let s = shared();
        let h = host();
        let c = h.container(id);
        let model = *s.controller_model.lock().unwrap();
        let (side, role, model_number) = match hand {
            Hand::Left => (
                "left",
                TrackedControllerRole_LeftHand,
                "Oculus Quest (Left Controller)",
            ),
            Hand::Right => (
                "right",
                TrackedControllerRole_RightHand,
                "Oculus Quest (Right Controller)",
            ),
        };
        // Present as genuine Touch controllers so games' shipped Touch bindings, pose offsets
        // and render models all apply unchanged.
        h.set_string(c, prop::TrackingSystemName_String, "oculus");
        h.set_string(c, prop::ManufacturerName_String, "Oculus");
        h.set_string(c, prop::ModelNumber_String, model_number);
        h.set_string(c, prop::SerialNumber_String, controller_serial(hand));
        h.set_string(c, prop::RenderModelName_String, model.render_model(hand));
        h.set_string(c, prop::ControllerType_String, "oculus_touch");
        h.set_string(
            c,
            prop::InputProfilePath_String,
            "{oculus}/input/touch_profile.json",
        );
        h.set_i32(c, prop::ControllerRoleHint_Int32, role);
        h.set_bool(c, prop::DeviceIsWireless_Bool, true);
        h.set_bool(c, prop::DeviceProvidesBatteryStatus_Bool, true);

        let b = |p: &CStr| h.create_bool(c, p);
        let f = |p: &CStr, two: bool| h.create_scalar(c, p, two);
        let (ax_click, ax_touch, by_click, by_touch) = match hand {
            Hand::Left => (
                b(c"/input/x/click"),
                b(c"/input/x/touch"),
                b(c"/input/y/click"),
                b(c"/input/y/touch"),
            ),
            Hand::Right => (
                b(c"/input/a/click"),
                b(c"/input/a/touch"),
                b(c"/input/b/click"),
                b(c"/input/b/touch"),
            ),
        };
        let handles = ControllerHandles {
            ax_click,
            ax_touch,
            by_click,
            by_touch,
            // The Touch profile maps the left menu button to /input/system.
            menu_click: (hand == Hand::Left).then(|| b(c"/input/system/click")),
            stick_click: b(c"/input/joystick/click"),
            stick_touch: b(c"/input/joystick/touch"),
            stick_x: f(c"/input/joystick/x", true),
            stick_y: f(c"/input/joystick/y", true),
            trigger: f(c"/input/trigger/value", false),
            trigger_touch: b(c"/input/trigger/touch"),
            grip: f(c"/input/grip/value", false),
            grip_touch: b(c"/input/grip/touch"),
            thumbrest_touch: b(c"/input/thumbrest/touch"),
            haptic: h.create_haptic(c, c"/output/haptic"),
        };
        *s.controllers[hand as usize].handles.lock().unwrap() = Some(handles);
        s.controllers[hand as usize].id.store(id, Ordering::Release);
        h.pose_updated(id, &DriverPose_t::disconnected());
        log::info!("{side} controller activated as device {id}");
        VRInitError_None
    })
}

unsafe extern "C" fn ctrl_deactivate(this: *mut c_void) {
    let hand = (*this.cast::<Controller>()).hand;
    let slot = &shared().controllers[hand as usize];
    slot.id
        .store(k_unTrackedDeviceIndexInvalid, Ordering::Release);
    *slot.handles.lock().unwrap() = None;
}

unsafe extern "C" fn ctrl_get_component(_: *mut c_void, _: *const c_char) -> *mut c_void {
    std::ptr::null_mut()
}

/// Forward one controller's buttons/axes. `time_offset` is the (negative) sample age.
pub fn update_inputs(handles: &ControllerHandles, st: &ControllerState, time_offset: f64) {
    let h = host();
    let i = &st.input;
    let bit = |m: u32| i.buttons & m != 0;
    h.update_bool(handles.ax_click, bit(buttons::A_X_CLICK), time_offset);
    h.update_bool(handles.ax_touch, bit(buttons::A_X_TOUCH), time_offset);
    h.update_bool(handles.by_click, bit(buttons::B_Y_CLICK), time_offset);
    h.update_bool(handles.by_touch, bit(buttons::B_Y_TOUCH), time_offset);
    if let Some(m) = handles.menu_click {
        h.update_bool(m, bit(buttons::MENU_CLICK), time_offset);
    }
    h.update_bool(
        handles.stick_click,
        bit(buttons::THUMBSTICK_CLICK),
        time_offset,
    );
    h.update_bool(
        handles.stick_touch,
        bit(buttons::THUMBSTICK_TOUCH),
        time_offset,
    );
    h.update_scalar(handles.stick_x, i.thumbstick[0], time_offset);
    h.update_scalar(handles.stick_y, i.thumbstick[1], time_offset);
    h.update_scalar(handles.trigger, i.trigger, time_offset);
    h.update_bool(
        handles.trigger_touch,
        bit(buttons::TRIGGER_TOUCH),
        time_offset,
    );
    h.update_scalar(handles.grip, i.grip, time_offset);
    h.update_bool(handles.grip_touch, bit(buttons::GRIP_TOUCH), time_offset);
    h.update_bool(
        handles.thumbrest_touch,
        bit(buttons::THUMBREST_TOUCH),
        time_offset,
    );
}

/// Called when a headset with possibly different controllers connects.
pub fn apply_controller_model(model: ControllerModel) {
    let s = shared();
    *s.controller_model.lock().unwrap() = model;
    for hand in [Hand::Left, Hand::Right] {
        let id = s.controllers[hand as usize].id.load(Ordering::Acquire);
        if id != k_unTrackedDeviceIndexInvalid {
            let h = host();
            h.set_string(
                h.container(id),
                prop::RenderModelName_String,
                model.render_model(hand),
            );
        }
    }
}
