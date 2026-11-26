//! Frame hand-off from the SteamVR driver to the server.
//!
//! The driver owns the compositor's eye textures and tells the server about them once, then
//! sends a small [`DriverVideoMsg::Frame`] per presented frame naming which textures hold it.
//! The server imports the textures into its encoder and sends [`ServerVideoMsg::Release`] once
//! its GPU work on a frame is done, so the driver does not hand a texture back to the
//! compositor while it is still being read.
//!
//! **Linux**: the textures are dmabufs. Transport is a `SOCK_SEQPACKET` Unix socket in the
//! abstract namespace (no filesystem path, so it works from inside the Steam Linux Runtime
//! container vrserver runs in). Each packet is one [`crate::encode`]d message; file descriptors
//! ride along as `SCM_RIGHTS`. The server waits on each dmabuf's implicit write fence.
//!
//! **Windows**: the textures are D3D11 textures with legacy (global) shared handles, which is
//! what SteamVR's `SharedTextureHandle_t` is there, so the handles travel inside the messages
//! and nothing rides along. D3D11 has no implicit fences: the driver signals a shared
//! `ID3D11Fence` ([`DriverVideoMsg::SyncFence`]) to a frame's id once the compositor is done
//! with it, and the server waits for that value. Transport is a loopback TCP connection
//! ([`VIDEO_PORT`]) with each message length-prefixed: Windows has no `SOCK_SEQPACKET`.

use serde::{Deserialize, Serialize};

#[cfg(target_os = "linux")]
#[path = "video_ipc/unix.rs"]
mod transport;
#[cfg(windows)]
#[path = "video_ipc/windows.rs"]
mod transport;

#[cfg(any(target_os = "linux", windows))]
pub use transport::{Attachment, Channel, Listener, RawAttachment};

/// Abstract socket name (without the leading NUL), Linux.
pub const SOCKET_NAME: &[u8] = b"openlink-video-v1";
/// Loopback TCP port of the video link, Windows.
pub const VIDEO_PORT: u16 = 9760;
/// Most file descriptors one message carries: every plane of every texture of a swap set.
pub const MAX_FDS: usize = 12;
/// Largest message we send; frames are ~200 bytes.
pub const MAX_MESSAGE: usize = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct DmabufPlane {
    pub offset: u32,
    pub stride: u32,
}

/// One texture of a swap set. Linux: one dmabuf fd per plane travels with the message, in
/// order; multiple planes come from modifiers with metadata planes (e.g. AMD DCC), not from YUV.
/// Windows: no planes; `handle` is the texture's global D3D11 shared handle.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DmabufTexture {
    /// SteamVR's `SharedTextureHandle_t` for this texture.
    pub handle: u64,
    pub planes: Vec<DmabufPlane>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TextureSet {
    /// Which GPU the textures live on, so the server opens the same one. Linux: `st_rdev` of the
    /// DRM render node. Windows: the DXGI adapter LUID (`HighPart << 32 | LowPart`).
    pub render_node: u64,
    pub width: u32,
    pub height: u32,
    /// `DRM_FORMAT_*` fourcc with the textures' memory layout (on Windows too, translated from
    /// the DXGI format, so the server has one format table).
    pub drm_format: u32,
    /// `DRM_FORMAT_MOD_*`; [`DRM_FORMAT_MOD_INVALID`] on Windows.
    pub modifier: u64,
    /// The format the compositor asked for (e.g. `R8G8B8A8_SRGB`): whether the bytes are
    /// sRGB-encoded. A `VkFormat` on Linux, a `DXGI_FORMAT` on Windows.
    pub vk_format: u32,
    pub textures: Vec<DmabufTexture>,
}

/// One eye of the frame's base (scene) layer.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct EyeLayer {
    pub texture: u64,
    /// `uMin, vMin, uMax, vMax` of the region that holds the image.
    pub bounds: [f32; 4],
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum DriverVideoMsg {
    /// First message on a connection. `pid`: vrserver's process id (Windows: the server
    /// duplicates [`DriverVideoMsg::SyncFence`]'s handle out of it).
    Hello { driver_version: String, pid: u32 },
    /// Linux: fds for every plane of `set.textures` follow as `SCM_RIGHTS`, in order.
    TextureSet(TextureSet),
    /// The textures with these handles are gone; drop any imports of them.
    TexturesDestroyed { handles: Vec<u64> },
    /// Windows: an NT handle, valid in the driver's process, to a shared `ID3D11Fence` on the
    /// textures' GPU. It reaches a frame's `frame_id` once the compositor's rendering into that
    /// frame's textures is done.
    SyncFence { handle: u64 },
    Frame {
        frame_id: u64,
        eyes: [EyeLayer; 2],
        /// SteamVR's own layer (dashboard, laser, overlays), to draw over an application's.
        overlay: Option<[EyeLayer; 2]>,
        /// Row-major 3x4 HMD pose the frame was rendered with.
        hmd_pose: [[f32; 4]; 3],
        /// The vsync this frame was presented for (`time::monotonic_ns`).
        present_ns: i64,
    },
}

/// `modifier` of a texture set whose layout is not given by a modifier but carried by the
/// buffer itself (its kernel metadata).
pub const DRM_FORMAT_MOD_INVALID: u64 = 0x00ff_ffff_ffff_ffff;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ServerVideoMsg {
    /// The server no longer reads the textures of this frame.
    Release { frame_id: u64 },
}
