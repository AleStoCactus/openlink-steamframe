//! Which codecs the server can encode, and the choice of one per session.
//!
//! The only encoder is Vulkan Video H.265 (`video/encode.rs`), on Linux and Windows alike, so
//! there is no backend to choose. AV1 and H.264 stay in the protocol and the codec setting so
//! that adding an encoder for them needs no wire or settings change.

use openlink_protocol::Codec;

/// Codecs the server can encode.
pub const CODECS: &[Codec] = &[Codec::H265];

/// Pick the codec for a session: the preferred one if both sides support it, else the best one
/// both sides support. A settings file may still prefer a codec this build cannot encode.
pub fn negotiate(encoder: &[Codec], decoder: &[Codec], preference: Option<Codec>) -> Option<Codec> {
    let both = |c: &Codec| encoder.contains(c) && decoder.contains(c);
    preference
        .filter(both)
        .or_else(|| [Codec::Av1, Codec::H265, Codec::H264].into_iter().find(both))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefers_av1_then_hevc() {
        let all = [Codec::H264, Codec::H265, Codec::Av1];
        assert_eq!(negotiate(&all, &all, None), Some(Codec::Av1));
        // e.g. RX 6800: no AV1 encode.
        assert_eq!(negotiate(&[Codec::H264, Codec::H265], &all, None), Some(Codec::H265));
        assert_eq!(negotiate(&all, &all, Some(Codec::H264)), Some(Codec::H264));
    }

    #[test]
    fn unsupported_preference_falls_back() {
        let all = [Codec::H264, Codec::H265, Codec::Av1];
        assert_eq!(negotiate(CODECS, &all, Some(Codec::Av1)), Some(Codec::H265));
        assert_eq!(negotiate(CODECS, &[Codec::H264], None), None);
    }
}
