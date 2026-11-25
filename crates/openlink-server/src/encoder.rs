//! Video encoder backends.
//!
//! Nothing is encoded yet (that is the video milestone); this module pins down the backend
//! matrix the GUI exposes, so settings files written today stay valid.
//!
//! | Backend        | OS            | GPUs                  | H.264 | HEVC | AV1                         |
//! |----------------|---------------|-----------------------|-------|------|-----------------------------|
//! | NVENC          | Windows/Linux | NVIDIA GTX 900+       | yes   | yes  | RTX 40+                     |
//! | AMF            | Windows       | AMD GCN+              | yes   | yes  | RX 7000+                    |
//! | VA-API         | Linux         | AMD (Mesa), Intel     | yes   | yes  | RX 7000+, Arc / Core Ultra  |
//! | Quick Sync     | Windows/Linux | Intel                 | yes   | yes  | Arc / Core Ultra            |
//! | Vulkan Video   | Windows/Linux | any with the KHR exts | yes   | yes  | driver dependent            |
//! | Software (x264)| any           | none                  | yes   | -    | -                           |
//!
//! AV1 *encode* is a hardware feature: on GPUs without it (e.g. RX 6000, RTX 30) the AV1 option
//! cannot be offered, by any streamer.

use openlink_protocol::Codec;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Backend {
    Nvenc,
    Amf,
    Vaapi,
    QuickSync,
    VulkanVideo,
    Software,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BackendPreference {
    Auto,
    Fixed(Backend),
}

impl Backend {
    pub const ALL: [Backend; 6] = [
        Backend::Nvenc,
        Backend::Amf,
        Backend::Vaapi,
        Backend::QuickSync,
        Backend::VulkanVideo,
        Backend::Software,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Backend::Nvenc => "NVENC (NVIDIA)",
            Backend::Amf => "AMF (AMD)",
            Backend::Vaapi => "VA-API (AMD/Intel)",
            Backend::QuickSync => "Quick Sync (Intel)",
            Backend::VulkanVideo => "Vulkan Video",
            Backend::Software => "Software (x264)",
        }
    }

    /// Whether the backend exists on this OS at all (not whether this machine's GPU has it).
    pub fn supported_on_this_os(self) -> bool {
        match self {
            Backend::Amf => cfg!(windows),
            Backend::Vaapi => cfg!(target_os = "linux"),
            _ => true,
        }
    }
}

/// Pick the codec for a session: the best one both sides support.
pub fn negotiate(encoder: &[Codec], decoder: &[Codec], preference: Option<Codec>) -> Option<Codec> {
    if let Some(p) = preference {
        return (encoder.contains(&p) && decoder.contains(&p)).then_some(p);
    }
    [Codec::Av1, Codec::H265, Codec::H264]
        .into_iter()
        .find(|c| encoder.contains(c) && decoder.contains(c))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefers_av1_then_hevc() {
        let all = [Codec::H264, Codec::H265, Codec::Av1];
        assert_eq!(negotiate(&all, &all, None), Some(Codec::Av1));
        // e.g. RX 6800: no AV1 encode.
        assert_eq!(
            negotiate(&[Codec::H264, Codec::H265], &all, None),
            Some(Codec::H265)
        );
        assert_eq!(negotiate(&[Codec::H264], &all, Some(Codec::Av1)), None);
    }
}
