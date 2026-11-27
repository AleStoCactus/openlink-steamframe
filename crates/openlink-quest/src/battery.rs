//! The headset's battery for SteamVR's dashboard, from Android. The controllers' come from
//! OpenXR, in `openlink_xr::battery`.

use android_activity::AndroidApp;
use jni::objects::JValue;
use jni::{jni_sig, jni_str};
use openlink_protocol::BatteryState;

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
