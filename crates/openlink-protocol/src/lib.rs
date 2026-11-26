//! OpenLink wire protocol.
//!
//! Three links use these types:
//!
//! * **headset <-> server**: [`HeadsetMessage`] / [`ServerMessage`] over UDP (tracking, input,
//!   haptics, clock sync), and the [`video`] stream on the same socket.
//! * **server <-> SteamVR driver**: [`ToDriver`] / [`FromDriver`] over loopback UDP. The driver
//!   lives inside `vrserver` and never touches the network itself; it only receives tracking that
//!   the server has already converted into the PC clock domain.
//!
//! * **audio**, both ways on the headset socket: [`audio`] (game audio to the headset, its
//!   microphone back).
//! * **driver -> server video**: [`video_ipc`], the compositor's textures (dmabufs on Linux,
//!   D3D11 shared textures on Windows) and per-frame metadata.
//!
//! Every datagram is `[MAGIC: 4][PROTOCOL_VERSION: u16 LE][postcard payload]`.

pub mod audio;
pub mod clock;
pub mod foveation;
pub mod net;
pub mod time;
pub mod video;
pub mod video_ipc;

use serde::{de::DeserializeOwned, Deserialize, Serialize};

pub const MAGIC: [u8; 4] = *b"OLNK";
/// Bumped on any incompatible change to a message below.
pub const PROTOCOL_VERSION: u16 = 11;

/// [`HeadsetInfo::high_refresh_rates`] by model name, for the headset to probe before it has a
/// `HeadsetInfo`.
pub fn high_refresh_rates(model: &str) -> &'static [f32] {
    let m: String = model.to_ascii_lowercase().split_whitespace().collect();
    if m.contains("quest3") && !m.contains("quest3s") {
        &[144.0, 165.0, 207.0, 240.0]
    } else {
        &[]
    }
}

/// UDP port the server listens on for headsets.
pub const SERVER_PORT: u16 = 9757;
/// Loopback UDP port the SteamVR driver listens on for the server.
pub const DRIVER_PORT: u16 = 9758;
/// Loopback UDP port the server listens on for the driver.
pub const SERVER_DRIVER_PORT: u16 = 9759;

/// Largest datagram we will ever produce for control/tracking traffic.
pub const MAX_DATAGRAM: usize = 1400;

// ------------------------------------------------------------------------------------------------
// Shared value types
// ------------------------------------------------------------------------------------------------

/// Position in metres, orientation as a unit quaternion `[x, y, z, w]`.
///
/// Coordinate system is the one shared by OpenXR's STAGE/LOCAL spaces and OpenVR's driver space:
/// +X right, +Y up, -Z forward.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Pose {
    pub position: [f32; 3],
    pub orientation: [f32; 4],
}

impl Pose {
    pub const IDENTITY: Pose = Pose {
        position: [0.0; 3],
        orientation: [0.0, 0.0, 0.0, 1.0],
    };

    /// From a row-major 3x4 rigid transform (OpenVR's `HmdMatrix34_t`).
    pub fn from_matrix34(m: &[[f32; 4]; 3]) -> Pose {
        let (m00, m11, m22) = (m[0][0], m[1][1], m[2][2]);
        let trace = m00 + m11 + m22;
        let q = if trace > 0.0 {
            let s = (trace + 1.0).sqrt() * 2.0;
            [(m[2][1] - m[1][2]) / s, (m[0][2] - m[2][0]) / s, (m[1][0] - m[0][1]) / s, 0.25 * s]
        } else if m00 > m11 && m00 > m22 {
            let s = (1.0 + m00 - m11 - m22).sqrt() * 2.0;
            [0.25 * s, (m[0][1] + m[1][0]) / s, (m[0][2] + m[2][0]) / s, (m[2][1] - m[1][2]) / s]
        } else if m11 > m22 {
            let s = (1.0 + m11 - m00 - m22).sqrt() * 2.0;
            [(m[0][1] + m[1][0]) / s, 0.25 * s, (m[1][2] + m[2][1]) / s, (m[0][2] - m[2][0]) / s]
        } else {
            let s = (1.0 + m22 - m00 - m11).sqrt() * 2.0;
            [(m[0][2] + m[2][0]) / s, (m[1][2] + m[2][1]) / s, 0.25 * s, (m[1][0] - m[0][1]) / s]
        };
        let n = q.iter().map(|v| v * v).sum::<f32>().sqrt();
        Pose { position: [m[0][3], m[1][3], m[2][3]], orientation: q.map(|v| v / n) }
    }
}

/// A pose plus its first derivatives, as reported by the headset runtime **at the sample time**.
///
/// Velocities are expressed in the same base space as the pose (world/stage space), which is what
/// `XrSpaceVelocity` returns.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Motion {
    pub pose: Pose,
    /// m/s, world space.
    pub linear_velocity: [f32; 3],
    /// rad/s, axis * angle, world space.
    pub angular_velocity: [f32; 3],
    pub flags: MotionFlags,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct MotionFlags(pub u8);

impl MotionFlags {
    pub const POSITION_VALID: u8 = 1 << 0;
    pub const ORIENTATION_VALID: u8 = 1 << 1;
    pub const POSITION_TRACKED: u8 = 1 << 2;
    pub const ORIENTATION_TRACKED: u8 = 1 << 3;
    pub const LINEAR_VELOCITY_VALID: u8 = 1 << 4;
    pub const ANGULAR_VELOCITY_VALID: u8 = 1 << 5;
    pub const ALL: MotionFlags = MotionFlags(0b11_1111);

    pub fn has(self, bit: u8) -> bool {
        self.0 & bit != 0
    }
    pub fn is_tracked(self) -> bool {
        self.has(Self::POSITION_TRACKED) && self.has(Self::ORIENTATION_TRACKED)
    }
    pub fn is_valid(self) -> bool {
        self.has(Self::POSITION_VALID) && self.has(Self::ORIENTATION_VALID)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Hand {
    Left = 0,
    Right = 1,
}

/// Button bits for Touch / Touch Plus / Touch Pro controllers.
pub mod buttons {
    pub const A_X_CLICK: u32 = 1 << 0;
    pub const A_X_TOUCH: u32 = 1 << 1;
    pub const B_Y_CLICK: u32 = 1 << 2;
    pub const B_Y_TOUCH: u32 = 1 << 3;
    pub const MENU_CLICK: u32 = 1 << 4;
    pub const THUMBSTICK_CLICK: u32 = 1 << 5;
    pub const THUMBSTICK_TOUCH: u32 = 1 << 6;
    pub const THUMBREST_TOUCH: u32 = 1 << 7;
    pub const TRIGGER_TOUCH: u32 = 1 << 8;
    pub const GRIP_TOUCH: u32 = 1 << 9;
}

#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct ControllerInput {
    /// Bitset of [`buttons`] constants.
    pub buttons: u32,
    pub trigger: f32,
    pub grip: f32,
    pub thumbstick: [f32; 2],
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ControllerState {
    pub motion: Motion,
    pub input: ControllerInput,
}

/// One tracking snapshot. All motions were sampled at `sample_time_ns` (sender's clock).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct TrackingSample {
    pub sample_time_ns: i64,
    pub hmd: Motion,
    pub controllers: [Option<ControllerState>; 2],
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Fov {
    /// Radians; left and down are negative, as in `XrFovf`.
    pub angle_left: f32,
    pub angle_right: f32,
    pub angle_up: f32,
    pub angle_down: f32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HeadsetInfo {
    /// e.g. "Meta Quest 3".
    pub model: String,
    pub serial: String,
    /// Recommended per-eye render size reported by the headset runtime.
    pub eye_width: u32,
    pub eye_height: u32,
    pub refresh_rates: Vec<f32>,
    pub current_refresh_rate: f32,
    pub ipd_m: f32,
    pub fov: [Fov; 2],
    /// Codecs the headset decoder can handle.
    pub decoders: Vec<Codec>,
}

impl HeadsetInfo {
    /// Per-eye panel resolution, which is what "100%" encoded resolution means.
    ///
    /// OpenXR only reports a *recommended* render size, which Quest runtimes set well below the
    /// panel (Quest 3: 1680x1760 for a 2064x2208 panel) to save GPU time in standalone apps.
    /// The panel size is not queryable, so it comes from the model name; an unknown headset
    /// falls back to its recommended size.
    pub fn panel_size(&self) -> (u32, u32) {
        let m: String = self.model.to_ascii_lowercase().split_whitespace().collect();
        if m.contains("quest3s") {
            (1832, 1920)
        } else if m.contains("quest3") {
            (2064, 2208)
        } else if m.contains("questpro") {
            (1800, 1920)
        } else if m.contains("quest2") {
            (1832, 1920)
        } else {
            (self.eye_width, self.eye_height)
        }
    }

    /// Refresh rates the model can run at beyond the common 72/80/90/120 Hz. Quest 3 on
    /// Horizon OS v2.7 and later: up to 207 Hz normally and 240 Hz in developer mode with display scaling on. Whether
    /// the headset really offers one right now is up to its runtime: the Quest app tries each
    /// and lists the ones accepted in [`refresh_rates`](Self::refresh_rates), and the stream
    /// uses the closest one listed.
    pub fn high_refresh_rates(&self) -> &'static [f32] {
        high_refresh_rates(&self.model)
    }

    /// Per-eye size to encode at `percent` of the panel resolution: even (video formats need
    /// that) and at least 2.
    pub fn encoded_size(&self, percent: u32) -> (u32, u32) {
        let (w, h) = self.panel_size();
        let scale = |v: u32| ((((v as u64 * percent as u64 + 50) / 100) as u32) & !1).max(2);
        (scale(w), scale(h))
    }

    /// Pixels per degree at the center of the view, as Meta publishes them per model.
    ///
    /// The lens magnifies the middle of the panel less than its edges, so the panel's pixels
    /// are densest (per degree) at the center. A flat render target covering the same FOV has
    /// to be considerably larger than the panel to match that density; this is why other
    /// headsets' 100% in SteamVR is well above their panel size (Index: 1.4x). The lens profile
    /// is not queryable through OpenXR, so it comes from the model name like [`panel_size`].
    ///
    /// [`panel_size`]: Self::panel_size
    pub fn center_ppd(&self) -> Option<f32> {
        let m: String = self.model.to_ascii_lowercase().split_whitespace().collect();
        if m.contains("quest3s") {
            Some(20.0)
        } else if m.contains("quest3") {
            Some(25.0)
        } else if m.contains("questpro") {
            Some(22.0)
        } else if m.contains("quest2") {
            Some(20.0)
        } else {
            None
        }
    }

    /// How much larger than the panel a flat (rectilinear) render target must be, per axis, to
    /// match the panel's pixel density at the center of the view after lens distortion.
    ///
    /// A rectilinear image `w` pixels wide spanning tangents `[l, r]` has `w / (r - l)` pixels
    /// per radian at its center, so the matching width is the center density times the tangent
    /// span. Quest 3 (FOV 54/40/44/55 degrees, 25 PPD): 3174x3429 for a 2064x2208 panel, 1.54x
    /// on both axes. Never below 1, and 1 for a headset whose lens is unknown.
    pub fn lens_scale(&self) -> (f32, f32) {
        let Some(ppd) = self.center_ppd() else {
            return (1.0, 1.0);
        };
        let px_per_rad = ppd * 180.0 / std::f32::consts::PI;
        // Both eyes get one render size, so the wider span of the two decides.
        let span = |a: fn(&Fov) -> f32, b: fn(&Fov) -> f32| {
            self.fov.iter().map(|f| a(f).tan() - b(f).tan()).fold(0.0f32, f32::max)
        };
        let width = span(|f| f.angle_right, |f| f.angle_left);
        let height = span(|f| f.angle_up, |f| f.angle_down);
        let (pw, ph) = self.panel_size();
        let scale = |span: f32, panel: u32| {
            let s = px_per_rad * span / panel as f32;
            if s.is_finite() { s.clamp(1.0, 2.0) } else { 1.0 }
        };
        (scale(width, pw), scale(height, ph))
    }

    /// The per-eye render size SteamVR should recommend (its 100%) for a stream of `encoded`
    /// per-eye pixels: the encoded size enlarged by [`lens_scale`](Self::lens_scale), as a
    /// native headset's 100% is its panel size enlarged for its lens. The server downsamples
    /// SteamVR's frames to the encoded size.
    pub fn render_size(&self, encoded: (u32, u32)) -> (u32, u32) {
        let (sx, sy) = self.lens_scale();
        let scale = |v: u32, s: f32| ((v as f32 * s).round() as u32).max(1);
        (scale(encoded.0, sx), scale(encoded.1, sy))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Codec {
    H264,
    H265,
    Av1,
}

/// One device's battery.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct BatteryState {
    /// Charge, 0.0 to 1.0.
    pub level: f32,
    pub charging: bool,
}

/// The headset's and controllers' batteries. `None` where the headset cannot tell: the
/// controllers' charge only reaches apps through an OpenXR extension not every runtime has,
/// and a controller that is off has no reading at all.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct Batteries {
    pub hmd: Option<BatteryState>,
    /// Left, right.
    pub controllers: [Option<BatteryState>; 2],
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct HapticPulse {
    pub hand: Hand,
    pub duration_s: f32,
    pub frequency_hz: f32,
    pub amplitude: f32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StreamConfig {
    pub codec: Codec,
    pub refresh_rate: f32,
    /// Encoded per-eye resolution before foveation: the size the headset displays at.
    pub eye_width: u32,
    pub eye_height: u32,
    /// Squeeze the edges of each eye before encoding (see [`foveation`]). Each frame carries the
    /// parameters it was encoded with; this is the current setting.
    pub foveation: Option<foveation::Foveation>,
    pub bitrate_bps: u64,
    /// The server streams game audio ([`audio::Stream::Speakers`]); the headset should play it.
    pub speakers: bool,
    /// The server wants the headset's microphone ([`audio::Stream::Microphone`]).
    pub microphone: bool,
}

impl StreamConfig {
    /// Per-eye size of the encoded picture: smaller than `eye_width` x `eye_height` when
    /// foveated.
    pub fn coded_eye_size(&self) -> (u32, u32) {
        let full = (self.eye_width, self.eye_height);
        self.foveation.map_or(full, |f| f.coded_eye_size())
    }
}

// ------------------------------------------------------------------------------------------------
// headset <-> server
// ------------------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum HeadsetMessage {
    Hello(HeadsetInfo),
    Tracking {
        seq: u32,
        sample: TrackingSample,
    },
    /// Reply to [`ServerMessage::ClockPing`]. `t1` = receive time, `t2` = send time, headset clock.
    ClockPong {
        t0: i64,
        t1: i64,
        t2: i64,
    },
    /// Sent every few seconds, so a lost datagram only delays the next reading.
    Battery(Batteries),
    /// Video frames were lost; the server should send an IDR. `frame` is the first number the
    /// headset knows it missed.
    VideoLoss {
        frame: u32,
    },
    /// A video frame reached the display for the first time. `present_ns` is echoed from
    /// [`video::FrameInfo`] (server clock); the rest are headset clock: frame complete off the
    /// network, handed to the decoder, decoded, photons. Together they give the true
    /// present-to-photon latency and where it goes.
    FrameDisplayed {
        present_ns: i64,
        received_ns: i64,
        queued_ns: i64,
        decoded_ns: i64,
        display_ns: i64,
    },
    /// The display's measured frame period (headset clock), more exact than its nominal rate.
    /// SteamVR is paced to it so frames are produced exactly as fast as they are shown.
    DisplayPeriod {
        period_ns: i64,
    },
    /// Shift SteamVR's vsync by this much (+ = later) so frames reach the display with a few
    /// milliseconds to spare: not so early that they wait a whole frame, not so late that
    /// jitter makes them miss it.
    VsyncShift {
        shift_ns: i64,
    },
    Disconnect,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ServerMessage {
    Welcome(StreamConfig),
    /// `t0` = server send time, server clock.
    ClockPing {
        t0: i64,
    },
    Haptics(HapticPulse),
    Disconnect,
}

// ------------------------------------------------------------------------------------------------
// server <-> SteamVR driver (loopback)
// ------------------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ToDriver {
    /// The headset, with `eye_width`/`eye_height` replaced by the render size SteamVR should
    /// recommend: the encoded per-eye size enlarged for the lens ([`HeadsetInfo::render_size`]).
    HeadsetConnected(HeadsetInfo),
    HeadsetDisconnected,
    /// `sample.sample_time_ns` has already been converted to the PC monotonic clock
    /// ([`time::monotonic_ns`]).
    Tracking(TrackingSample),
    Battery(Batteries),
    /// Measured time from `Present` to photons on the headset. SteamVR predicts poses this far
    /// past vsync, so what the user sees matches where things are when they see it.
    PhotonLatency {
        seconds: f32,
    },
    /// Pace SteamVR's vsync to this period (the headset display's measured one).
    DisplayPeriod {
        period_ns: i64,
    },
    /// Shift SteamVR's vsync once by this much (+ = later).
    VsyncShift {
        shift_ns: i64,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum FromDriver {
    /// Sent periodically so the server knows the driver is alive and where to send.
    Heartbeat,
    Haptics(HapticPulse),
}

// ------------------------------------------------------------------------------------------------
// Framing
// ------------------------------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum DecodeError {
    #[error("datagram too short")]
    TooShort,
    #[error("bad magic")]
    BadMagic,
    #[error("protocol version mismatch: peer {peer}, ours {PROTOCOL_VERSION}")]
    Version { peer: u16 },
    #[error("payload: {0}")]
    Payload(#[from] postcard::Error),
}

const HEADER_LEN: usize = 6;

pub fn encode<T: Serialize>(msg: &T) -> Vec<u8> {
    let mut out = Vec::with_capacity(128);
    out.extend_from_slice(&MAGIC);
    out.extend_from_slice(&PROTOCOL_VERSION.to_le_bytes());
    // Serialising our own plain-data types into a Vec cannot fail.
    postcard::to_io(msg, &mut out).expect("postcard serialisation into Vec");
    out
}

pub fn decode<T: DeserializeOwned>(buf: &[u8]) -> Result<T, DecodeError> {
    if buf.len() < HEADER_LEN {
        return Err(DecodeError::TooShort);
    }
    if buf[..4] != MAGIC {
        return Err(DecodeError::BadMagic);
    }
    let peer = u16::from_le_bytes([buf[4], buf[5]]);
    if peer != PROTOCOL_VERSION {
        return Err(DecodeError::Version { peer });
    }
    Ok(postcard::from_bytes(&buf[HEADER_LEN..])?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encoded_size_is_relative_to_the_panel() {
        let info = |model: &str| HeadsetInfo {
            model: model.into(),
            serial: String::new(),
            eye_width: 1680,
            eye_height: 1760,
            refresh_rates: vec![90.0],
            current_refresh_rate: 90.0,
            ipd_m: 0.063,
            fov: [Fov { angle_left: -0.9, angle_right: 0.8, angle_up: 0.8, angle_down: -0.9 }; 2],
            decoders: vec![Codec::H265],
        };
        assert_eq!(info("Meta Quest 3").encoded_size(100), (2064, 2208));
        assert_eq!(info("Meta Quest 3").encoded_size(50), (1032, 1104));
        assert_eq!(info("Meta Quest 3S").encoded_size(100), (1832, 1920));
        assert_eq!(info("Oculus Quest2").encoded_size(100), (1832, 1920));
        assert_eq!(info("Unknown HMD").encoded_size(100), (1680, 1760));
    }

    #[test]
    fn quest3_offers_high_refresh_rates() {
        let info = |model: &str| HeadsetInfo {
            model: model.into(),
            serial: String::new(),
            eye_width: 1680,
            eye_height: 1760,
            refresh_rates: vec![90.0],
            current_refresh_rate: 90.0,
            ipd_m: 0.063,
            fov: [Fov { angle_left: -0.9, angle_right: 0.8, angle_up: 0.8, angle_down: -0.9 }; 2],
            decoders: vec![Codec::H265],
        };
        assert_eq!(info("Meta Quest 3").high_refresh_rates(), &[144.0, 165.0, 207.0, 240.0]);
        assert!(info("Meta Quest 3S").high_refresh_rates().is_empty());
        assert!(info("Meta Quest 2").high_refresh_rates().is_empty());
    }

    #[test]
    fn render_size_corrects_for_the_lens() {
        // A Quest 3's FOV as its runtime reports it (left eye; the right mirrors it).
        let deg = |d: f32| d.to_radians();
        let left = Fov { angle_left: deg(-54.0), angle_right: deg(40.0), angle_up: deg(44.0), angle_down: deg(-55.0) };
        let right = Fov { angle_left: -left.angle_right, angle_right: -left.angle_left, ..left };
        let info = |model: &str| HeadsetInfo {
            model: model.into(),
            serial: String::new(),
            eye_width: 1680,
            eye_height: 1760,
            refresh_rates: vec![90.0],
            current_refresh_rate: 90.0,
            ipd_m: 0.063,
            fov: [left, right],
            decoders: vec![Codec::H265],
        };
        let q3 = info("Meta Quest 3");
        let (w, h) = q3.render_size(q3.encoded_size(100));
        assert!((3150..3200).contains(&w) && (3400..3460).contains(&h), "{w}x{h}");
        // It scales with the encoded size, so SteamVR's 100% follows the slider.
        let (w50, h50) = q3.render_size(q3.encoded_size(50));
        assert!(w50.abs_diff(w / 2) <= 1 && h50.abs_diff(h / 2) <= 1);
        // Unknown lens: render exactly what is streamed.
        let unknown = info("Unknown HMD");
        assert_eq!(unknown.render_size((1680, 1760)), (1680, 1760));
    }

    fn motion() -> Motion {
        Motion {
            pose: Pose {
                position: [0.1, 1.6, -0.2],
                orientation: [0.0, 0.0, 0.0, 1.0],
            },
            linear_velocity: [1.0, 0.0, 0.0],
            angular_velocity: [0.0, 6.0, 0.0],
            flags: MotionFlags::ALL,
        }
    }

    #[test]
    fn tracking_roundtrip_fits_in_one_datagram() {
        let ctrl = ControllerState {
            motion: motion(),
            input: ControllerInput::default(),
        };
        let msg = HeadsetMessage::Tracking {
            seq: 42,
            sample: TrackingSample {
                sample_time_ns: 123_456_789,
                hmd: motion(),
                controllers: [Some(ctrl), Some(ctrl)],
            },
        };
        let bytes = encode(&msg);
        assert!(bytes.len() < MAX_DATAGRAM, "{} bytes", bytes.len());
        assert_eq!(decode::<HeadsetMessage>(&bytes).unwrap(), msg);
    }

    #[test]
    fn pose_from_matrix() {
        // 90 degrees about +Y: x -> -z.
        let m = [[0.0, 0.0, 1.0, 1.0], [0.0, 1.0, 0.0, 2.0], [-1.0, 0.0, 0.0, 3.0]];
        let p = Pose::from_matrix34(&m);
        let h = std::f32::consts::FRAC_1_SQRT_2;
        let expected = [0.0, h, 0.0, h];
        assert!(p.orientation.iter().zip(expected).all(|(a, b)| (a - b).abs() < 1e-6), "{p:?}");
        assert_eq!(p.position, [1.0, 2.0, 3.0]);
    }

    #[test]
    fn rejects_other_versions() {
        let mut bytes = encode(&FromDriver::Heartbeat);
        bytes[4] = bytes[4].wrapping_add(1);
        assert!(matches!(
            decode::<FromDriver>(&bytes),
            Err(DecodeError::Version { .. })
        ));
    }
}
