//! Windows backend: not written yet.
//!
//! Windows cannot create audio devices from user mode (unlike PipeWire): a new output or input
//! device in the Sound settings needs a kernel audio driver. The plan:
//!
//! * **Bundled virtual audio driver** (a signed SysVAD-style miniport exposing one render and
//!   one capture endpoint named "OpenLink"), installed with the SteamVR driver. The backend then
//!   reads what plays on the render endpoint and writes the microphone into the capture
//!   endpoint, both through WASAPI, and the rest is [`super::Bridge`] as on Linux.
//! * **Until then**, WASAPI loopback capture of a chosen output device for game audio (what
//!   other streamers do), and the microphone into a third-party virtual cable.
//!
//! The driver should also set `Prop_Audio_DefaultPlaybackDeviceId_String` and
//! `Prop_Audio_DefaultRecordingDeviceId_String` on the HMD to the endpoints' IDs, so SteamVR
//! switches Windows to the headset's audio while it runs, as it does for wired headsets.

use super::Bridge;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

pub fn run(_bridge: &Arc<Bridge>, _stop: &Arc<AtomicBool>) -> Result<(), String> {
    Err("headset audio is not available on Windows yet".into())
}
