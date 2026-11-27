//! User settings, persisted as JSON in the platform config dir.

use openlink_protocol::Codec;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CodecPreference {
    /// Best codec both the server and the headset decoder support (AV1 > HEVC > H.264).
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
    /// The Steam Frame's USB wireless adapter: a direct 6 GHz link to the headset's own access
    /// point. The server joins it and answers headsets only there.
    SteamFrameAdapter,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub codec: CodecPreference,
    pub bitrate_mbps: u32,
    /// Encoded density at the centre of view, in percent of SteamVR's 100% render size (the
    /// panel enlarged for the lens, [`openlink_protocol::HeadsetInfo::encoded_size`]): 100 is
    /// 1:1. Applies live; SteamVR's render size does not depend on it. (Replaces
    /// `resolution_percent`, which was relative to the panel, so 100 there meant 0.65x here; an
    /// old value is ignored rather than reinterpreted.)
    pub center_resolution_percent: u32,
    /// Foveated encoding: percent of each eye's width and height, around the lens's center,
    /// encoded at full resolution; outside it resolution falls off to a quarter at the edges
    /// ([`openlink_protocol::foveation`]). 100 turns it off. Applies live. (Replaces
    /// `foveation_percent`: the same fraction of a 1.54x larger picture codes 2.4x the pixels,
    /// so an old value is not carried over either.)
    pub foveation_band_percent: u32,
    pub refresh_rate: f32,
    pub connection: ConnectionMode,
    /// Draw SteamVR's own layer (dashboard, laser, controllers) over a game's. Off where
    /// SteamVR leaves textures of that layer undrawn: they then cover the game, black.
    pub steamvr_layer_over_games: bool,
    /// Offer an "OpenLink" output device whose sound plays in the headset.
    pub speakers: bool,
    /// Offer an "OpenLink" microphone fed by the headset's.
    pub microphone: bool,
    /// Windows: the output device the headset's microphone plays into, by (part of) its name:
    /// a virtual audio cable, whose other end applications record from. Windows cannot create
    /// an "OpenLink" microphone without a kernel driver.
    pub microphone_device: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            codec: CodecPreference::Auto,
            bitrate_mbps: 200,
            // The centre 1:1 with SteamVR's 100% (Quest 3: 3172x3428 per eye at full resolution)...
            center_resolution_percent: 100,
            // ...and the periphery squeezed so the coded picture stays affordable (Quest 3:
            // 1746x1886 per eye, 6.6 MP for both: about 7.8 ms to encode on an RX 6800, which
            // fits 90 Hz; a 40% band's 8.9 MP took 10.2 ms and skipped frames).
            foveation_band_percent: 25,
            refresh_rate: 90.0,
            connection: ConnectionMode::WiFi,
            steamvr_layer_over_games: true,
            speakers: true,
            microphone: true,
            // VB-CABLE, the common free virtual cable.
            microphone_device: "CABLE Input".into(),
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
            .map(|mut s: Self| {
                // A codec this build cannot encode (an older version offered them all) shows as
                // Auto rather than as a selected, disabled entry.
                if let CodecPreference::Fixed(c) = s.codec {
                    if !crate::encoder::CODECS.contains(&c) {
                        s.codec = CodecPreference::Auto;
                    }
                }
                s
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
