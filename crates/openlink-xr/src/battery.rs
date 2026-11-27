//! Controller battery readings for SteamVR's dashboard, from OpenXR where the runtime offers
//! `XR_EXT_interaction_profile_battery_state_display`. The headset's own battery is not an
//! OpenXR matter: each app reads it from its platform and hands a reader to
//! [`XrTracking`](crate::tracking::XrTracking).
//!
//! OpenXR has no core way to read a controller's charge, and Meta's runtime (Quest 3, Horizon
//! OS v2.7) does not offer the extension, so on a Quest the controllers report no reading. The
//! system does know their charge, but only through `OVRRemoteService`
//! (`oculus.internal.remote.IRemoteService.getPairedDeviceStatus`), which answers apps outside
//! Meta with "Permission denied"; the permission is not one an app can request.

use openlink_protocol::BatteryState;
use openxr as xr;

/// A controller's battery (`hand` is the `/user/hand/…` path), or `None` without the extension,
/// with the controller off, or before the runtime has a reading.
pub fn controller(
    instance: &xr::Instance,
    session: &xr::Session<xr::AnyGraphics>,
    hand: xr::Path,
) -> Option<BatteryState> {
    if instance.exts().ext_interaction_profile_battery_state_display.is_none() {
        return None;
    }
    let mut battery = xr::sys::BatteryStateDisplayEXT {
        ty: xr::sys::BatteryStateDisplayEXT::TYPE,
        next: std::ptr::null_mut(),
        state_flags: xr::sys::BatteryStateDisplayStateFlagsEXT::EMPTY,
        battery_level: 0.0,
    };
    let mut state = xr::sys::InteractionProfileState {
        ty: xr::sys::InteractionProfileState::TYPE,
        next: (&raw mut battery).cast(),
        interaction_profile: xr::Path::NULL,
    };
    // SAFETY: valid session and path; `state` and its chained `battery` outlive the call.
    let r = unsafe { (instance.fp().get_current_interaction_profile)(session.as_raw(), hand, &mut state) };
    let flags = battery.state_flags;
    // The spec: the level must not be read unless VALID is set (never with no profile bound).
    if r != xr::sys::Result::SUCCESS || !flags.contains(xr::sys::BatteryStateDisplayStateFlagsEXT::VALID) {
        return None;
    }
    Some(BatteryState {
        level: battery.battery_level.clamp(0.0, 1.0),
        charging: flags.contains(xr::sys::BatteryStateDisplayStateFlagsEXT::CHARGING),
    })
}
