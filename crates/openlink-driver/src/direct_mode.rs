//! `IVRDriverDirectModeComponent`: SteamVR's compositor renders into textures we own and hands
//! them to us every frame, instead of driving a physical display. This is how frames reach the
//! encoder, and on Linux it is also the only way the compositor starts at all for a headset
//! without a real (DRM-leasable) display.
//!
//! Linux: the driver allocates each texture as a dmabuf with GBM, using a DRM modifier the
//! compositor accepts, and hands it to SteamVR with `IVRIPCResourceManagerClient::ImportDmabuf`.
//! Because we chose the layout, the server can import the same dmabufs into its encoder
//! ([`crate::video`] passes the fds over). If GBM or the import is unavailable the textures are
//! allocated by SteamVR instead (`NewSharedVulkanImage`): the compositor still runs, but nothing
//! can be streamed.
//!
//! The driver also decides which texture of a swap set the compositor renders into next
//! (`GetNextSwapTextureSetIndex`), and skips textures the encoder has not released yet.
//!
//! Windows: not wired up yet (needs D3D11 shared textures created by the driver).

use crate::gbm;
use crate::host::host;
use crate::ovr::*;
use crate::video;
use openlink_protocol::time::monotonic_ns;
use openlink_protocol::video_ipc::{self as vipc, DmabufPlane, DmabufTexture};
use std::collections::VecDeque;
use std::ffi::{c_void, CString};
use std::os::fd::{AsRawFd, RawFd};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

#[repr(C)]
pub struct DirectModeObj {
    vtable: &'static IVRDriverDirectModeComponent_Vtbl,
}

impl Default for DirectModeObj {
    fn default() -> Self {
        Self { vtable: &VTBL }
    }
}

static VTBL: IVRDriverDirectModeComponent_Vtbl = IVRDriverDirectModeComponent_Vtbl {
    CreateSwapTextureSet: create_swap_texture_set,
    DestroySwapTextureSet: destroy_swap_texture_set,
    DestroyAllSwapTextureSets: destroy_all_swap_texture_sets,
    GetNextSwapTextureSetIndex: get_next_swap_texture_set_index,
    SubmitLayer: submit_layer,
    Present: present,
    PostPresent: post_present,
    GetFrameTiming: get_frame_timing,
};

const SET_SIZE: usize = 3;

struct Texture {
    handle: SharedTextureHandle_t,
    /// `Some` when we allocated it with GBM (the compositor's own textures).
    bo: Option<gbm::Bo>,
    /// SteamVR's dmabuf for an image it allocated (applications' textures); its layout is in
    /// the buffer's kernel metadata rather than a modifier.
    shared: Option<std::os::fd::OwnedFd>,
    /// Newest frame the server is still reading this texture for.
    busy: Option<u64>,
}

struct TextureSet {
    pid: u32,
    /// Allocated for SteamVR's compositor (rather than an application).
    compositor: bool,
    desc: SwapTextureSetDesc_t,
    drm_format: u32,
    textures: Vec<Texture>,
}

impl TextureSet {
    fn streamable(&self) -> bool {
        self.textures.iter().all(|t| t.bo.is_some() || t.shared.is_some())
    }

    /// The announcement for the server, and the fds that go with it.
    fn announcement(&self, render_node: u64) -> Option<(vipc::TextureSet, Vec<RawFd>)> {
        if !self.streamable() {
            return None;
        }
        let mut fds = Vec::new();
        let textures = self
            .textures
            .iter()
            .map(|t| match (&t.bo, &t.shared) {
                (Some(bo), _) => {
                    fds.extend(bo.planes.iter().map(|p| p.fd.as_raw_fd()));
                    DmabufTexture {
                        handle: t.handle,
                        planes: bo.planes.iter().map(|p| DmabufPlane { offset: p.offset, stride: p.stride }).collect(),
                    }
                }
                (None, Some(fd)) => {
                    fds.push(fd.as_raw_fd());
                    DmabufTexture { handle: t.handle, planes: vec![DmabufPlane { offset: 0, stride: 0 }] }
                }
                (None, None) => unreachable!("streamable() checked"),
            })
            .collect();
        let set = vipc::TextureSet {
            render_node,
            width: self.desc.nWidth,
            height: self.desc.nHeight,
            drm_format: self.drm_format,
            modifier: self.textures[0].bo.as_ref().map_or(vipc::DRM_FORMAT_MOD_INVALID, |bo| bo.modifier),
            vk_format: self.desc.nFormat,
            textures,
        };
        Some((set, fds))
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct CompositorStats {
    pub presents_per_s: f32,
    pub layers_per_frame: f32,
    pub width: u32,
    pub height: u32,
    pub vk_format: u32,
    pub frames_sent: u64,
    /// Times every texture of a set was still held by the encoder.
    pub starved: u64,
}

enum Gbm {
    Untried,
    Failed,
    Ready(gbm::Device),
}

struct Compositor {
    last_layer_shape: Option<Vec<bool>>,
    sets: Vec<TextureSet>,
    pending: Vec<[SubmitLayerPerEye_t; 2]>,
    presents: VecDeque<Instant>,
    layer_counts: VecDeque<usize>,
    last_log: Option<Instant>,
    gbm: Gbm,
    next_frame_id: u64,
    frames_sent: u64,
    starved: u64,
    /// Texture of the last streamed frame's left eye, and how many presents brought a new one
    /// (a new frame from the game or compositor) versus repeated the last.
    last_texture: SharedTextureHandle_t,
    new_content: u32,
    repeated_content: u32,
}

impl Default for Compositor {
    fn default() -> Self {
        Self {
            last_layer_shape: None,
            last_texture: 0,
            new_content: 0,
            repeated_content: 0,
            sets: Vec::new(),
            pending: Vec::new(),
            presents: VecDeque::new(),
            layer_counts: VecDeque::new(),
            last_log: None,
            gbm: Gbm::Untried,
            next_frame_id: 1,
            frames_sent: 0,
            starved: 0,
        }
    }
}

static COMPOSITOR: Mutex<Option<Compositor>> = Mutex::new(None);

/// Lock order: `COMPOSITOR` before `video`'s channel.
fn with<R>(f: impl FnOnce(&mut Compositor) -> R) -> R {
    let mut g = COMPOSITOR.lock().unwrap();
    f(g.get_or_insert_with(Compositor::default))
}

pub fn stats() -> CompositorStats {
    with(|c| {
        let window = Duration::from_secs(1);
        let now = Instant::now();
        let n = c.presents.iter().filter(|t| now - **t <= window).count();
        let set = c.sets.last();
        CompositorStats {
            presents_per_s: n as f32,
            layers_per_frame: if c.layer_counts.is_empty() {
                0.0
            } else {
                c.layer_counts.iter().sum::<usize>() as f32 / c.layer_counts.len() as f32
            },
            width: set.map_or(0, |s| s.desc.nWidth),
            height: set.map_or(0, |s| s.desc.nHeight),
            vk_format: set.map_or(0, |s| s.desc.nFormat),
            frames_sent: c.frames_sent,
            starved: c.starved,
        }
    })
}

/// Called by [`crate::video`] when the server connects: `send` gets every current set, then
/// `publish` runs, all under the compositor lock, so a set created meanwhile is either in the
/// announcement or sent through the published channel, never neither.
pub(crate) fn announce_all(send: &mut dyn FnMut(&vipc::TextureSet, &[RawFd]), publish: impl FnOnce()) {
    with(|c| {
        let rdev = match &c.gbm {
            Gbm::Ready(d) => d.rdev(),
            _ => 0,
        };
        for s in &mut c.sets {
            s.textures.iter_mut().for_each(|t| t.busy = None);
            if let Some((set, fds)) = s.announcement(rdev) {
                send(&set, &fds);
            }
        }
        publish();
    })
}

/// The server finished with `frame_id` (and every frame before it).
pub(crate) fn release(frame_id: u64) {
    with(|c| {
        for t in c.sets.iter_mut().flat_map(|s| &mut s.textures) {
            if t.busy.is_some_and(|b| b <= frame_id) {
                t.busy = None;
            }
        }
    })
}

/// The server went away: nothing is held any more.
pub(crate) fn release_all() {
    with(|c| {
        for t in c.sets.iter_mut().flat_map(|s| &mut s.textures) {
            t.busy = None;
        }
    })
}

const fn fourcc(s: &[u8; 4]) -> u32 {
    u32::from_le_bytes(*s)
}

/// The DRM format with the same memory layout as a compositor `VkFormat`.
fn drm_format_for(vk_format: u32) -> Option<u32> {
    Some(match vk_format {
        37 | 43 => fourcc(b"AB24"), // R8G8B8A8_UNORM / _SRGB
        44 | 50 => fourcc(b"AR24"), // B8G8R8A8_UNORM / _SRGB
        64 => fourcc(b"AB30"),      // A2B10G10R10_UNORM_PACK32
        58 => fourcc(b"AR30"),      // A2R10G10B10_UNORM_PACK32
        _ => return None,
    })
}

fn render_node() -> CString {
    let path = std::env::var("OPENLINK_RENDER_NODE").unwrap_or_else(|_| "/dev/dri/renderD128".into());
    CString::new(path).unwrap_or_else(|_| c"/dev/dri/renderD128".into())
}

/// Allocates `desc` as dmabufs and imports them into SteamVR. `None` (with a log line) when
/// this path is unavailable; the caller then falls back to SteamVR's own allocation.
/// `allow_compression`: AMD DCC-compressed modifiers are fine for the compositor, but
/// applications (games under Proton) reject textures that use them.
fn create_dmabuf_textures(
    c: &mut Compositor,
    desc: &SwapTextureSetDesc_t,
    allow_compression: bool,
) -> Option<(u32, Vec<Texture>)> {
    let h = host();
    if matches!(c.gbm, Gbm::Untried) {
        let node = render_node();
        c.gbm = match gbm::Device::open(&node) {
            Ok(d) => {
                log::info!("allocating compositor textures with GBM on {}", node.to_string_lossy());
                Gbm::Ready(d)
            }
            Err(e) => {
                log::warn!("GBM unavailable ({e}); frames cannot be streamed");
                Gbm::Failed
            }
        };
    }
    let Gbm::Ready(dev) = &c.gbm else { return None };
    if desc.nSampleCount > 1 {
        log::warn!("multisampled swap textures ({}x) cannot be streamed", desc.nSampleCount);
        return None;
    }
    let Some(drm_format) = drm_format_for(desc.nFormat) else {
        log::warn!("no DRM format for VkFormat {}; this set cannot be streamed", desc.nFormat);
        return None;
    };
    let mut modifiers = h.dmabuf_modifiers(drm_format);
    if !allow_compression {
        modifiers.retain(|&m| !amd_dcc(m));
    }
    if modifiers.is_empty() {
        log::warn!("compositor reports no dmabuf modifiers for format {drm_format:#x}");
        return None;
    }

    let mut textures = Vec::with_capacity(SET_SIZE);
    for _ in 0..SET_SIZE {
        let Some(bo) = dev.create(desc.nWidth, desc.nHeight, drm_format, &modifiers) else {
            log::warn!("GBM allocation of {}x{} failed", desc.nWidth, desc.nHeight);
            release_handles(&textures);
            return None;
        };
        if bo.planes.len() > MaxDmabufPlaneCount {
            log::warn!("{} planes is more than SteamVR accepts", bo.planes.len());
            release_handles(&textures);
            return None;
        }
        let mut attrs = DmabufAttributes_t {
            pNext: std::ptr::null_mut(),
            unWidth: desc.nWidth,
            unHeight: desc.nHeight,
            unDepth: 1,
            unMipLevels: 1,
            unArrayLayers: 1,
            unSampleCount: 1,
            unFormat: drm_format,
            ulModifier: bo.modifier,
            unPlaneCount: bo.planes.len() as u32,
            plane: [DmabufPlane_t::default(); MaxDmabufPlaneCount],
        };
        for (dst, p) in attrs.plane.iter_mut().zip(&bo.planes) {
            *dst = DmabufPlane_t { unOffset: p.offset, unStride: p.stride, nFd: p.fd.as_raw_fd() };
        }
        let Some(handle) = h.import_dmabuf(&mut attrs) else {
            log::warn!("ImportDmabuf refused modifier {:#x}", bo.modifier);
            release_handles(&textures);
            return None;
        };
        textures.push(Texture { handle, bo: Some(bo), shared: None, busy: None });
    }
    Some((drm_format, textures))
}

fn release_handles(textures: &[Texture]) {
    let h = host();
    textures.iter().for_each(|t| h.unref_resource(t.handle));
}

/// SteamVR allocates the images itself. The compositor runs, but the server cannot read them.
fn create_steamvr_textures(desc: &SwapTextureSetDesc_t) -> Option<Vec<Texture>> {
    let h = host();
    let mut textures = Vec::with_capacity(SET_SIZE);
    for _ in 0..SET_SIZE {
        match h.new_shared_vulkan_image(desc.nFormat, desc.nWidth, desc.nHeight) {
            Some(handle) => {
                // Its dmabuf, so the server can read it (an application's layer).
                let shared = h.shared_fd(handle);
                if shared.is_none() {
                    log::warn!("no fd for SteamVR image {handle:#x}; this set cannot be streamed");
                }
                textures.push(Texture { handle, bo: None, shared, busy: None })
            }
            None => {
                release_handles(&textures);
                return None;
            }
        }
    }
    Some(textures)
}

/// Whether an AMD format modifier uses DCC compression (`AMD_FMT_MOD_DCC`, bit 13 of an
/// `AMD_FMT_MOD` modifier, vendor 0x02 in the top byte).
fn amd_dcc(modifier: u64) -> bool {
    modifier >> 56 == 0x02 && modifier & (1 << 13) != 0
}

/// Whether `pid` is SteamVR's compositor (`vrcompositor`).
fn is_compositor(pid: u32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/comm")).is_ok_and(|c| c.trim() == "vrcompositor")
}

unsafe extern "C" fn create_swap_texture_set(
    _this: *mut c_void,
    pid: u32,
    desc: *const SwapTextureSetDesc_t,
    out: *mut SwapTextureSet_t,
) {
    crate::guard((), || {
        let desc = *desc;
        *out = SwapTextureSet_t::default();
        with(|c| {
            // Only the compositor can use our GBM dmabufs: applications (e.g. games under
            // Proton) reject them, compressed or not, and ask again in an endless loop.
            let compositor = is_compositor(pid);
            let dmabuf = if compositor { create_dmabuf_textures(c, &desc, true) } else { None };
            let (drm_format, textures) = match dmabuf {
                Some(t) => t,
                None => match create_steamvr_textures(&desc) {
                    Some(t) => (drm_format_for(desc.nFormat).unwrap_or(0), t),
                    None => {
                        log::error!(
                            "cannot allocate swap textures {}x{} format {} (pid {pid})",
                            desc.nWidth,
                            desc.nHeight,
                            desc.nFormat
                        );
                        return;
                    }
                },
            };
            let set = TextureSet { pid, compositor: is_compositor(pid), desc, drm_format, textures };
            log::info!(
                "swap texture set for pid {pid}: {}x{} format {} -> {:x?} ({})",
                desc.nWidth,
                desc.nHeight,
                desc.nFormat,
                set.textures.iter().map(|t| t.handle).collect::<Vec<_>>(),
                match set.textures[0].bo.as_ref() {
                    Some(bo) => format!("dmabuf, modifier {:#x}, {} plane(s)", bo.modifier, bo.planes.len()),
                    None if set.streamable() => "SteamVR-allocated, shared as dmabuf".into(),
                    None => "SteamVR-allocated, not streamable".into(),
                }
            );
            for (dst, t) in (*out).rSharedTextureHandles.iter_mut().zip(&set.textures) {
                *dst = t.handle;
            }
            let rdev = match &c.gbm {
                Gbm::Ready(d) => d.rdev(),
                _ => 0,
            };
            if let Some((ann, fds)) = set.announcement(rdev) {
                video::send_set(&ann, &fds);
            }
            c.sets.push(set);
        })
    })
}

fn remove_sets(pred: impl Fn(&TextureSet) -> bool) {
    with(|c| {
        let (gone, keep): (Vec<_>, Vec<_>) = c.sets.drain(..).partition(|s| pred(s));
        c.sets = keep;
        if gone.is_empty() {
            return;
        }
        let handles: Vec<u64> = gone.iter().flat_map(|s| s.textures.iter().map(|t| t.handle)).collect();
        log::info!("releasing swap textures {handles:x?}");
        video::send_destroyed(handles);
        // Dropping the bos closes our fds; the server and SteamVR hold their own references.
        gone.iter().for_each(|s| release_handles(&s.textures));
    })
}

unsafe extern "C" fn destroy_swap_texture_set(_this: *mut c_void, handle: SharedTextureHandle_t) {
    crate::guard((), || remove_sets(|s| s.textures.iter().any(|t| t.handle == handle)))
}

unsafe extern "C" fn destroy_all_swap_texture_sets(_this: *mut c_void, pid: u32) {
    crate::guard((), || remove_sets(|s| s.pid == pid))
}

unsafe extern "C" fn get_next_swap_texture_set_index(
    _this: *mut c_void,
    handles: *const SharedTextureHandle_t,
    indices: *mut [u32; 2],
) {
    crate::guard((), || {
        let handles = std::slice::from_raw_parts(handles, 2);
        let idx = &mut *indices;
        with(|c| {
            for eye in 0..2 {
                let cur = idx[eye] as usize % SET_SIZE;
                let Some(set) = c.sets.iter().find(|s| s.textures.iter().any(|t| t.handle == handles[eye])) else {
                    idx[eye] = ((cur + 1) % SET_SIZE) as u32;
                    continue;
                };
                let free = (1..SET_SIZE)
                    .map(|k| (cur + k) % SET_SIZE)
                    .find(|&i| set.textures[i].busy.is_none());
                idx[eye] = match free {
                    Some(i) => i as u32,
                    None => {
                        // Every other texture is still being encoded; overwriting one tears that
                        // frame, which beats stalling the compositor.
                        c.starved += 1;
                        ((cur + 1) % SET_SIZE) as u32
                    }
                };
            }
        })
    })
}

unsafe extern "C" fn submit_layer(_this: *mut c_void, per_eye: *const [SubmitLayerPerEye_t; 2]) {
    let layer = *per_eye;
    with(|c| c.pending.push(layer));
}

/// Hands the base layer of the frame to the server, marking its textures busy.
fn send_frame(c: &mut Compositor, base: &[SubmitLayerPerEye_t; 2], now_ns: i64) {
    let streamable = |h: SharedTextureHandle_t| {
        c.sets.iter().any(|s| s.streamable() && s.textures.iter().any(|t| t.handle == h))
    };
    if !base.iter().all(|e| streamable(e.hTexture)) {
        return;
    }
    let frame_id = c.next_frame_id;
    let eye = |e: &SubmitLayerPerEye_t| vipc::EyeLayer {
        texture: e.hTexture,
        bounds: [e.bounds.uMin, e.bounds.vMin, e.bounds.uMax, e.bounds.vMax],
    };
    let msg = vipc::DriverVideoMsg::Frame {
        frame_id,
        eyes: [eye(&base[0]), eye(&base[1])],
        hmd_pose: crate::render_pose::correct(&base[0].mHmdPose, VSYNC_PERIOD_NS.load(Ordering::Acquire)),
        present_ns: target_vsync_ns(now_ns),
    };
    if !video::send_frame(&msg) {
        return;
    }
    c.next_frame_id += 1;
    c.frames_sent += 1;
    for t in c.sets.iter_mut().flat_map(|s| &mut s.textures) {
        if base.iter().any(|e| e.hTexture == t.handle) {
            t.busy = Some(frame_id);
        }
    }
}

unsafe extern "C" fn present(_this: *mut c_void, _sync_texture: SharedTextureHandle_t) {
    crate::guard((), || {
        let now = Instant::now();
        let now_ns = monotonic_ns();
        let should_log = with(|c| {
            let layers = std::mem::take(&mut c.pending);
            // Stream the first layer we can read (an application's own layer uses textures
            // SteamVR allocated, which we cannot).
            let streamable = |h: SharedTextureHandle_t| {
                c.sets.iter().any(|s| s.streamable() && s.textures.iter().any(|t| t.handle == h))
            };
            // Diagnostics: log the layer layout whenever it changes.
            let layout: Vec<(u64, u64, bool)> =
                layers.iter().map(|l| (l[0].hTexture >> 32, l[1].hTexture >> 32, streamable(l[0].hTexture))).collect();
            let shape: Vec<bool> = layout.iter().map(|l| l.2).collect();
            if c.last_layer_shape.as_ref() != Some(&shape) {
                log::info!("layers this frame (left/right texture index, streamable): {layout:x?}");
                c.last_layer_shape = Some(shape);
            }
            // The scene is the largest application layer. Applications also submit small extra
            // layers (Beat Saber a 16x16 one), and the compositor's own layer then holds only
            // overlays: streaming "the first readable layer" showed those, every few frames.
            // Without an application, the compositor's layer is the scene.
            let area = |s: &TextureSet| s.desc.nWidth as u64 * s.desc.nHeight as u64;
            let eye_area = c.sets.iter().filter(|s| s.compositor).map(area).max().unwrap_or(0);
            let rank = |h: SharedTextureHandle_t| {
                c.sets.iter().find(|s| s.textures.iter().any(|t| t.handle == h)).map_or((false, 0), |s| {
                    // An application layer is the scene only if it is eye-sized (a game's
                    // placeholder layers while it loads are tiny).
                    (!s.compositor && area(s) * 4 >= eye_area, area(s))
                })
            };
            if let Some(base) = layers
                .iter()
                .filter(|l| l.iter().all(|e| streamable(e.hTexture)))
                .max_by_key(|l| rank(l[0].hTexture))
            {
                if base[0].hTexture == c.last_texture {
                    c.repeated_content += 1;
                } else {
                    c.new_content += 1;
                    c.last_texture = base[0].hTexture;
                }
                send_frame(c, base, now_ns);
            }
            c.layer_counts.push_back(layers.len());
            c.presents.push_back(now);
            while c.presents.len() > 512 {
                c.presents.pop_front();
            }
            while c.layer_counts.len() > 128 {
                c.layer_counts.pop_front();
            }
            let due = c.last_log.is_none_or(|t| now - t > Duration::from_secs(5));
            if due {
                if let Some(t) = c.last_log {
                    let secs = (now - t).as_secs_f32();
                    log::info!(
                        "content: {:.1} new frames/s, {:.1} repeats/s (the streamed layer's texture unchanged since the last present)",
                        c.new_content as f32 / secs,
                        c.repeated_content as f32 / secs
                    );
                }
                (c.new_content, c.repeated_content) = (0, 0);
                c.last_log = Some(now);
            }
            due
        });
        if should_log {
            let s = stats();
            log::info!(
                "compositor: {:.0} presents/s, {:.1} layers/frame, {}x{} format {}, {} frames to encoder, {} starved",
                s.presents_per_s,
                s.layers_per_frame,
                s.width,
                s.height,
                s.vk_format,
                s.frames_sent,
                s.starved
            );
        }
    })
}

unsafe extern "C" fn post_present(_this: *mut c_void, _throttling: *const Throttling_t) {}

unsafe extern "C" fn get_frame_timing(_this: *mut c_void, timing: *mut DriverDirectMode_FrameTiming) {
    // Per the header: clear the reprojection flags or they are read as throttling requests.
    (*timing).m_nReprojectionFlags = 0;
}

/// When the last vsync event fired and the period, for stamping frames with the vsync they
/// were presented for.
static LAST_VSYNC_NS: AtomicI64 = AtomicI64::new(0);
static VSYNC_PERIOD_NS: AtomicI64 = AtomicI64::new(0);
/// Pending one-shot shift of the next vsync (nanoseconds, + = later), from the headset's phase
/// measurement.
pub static VSYNC_SHIFT_NS: AtomicI64 = AtomicI64::new(0);

/// The vsync a frame presented at `now_ns` is for: SteamVR's compositor starts at a vsync and
/// presents for the next one, which is also where SteamVR's `SecondsFromVsyncToPhotons` counts
/// from.
fn target_vsync_ns(now_ns: i64) -> i64 {
    let (last, period) = (LAST_VSYNC_NS.load(Ordering::Acquire), VSYNC_PERIOD_NS.load(Ordering::Acquire));
    if last == 0 || period <= 0 || now_ns < last {
        return now_ns;
    }
    last + ((now_ns - last) / period + 1) * period
}

/// Without a physical display SteamVR has no vsync, so the driver supplies it.
pub struct VsyncThread {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl VsyncThread {
    pub fn start(refresh_hz: impl Fn() -> f32 + Send + 'static) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let stop = stop.clone();
            std::thread::Builder::new()
                .name("openlink-vsync".into())
                .spawn(move || {
                    let mut next = Instant::now();
                    while !stop.load(Ordering::Acquire) {
                        let period = Duration::from_secs_f64(1.0 / refresh_hz().clamp(30.0, 240.0) as f64);
                        next += period;
                        // Phase correction from the headset (one-shot, signed).
                        let shift = VSYNC_SHIFT_NS.swap(0, Ordering::AcqRel);
                        if shift > 0 {
                            next += Duration::from_nanos(shift as u64);
                        } else if shift < 0 {
                            next = next.checked_sub(Duration::from_nanos((-shift) as u64)).unwrap_or(next);
                        }
                        let now = Instant::now();
                        if next > now {
                            std::thread::sleep(next - now);
                        } else {
                            next = now;
                        }
                        LAST_VSYNC_NS.store(monotonic_ns(), Ordering::Release);
                        VSYNC_PERIOD_NS.store(period.as_nanos() as i64, Ordering::Release);
                        host().vsync_event(0.0);
                    }
                })
                .expect("spawn vsync thread")
        };
        Self { stop, thread: Some(thread) }
    }
}

impl Drop for VsyncThread {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}
