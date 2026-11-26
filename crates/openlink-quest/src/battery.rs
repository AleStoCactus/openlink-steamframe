//! Battery readings for SteamVR's dashboard: the headset's from Android, the controllers' from
//! OpenXR where the runtime offers `XR_EXT_interaction_profile_battery_state_display`.
//!
//! OpenXR has no core way to read a controller's charge, and Meta's runtime (Quest 3, Horizon
//! OS v2.7) does not offer the extension, so on a Quest the controllers report no reading. The
//! system does know their charge, but only through `OVRRemoteService`
//! (`oculus.internal.remote.IRemoteService.getPairedDeviceStatus`), which answers apps outside
//! Meta with "Permission denied"; the permission is not one an app can request.

use android_activity::AndroidApp;
use jni::objects::JValue;
use jni::{jni_sig, jni_str};
use openlink_protocol::BatteryState;
use openxr as xr;

/// `BatteryManager.BATTERY_PROPERTY_CAPACITY`: remaining charge in percent.
const BATTERY_PROPERTY_CAPACITY: i32 = 4;

/// The headset's own battery, through `BatteryManager` (no broadcast receiver needed: both
/// calls answer immediately).
pub fn headset(app: &AndroidApp) -> Option<BatteryState> {
    let r = crate::audio::with_activity(app, |env, activity| {
        env.with_local_frame(4, |env| {
            let name = env.new_string("batterymanager")?; // Context.BATTERY_SERVICE
            let manager = env
                .call_method(
                    activity,
                    jni_str!("getSystemService"),
                    jni_sig!("(Ljava/lang/String;)Ljava/lang/Object;"),
                    &[JValue::Object(&name)],
                )?
                .l()?;
            if manager.is_null() {
                return Ok(None);
            }
            let percent = env
                .call_method(
                    &manager,
                    jni_str!("getIntProperty"),
                    jni_sig!("(I)I"),
                    &[JValue::Int(BATTERY_PROPERTY_CAPACITY)],
                )?
                .i()?;
            let charging = env.call_method(&manager, jni_str!("isCharging"), jni_sig!("()Z"), &[])?.z()?;
            // getIntProperty answers Integer.MIN_VALUE (or 0 on some devices) when unsupported.
            Ok((1..=100).contains(&percent).then(|| BatteryState { level: percent as f32 / 100.0, charging }))
        })
    });
    r.unwrap_or_else(|e: jni::errors::Error| {
        log::warn!("cannot read the headset battery: {e}");
        None
    })
}

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
