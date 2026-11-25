//! Hand-written OpenVR driver ABI (openvr_driver.h, SDK 2.x).
//!
//! OpenVR interfaces are C++ classes with only pure virtual methods and no virtual destructor,
//! so an object is `{ vtable*, ... }` and every method is a plain function taking `this` first.
//! On x86-64 both the Itanium (Linux) and MSVC (Windows) ABIs pass `this` like an ordinary first
//! argument, so `extern "C"` matches. The one exception is methods returning a large struct by
//! value (`ComputeDistortion`), where MSVC puts the hidden return pointer *after* `this`; see
//! [`ComputeDistortionFn`].
//!
//! Struct layouts were checked against the real header with `sizeof`/`offsetof` (see the
//! `layout` test below).

#![allow(
    non_camel_case_types,
    non_snake_case,
    non_upper_case_globals,
    dead_code
)]

use std::ffi::{c_char, c_void};

pub type EVRInitError = i32;
pub type ETrackedPropertyError = i32;
pub type EVRInputError = i32;
pub type PropertyContainerHandle_t = u64;
pub type VRInputComponentHandle_t = u64;
pub type TrackedDeviceIndex_t = u32;

pub const VRInitError_None: EVRInitError = 0;
pub const VRInitError_Init_InterfaceNotFound: EVRInitError = 105;
pub const VRInitError_Driver_Failed: EVRInitError = 200;

pub const k_unTrackedDeviceIndexInvalid: u32 = 0xFFFF_FFFF;

pub const TrackedDeviceClass_HMD: i32 = 1;
pub const TrackedDeviceClass_Controller: i32 = 2;

pub const TrackedControllerRole_LeftHand: i32 = 1;
pub const TrackedControllerRole_RightHand: i32 = 2;

pub const TrackingResult_Uninitialized: i32 = 1;
pub const TrackingResult_Running_OK: i32 = 200;
pub const TrackingResult_Running_OutOfRange: i32 = 201;

pub const VRScalarType_Absolute: i32 = 0;
pub const VRScalarUnits_NormalizedOneSided: i32 = 0;
pub const VRScalarUnits_NormalizedTwoSided: i32 = 1;

pub const VREvent_Input_HapticVibration: u32 = 1700;

// Property type tags.
pub const k_unFloatPropertyTag: u32 = 1;
pub const k_unInt32PropertyTag: u32 = 2;
pub const k_unUint64PropertyTag: u32 = 3;
pub const k_unBoolPropertyTag: u32 = 4;
pub const k_unStringPropertyTag: u32 = 5;

pub const PropertyWrite_Set: i32 = 0;

/// The subset of `ETrackedDeviceProperty` we write.
pub mod prop {
    pub const TrackingSystemName_String: i32 = 1000;
    pub const ModelNumber_String: i32 = 1001;
    pub const SerialNumber_String: i32 = 1002;
    pub const RenderModelName_String: i32 = 1003;
    pub const ManufacturerName_String: i32 = 1005;
    pub const DeviceIsWireless_Bool: i32 = 1010;
    pub const DeviceIsCharging_Bool: i32 = 1011;
    pub const DeviceBatteryPercentage_Float: i32 = 1012;
    pub const DeviceProvidesBatteryStatus_Bool: i32 = 1026;
    pub const InputProfilePath_String: i32 = 1037;
    pub const SecondsFromVsyncToPhotons_Float: i32 = 2001;
    pub const DisplayFrequency_Float: i32 = 2002;
    pub const UserIpdMeters_Float: i32 = 2003;
    pub const CurrentUniverseId_Uint64: i32 = 2004;
    pub const IsOnDesktop_Bool: i32 = 2007;
    pub const ControllerRoleHint_Int32: i32 = 3007;
    pub const ControllerType_String: i32 = 7000;
}

pub const IServerTrackedDeviceProvider_Version: &[u8] = b"IServerTrackedDeviceProvider_004\0";
pub const ITrackedDeviceServerDriver_Version: &[u8] = b"ITrackedDeviceServerDriver_005\0";
pub const IVRDisplayComponent_Version: &[u8] = b"IVRDisplayComponent_003\0";
pub const IVRServerDriverHost_Version: &[u8] = b"IVRServerDriverHost_006\0";
pub const IVRProperties_Version: &[u8] = b"IVRProperties_001\0";
pub const IVRDriverInput_Version: &[u8] = b"IVRDriverInput_004\0";
pub const IVRDriverLog_Version: &[u8] = b"IVRDriverLog_001\0";

/// `k_InterfaceVersions`: what this driver was built against, returned from
/// `GetInterfaceVersions`.
pub struct InterfaceVersions(pub [*const c_char; 13]);
// SAFETY: pointers to 'static string literals.
unsafe impl Sync for InterfaceVersions {}

pub static INTERFACE_VERSIONS: InterfaceVersions = InterfaceVersions([
    c"IVRSettings_003".as_ptr(),
    c"ITrackedDeviceServerDriver_005".as_ptr(),
    c"IVRDisplayComponent_003".as_ptr(),
    c"IVRDriverDirectModeComponent_009".as_ptr(),
    c"IVRCameraComponent_003".as_ptr(),
    c"IServerTrackedDeviceProvider_004".as_ptr(),
    c"IVRWatchdogProvider_001".as_ptr(),
    c"IVRVirtualDisplay_002".as_ptr(),
    c"IVRDriverManager_001".as_ptr(),
    c"IVRResources_001".as_ptr(),
    c"IVRCompositorPluginProvider_001".as_ptr(),
    c"IVRIPCResourceManagerClient_003".as_ptr(),
    std::ptr::null(),
]);

// ------------------------------------------------------------------------------------------------
// Plain structs
// ------------------------------------------------------------------------------------------------

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct HmdQuaternion_t {
    pub w: f64,
    pub x: f64,
    pub y: f64,
    pub z: f64,
}

impl HmdQuaternion_t {
    pub const IDENTITY: Self = Self {
        w: 1.0,
        x: 0.0,
        y: 0.0,
        z: 0.0,
    };
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct DriverPose_t {
    pub poseTimeOffset: f64,
    pub qWorldFromDriverRotation: HmdQuaternion_t,
    pub vecWorldFromDriverTranslation: [f64; 3],
    pub qDriverFromHeadRotation: HmdQuaternion_t,
    pub vecDriverFromHeadTranslation: [f64; 3],
    pub vecPosition: [f64; 3],
    pub vecVelocity: [f64; 3],
    pub vecAcceleration: [f64; 3],
    pub qRotation: HmdQuaternion_t,
    pub vecAngularVelocity: [f64; 3],
    pub vecAngularAcceleration: [f64; 3],
    pub result: i32,
    pub poseIsValid: bool,
    pub willDriftInYaw: bool,
    pub shouldApplyHeadModel: bool,
    pub deviceIsConnected: bool,
}

impl DriverPose_t {
    /// A pose with identity transforms and nothing valid.
    pub fn disconnected() -> Self {
        Self {
            poseTimeOffset: 0.0,
            qWorldFromDriverRotation: HmdQuaternion_t::IDENTITY,
            vecWorldFromDriverTranslation: [0.0; 3],
            qDriverFromHeadRotation: HmdQuaternion_t::IDENTITY,
            vecDriverFromHeadTranslation: [0.0; 3],
            vecPosition: [0.0; 3],
            vecVelocity: [0.0; 3],
            vecAcceleration: [0.0; 3],
            qRotation: HmdQuaternion_t::IDENTITY,
            vecAngularVelocity: [0.0; 3],
            vecAngularAcceleration: [0.0; 3],
            result: TrackingResult_Uninitialized,
            poseIsValid: false,
            willDriftInYaw: false,
            shouldApplyHeadModel: false,
            deviceIsConnected: false,
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct DistortionCoordinates_t {
    pub rfRed: [f32; 2],
    pub rfGreen: [f32; 2],
    pub rfBlue: [f32; 2],
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct HmdVector2_t {
    pub v: [f32; 2],
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct HmdRect2_t {
    pub vTopLeft: HmdVector2_t,
    pub vBottomRight: HmdVector2_t,
}

#[repr(C)]
pub struct PropertyWrite_t {
    pub prop: i32,
    pub writeType: i32,
    pub eSetError: ETrackedPropertyError,
    pub pvBuffer: *mut c_void,
    pub unBufferSize: u32,
    pub unTag: u32,
    pub eError: ETrackedPropertyError,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct VREvent_HapticVibration_t {
    pub containerHandle: u64,
    pub componentHandle: u64,
    pub fDurationSeconds: f32,
    pub fFrequency: f32,
    pub fAmplitude: f32,
}

/// `VREvent_t` is packed to 4 on Linux (a historical accident Valve preserved) and naturally
/// aligned on Windows, so we treat it as bytes and read fields at the right offsets.
#[cfg(not(windows))]
pub const VREVENT_SIZE: usize = 60;
#[cfg(not(windows))]
pub const VREVENT_DATA_OFFSET: usize = 12;
#[cfg(windows)]
pub const VREVENT_SIZE: usize = 64;
#[cfg(windows)]
pub const VREVENT_DATA_OFFSET: usize = 16;

#[repr(C, align(8))]
pub struct VREvent_t(pub [u8; VREVENT_SIZE]);

impl VREvent_t {
    pub fn zeroed() -> Self {
        Self([0; VREVENT_SIZE])
    }
    pub fn event_type(&self) -> u32 {
        u32::from_ne_bytes(self.0[0..4].try_into().unwrap())
    }
    pub fn haptic(&self) -> VREvent_HapticVibration_t {
        let d = &self.0[VREVENT_DATA_OFFSET..];
        let u64_at = |o: usize| u64::from_ne_bytes(d[o..o + 8].try_into().unwrap());
        let f32_at = |o: usize| f32::from_ne_bytes(d[o..o + 4].try_into().unwrap());
        VREvent_HapticVibration_t {
            containerHandle: u64_at(0),
            componentHandle: u64_at(8),
            fDurationSeconds: f32_at(16),
            fFrequency: f32_at(20),
            fAmplitude: f32_at(24),
        }
    }
}

// ------------------------------------------------------------------------------------------------
// Interfaces implemented by SteamVR (we call these)
// ------------------------------------------------------------------------------------------------

/// Any C++ interface pointer: points at an object whose first word is its vtable.
#[repr(C)]
pub struct Iface<V> {
    pub vtable: *const V,
}

#[repr(C)]
pub struct IVRDriverContext_Vtbl {
    pub GetGenericInterface: unsafe extern "C" fn(
        this: *mut Iface<Self>,
        name: *const c_char,
        err: *mut EVRInitError,
    ) -> *mut c_void,
    pub GetDriverHandle: unsafe extern "C" fn(this: *mut Iface<Self>) -> u64,
}

#[repr(C)]
pub struct IVRServerDriverHost_Vtbl {
    pub TrackedDeviceAdded: unsafe extern "C" fn(
        this: *mut Iface<Self>,
        serial: *const c_char,
        class: i32,
        driver: *mut c_void,
    ) -> bool,
    pub TrackedDevicePoseUpdated: unsafe extern "C" fn(
        this: *mut Iface<Self>,
        which: u32,
        pose: *const DriverPose_t,
        size: u32,
    ),
    pub VsyncEvent: unsafe extern "C" fn(this: *mut Iface<Self>, offset: f64),
    pub VendorSpecificEvent: *const c_void,
    pub IsExiting: unsafe extern "C" fn(this: *mut Iface<Self>) -> bool,
    pub PollNextEvent:
        unsafe extern "C" fn(this: *mut Iface<Self>, ev: *mut VREvent_t, size: u32) -> bool,
    pub GetRawTrackedDevicePoses: *const c_void,
    pub RequestRestart: *const c_void,
    pub GetFrameTimings: *const c_void,
    pub SetDisplayEyeToHead: *const c_void,
    pub SetDisplayProjectionRaw: unsafe extern "C" fn(
        this: *mut Iface<Self>,
        which: u32,
        left: *const HmdRect2_t,
        right: *const HmdRect2_t,
    ),
    pub SetRecommendedRenderTargetSize:
        unsafe extern "C" fn(this: *mut Iface<Self>, which: u32, w: u32, h: u32),
}

#[repr(C)]
pub struct IVRProperties_Vtbl {
    pub ReadPropertyBatch: *const c_void,
    pub WritePropertyBatch: unsafe extern "C" fn(
        this: *mut Iface<Self>,
        container: PropertyContainerHandle_t,
        batch: *mut PropertyWrite_t,
        count: u32,
    ) -> ETrackedPropertyError,
    pub GetPropErrorNameFromEnum: *const c_void,
    pub TrackedDeviceToPropertyContainer: unsafe extern "C" fn(
        this: *mut Iface<Self>,
        device: TrackedDeviceIndex_t,
    ) -> PropertyContainerHandle_t,
}

#[repr(C)]
pub struct IVRDriverInput_Vtbl {
    pub CreateBooleanComponent: unsafe extern "C" fn(
        this: *mut Iface<Self>,
        container: PropertyContainerHandle_t,
        name: *const c_char,
        handle: *mut VRInputComponentHandle_t,
    ) -> EVRInputError,
    pub UpdateBooleanComponent: unsafe extern "C" fn(
        this: *mut Iface<Self>,
        handle: VRInputComponentHandle_t,
        value: bool,
        time_offset: f64,
    ) -> EVRInputError,
    pub CreateScalarComponent: unsafe extern "C" fn(
        this: *mut Iface<Self>,
        container: PropertyContainerHandle_t,
        name: *const c_char,
        handle: *mut VRInputComponentHandle_t,
        kind: i32,
        units: i32,
    ) -> EVRInputError,
    pub UpdateScalarComponent: unsafe extern "C" fn(
        this: *mut Iface<Self>,
        handle: VRInputComponentHandle_t,
        value: f32,
        time_offset: f64,
    ) -> EVRInputError,
    pub CreateHapticComponent: unsafe extern "C" fn(
        this: *mut Iface<Self>,
        container: PropertyContainerHandle_t,
        name: *const c_char,
        handle: *mut VRInputComponentHandle_t,
    ) -> EVRInputError,
}

#[repr(C)]
pub struct IVRDriverLog_Vtbl {
    pub Log: unsafe extern "C" fn(this: *mut Iface<Self>, msg: *const c_char),
}

// ------------------------------------------------------------------------------------------------
// Interfaces implemented by us (SteamVR calls these)
// ------------------------------------------------------------------------------------------------

#[repr(C)]
pub struct IServerTrackedDeviceProvider_Vtbl {
    pub Init: unsafe extern "C" fn(
        this: *mut c_void,
        ctx: *mut Iface<IVRDriverContext_Vtbl>,
    ) -> EVRInitError,
    pub Cleanup: unsafe extern "C" fn(this: *mut c_void),
    pub GetInterfaceVersions: unsafe extern "C" fn(this: *mut c_void) -> *const *const c_char,
    pub RunFrame: unsafe extern "C" fn(this: *mut c_void),
    pub ShouldBlockStandbyMode: unsafe extern "C" fn(this: *mut c_void) -> bool,
    pub EnterStandby: unsafe extern "C" fn(this: *mut c_void),
    pub LeaveStandby: unsafe extern "C" fn(this: *mut c_void),
}

#[repr(C)]
pub struct ITrackedDeviceServerDriver_Vtbl {
    pub Activate: unsafe extern "C" fn(this: *mut c_void, object_id: u32) -> EVRInitError,
    pub Deactivate: unsafe extern "C" fn(this: *mut c_void),
    pub EnterStandby: unsafe extern "C" fn(this: *mut c_void),
    pub GetComponent: unsafe extern "C" fn(this: *mut c_void, name: *const c_char) -> *mut c_void,
    pub DebugRequest:
        unsafe extern "C" fn(this: *mut c_void, req: *const c_char, resp: *mut c_char, size: u32),
    /// Documented as never called; present only to keep the vtable the right length.
    pub GetPose: unsafe extern "C" fn(),
}

/// `DistortionCoordinates_t ComputeDistortion(EVREye, float u, float v)`.
#[cfg(not(windows))]
pub type ComputeDistortionFn =
    unsafe extern "C" fn(this: *mut c_void, eye: i32, u: f32, v: f32) -> DistortionCoordinates_t;
/// MSVC member functions pass the hidden return pointer after `this` and return it.
#[cfg(windows)]
pub type ComputeDistortionFn = unsafe extern "C" fn(
    this: *mut c_void,
    out: *mut DistortionCoordinates_t,
    eye: i32,
    u: f32,
    v: f32,
) -> *mut DistortionCoordinates_t;

#[repr(C)]
pub struct IVRDisplayComponent_Vtbl {
    pub GetWindowBounds:
        unsafe extern "C" fn(this: *mut c_void, x: *mut i32, y: *mut i32, w: *mut u32, h: *mut u32),
    pub IsDisplayOnDesktop: unsafe extern "C" fn(this: *mut c_void) -> bool,
    pub IsDisplayRealDisplay: unsafe extern "C" fn(this: *mut c_void) -> bool,
    pub GetRecommendedRenderTargetSize:
        unsafe extern "C" fn(this: *mut c_void, w: *mut u32, h: *mut u32),
    pub GetEyeOutputViewport: unsafe extern "C" fn(
        this: *mut c_void,
        eye: i32,
        x: *mut u32,
        y: *mut u32,
        w: *mut u32,
        h: *mut u32,
    ),
    pub GetProjectionRaw: unsafe extern "C" fn(
        this: *mut c_void,
        eye: i32,
        l: *mut f32,
        r: *mut f32,
        t: *mut f32,
        b: *mut f32,
    ),
    pub ComputeDistortion: ComputeDistortionFn,
    pub ComputeInverseDistortion: unsafe extern "C" fn(
        this: *mut c_void,
        out: *mut HmdVector2_t,
        eye: i32,
        channel: u32,
        u: f32,
        v: f32,
    ) -> bool,
}

#[cfg(test)]
mod layout {
    use super::*;
    use std::mem::{offset_of, size_of};

    /// Values printed by compiling openvr_driver.h (SDK 2.x) with g++ on x86-64 Linux.
    #[test]
    fn matches_openvr_header() {
        assert_eq!(size_of::<DriverPose_t>(), 280);
        assert_eq!(offset_of!(DriverPose_t, result), 272);
        assert_eq!(size_of::<DistortionCoordinates_t>(), 24);
        assert_eq!(size_of::<PropertyWrite_t>(), 40);
        assert_eq!(size_of::<VREvent_HapticVibration_t>(), 32);
        #[cfg(target_os = "linux")]
        assert_eq!(size_of::<VREvent_t>(), 64); // 60 bytes + our align(8) padding; we pass 60.
    }
}
