//! OpenLink SteamVR driver.
//!
//! Loaded by `vrserver` through [`HmdDriverFactory`]. It registers one HMD and two Touch
//! controllers, receives already-clock-corrected tracking from the OpenLink server over
//! loopback, and hands it to SteamVR the moment it arrives. It deliberately has no networking
//! to the headset, no encoder and no UI: all of that lives in the server process so a crash or
//! restart there never takes SteamVR down.

pub mod devices;
pub mod direct_mode;
pub mod host;
pub mod link;
pub mod ovr;
pub mod state;

use devices::{controller_serial, Controller, Hmd, HMD_SERIAL};
use host::{host, Host, VrLogger};
use openlink_protocol::{FromDriver, Hand, HapticPulse};
use ovr::*;
use state::shared;
use std::ffi::{c_char, c_void, CStr};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::Mutex;

/// Run `f`, turning a panic into `fallback` so it never unwinds into vrserver.
pub(crate) fn guard<T>(fallback: T, f: impl FnOnce() -> T) -> T {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(v) => v,
        Err(e) => {
            let msg = e
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| e.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "unknown panic".into());
            log::error!("panic in driver callback: {msg}");
            fallback
        }
    }
}

// ------------------------------------------------------------------------------------------------
// IServerTrackedDeviceProvider
// ------------------------------------------------------------------------------------------------

#[repr(C)]
struct Provider {
    vtable: &'static IServerTrackedDeviceProvider_Vtbl,
}

static PROVIDER_VTBL: IServerTrackedDeviceProvider_Vtbl = IServerTrackedDeviceProvider_Vtbl {
    Init: provider_init,
    Cleanup: provider_cleanup,
    GetInterfaceVersions: provider_interface_versions,
    RunFrame: provider_run_frame,
    ShouldBlockStandbyMode: provider_block_standby,
    EnterStandby: provider_noop,
    LeaveStandby: provider_noop,
};

struct SyncProvider(Provider);
// SAFETY: only contains a pointer to an immutable static vtable.
unsafe impl Sync for SyncProvider {}
static PROVIDER: SyncProvider = SyncProvider(Provider {
    vtable: &PROVIDER_VTBL,
});

static LINK: Mutex<Option<link::Link>> = Mutex::new(None);
pub(crate) static VSYNC: Mutex<Option<direct_mode::VsyncThread>> = Mutex::new(None);

unsafe extern "C" fn provider_init(
    _this: *mut c_void,
    ctx: *mut Iface<IVRDriverContext_Vtbl>,
) -> EVRInitError {
    guard(VRInitError_Driver_Failed, || {
        let host = match Host::init(ctx) {
            Ok(h) => h,
            Err(e) => return e,
        };
        let _ = log::set_logger(&VrLogger).map(|()| log::set_max_level(log::LevelFilter::Info));
        log::info!("OpenLink driver {} initialising", env!("CARGO_PKG_VERSION"));
        shared();

        // Device objects must outlive the driver; SteamVR keeps raw pointers to them.
        let hmd: &'static mut Hmd = Box::leak(Box::new(Hmd::default()));
        if !host.add_device(HMD_SERIAL, TrackedDeviceClass_HMD, (hmd as *mut Hmd).cast()) {
            log::error!("SteamVR refused the HMD");
            return VRInitError_Driver_Failed;
        }
        for hand in [Hand::Left, Hand::Right] {
            let c: &'static mut Controller = Box::leak(Box::new(Controller::new(hand)));
            if !host.add_device(
                controller_serial(hand),
                TrackedDeviceClass_Controller,
                (c as *mut Controller).cast(),
            ) {
                log::warn!("SteamVR refused the {hand:?} controller");
            }
        }

        match link::Link::start() {
            Ok(l) => *LINK.lock().unwrap() = Some(l),
            Err(e) => {
                log::error!(
                    "cannot bind loopback port {}: {e}",
                    openlink_protocol::DRIVER_PORT
                );
                return VRInitError_Driver_Failed;
            }
        }
        VRInitError_None
    })
}

unsafe extern "C" fn provider_cleanup(_this: *mut c_void) {
    guard((), || {
        log::info!("OpenLink driver shutting down");
        VSYNC.lock().unwrap().take();
        LINK.lock().unwrap().take();
    })
}

unsafe extern "C" fn provider_interface_versions(_this: *mut c_void) -> *const *const c_char {
    INTERFACE_VERSIONS.0.as_ptr()
}

unsafe extern "C" fn provider_run_frame(_this: *mut c_void) {
    guard((), || {
        while let Some(ev) = host().poll_event() {
            if ev.event_type() == VREvent_Input_HapticVibration {
                forward_haptic(&ev.haptic());
            }
        }
    })
}

unsafe extern "C" fn provider_block_standby(_this: *mut c_void) -> bool {
    false
}

unsafe extern "C" fn provider_noop(_this: *mut c_void) {}

fn forward_haptic(v: &VREvent_HapticVibration_t) {
    let s = shared();
    let hand = [Hand::Left, Hand::Right].into_iter().find(|&hand| {
        s.controllers[hand as usize]
            .handles
            .lock()
            .unwrap()
            .is_some_and(|h| h.haptic == v.componentHandle)
    });
    let Some(hand) = hand else { return };
    if let Some(link) = LINK.lock().unwrap().as_ref() {
        link.send(&FromDriver::Haptics(HapticPulse {
            hand,
            duration_s: v.fDurationSeconds,
            frequency_hz: v.fFrequency,
            amplitude: v.fAmplitude,
        }));
    }
}

// ------------------------------------------------------------------------------------------------
// Entry point
// ------------------------------------------------------------------------------------------------

/// Called by vrserver once per interface it wants from the driver library.
///
/// # Safety
/// Called by SteamVR with a valid NUL-terminated interface name and a valid (or null) out
/// pointer.
#[no_mangle]
pub unsafe extern "C" fn HmdDriverFactory(
    interface_name: *const c_char,
    return_code: *mut i32,
) -> *mut c_void {
    let wanted = !interface_name.is_null()
        && CStr::from_ptr(interface_name).to_bytes_with_nul()
            == IServerTrackedDeviceProvider_Version;
    if wanted {
        return (&PROVIDER.0 as *const Provider).cast_mut().cast();
    }
    if !return_code.is_null() {
        *return_code = VRInitError_Init_InterfaceNotFound;
    }
    std::ptr::null_mut()
}
