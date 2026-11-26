//! User settings, persisted as JSON in the platform config dir.

use openlink_protocol::Codec;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CodecPreference {
    /// Best codec both the GPU encoder and the headset decoder support (AV1 > HEVC > H.264).
    Auto,
    Fixed(Codec),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConnectionMode {
    /// Headset reaches the PC over the LAN.
    WiFi,
    /// USB cable. The headset's USB NCM interface (or `adb forward` as a fallback) is just
    /// another IP route, so the session code is the same; only discovery differs.
    Usb,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub codec: CodecPreference,
    pub encoder: crate::encoder::BackendPreference,
    pub bitrate_mbps: u32,
    /// Multiplier on the headset's recommended per-eye resolution.
    pub resolution_scale: f32,
    pub refresh_rate: f32,
    pub connection: ConnectionMode,
    /// Offer an "OpenLink" output device whose sound plays in the headset.
    pub speakers: bool,
    /// Offer an "OpenLink" microphone fed by the headset's.
    pub microphone: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            codec: CodecPreference::Auto,
            encoder: crate::encoder::BackendPreference::Auto,
            bitrate_mbps: 200,
            // Per-eye size relative to the headset's recommended one. Quest runtimes recommend
            // less than panel resolution (Quest 3: 1680x1760 for a 2064x2208 panel).
            resolution_scale: 1.2,
            refresh_rate: 90.0,
            connection: ConnectionMode::WiFi,
            speakers: true,
            microphone: true,
        }
    }
}

impl Settings {
    pub fn path() -> Option<PathBuf> {
        dirs::config_dir().map(|d| d.join("openlink").join("settings.json"))
    }

    pub fn load() -> Self {
        Self::path()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| {
                serde_json::from_str(&s)
                    .map_err(|e| log::warn!("ignoring bad settings file: {e}"))
                    .ok()
            })
            .unwrap_or_default()
    }

    pub fn save(&self) -> std::io::Result<()> {
        let Some(p) = Self::path() else { return Ok(()) };
        if let Some(dir) = p.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(
            p,
            serde_json::to_string_pretty(self).expect("settings serialise"),
        )
    }
}
