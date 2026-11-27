//! Keeps the headset's Wi-Fi out of power-save while the app runs.
//!
//! In power-save the radio sleeps between beacons and the router holds packets for it, then
//! releases them in a clump: on a strong link (-36 dBm, 2.4 Gbit/s) a Quest 3 showed only
//! 50-70 of 90 pictures a second, with several frames arriving at once and overflowing the
//! decoder queue (each overflow waits for an IDR), and audio dropping out with them, even at
//! 20 Mbit/s. `WIFI_MODE_FULL_LOW_LATENCY` disables power-save (and favours latency in the
//! driver) while the app is in the foreground with the screen on, which is when it streams.

use android_activity::AndroidApp;
use jni::objects::JValue;
use jni::{jni_sig, jni_str};

/// `WifiManager.WIFI_MODE_FULL_LOW_LATENCY` (API 29).
const WIFI_MODE_FULL_LOW_LATENCY: i32 = 4;

/// Acquires the lock for the rest of the process. Its global reference is leaked on purpose,
/// so the lock is never garbage collected (which would release it).
pub fn hold_low_latency_lock(app: &AndroidApp) {
    let r = crate::audio::with_activity(app, |env, activity| {
        env.with_local_frame(4, |env| {
            let name = env.new_string("wifi")?; // Context.WIFI_SERVICE
            let manager = env
                .call_method(
                    activity,
                    jni_str!("getSystemService"),
                    jni_sig!("(Ljava/lang/String;)Ljava/lang/Object;"),
                    &[JValue::Object(&name)],
                )?
                .l()?;
            if manager.is_null() {
                return Ok(false);
            }
            let tag = env.new_string("OpenLink")?;
            let lock = env
                .call_method(
                    &manager,
                    jni_str!("createWifiLock"),
                    jni_sig!("(ILjava/lang/String;)Landroid/net/wifi/WifiManager$WifiLock;"),
                    &[JValue::Int(WIFI_MODE_FULL_LOW_LATENCY), JValue::Object(&tag)],
                )?
                .l()?;
            env.call_method(&lock, jni_str!("acquire"), jni_sig!("()V"), &[])?;
            std::mem::forget(env.new_global_ref(&lock)?);
            Ok(true)
        })
    });
    match r {
        Ok(true) => log::info!("Wi-Fi: low-latency lock held (no power-save while streaming)"),
        Ok(false) => log::warn!("Wi-Fi: no WifiManager; power-save stays on"),
        Err(e) => log::warn!("Wi-Fi: cannot take the low-latency lock: {e}; power-save stays on"),
    }
}
