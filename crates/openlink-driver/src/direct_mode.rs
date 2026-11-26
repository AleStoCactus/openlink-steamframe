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
//! Windows: the driver creates every swap texture (the compositor's and applications') as a
//! D3D11 texture with a legacy shared handle, which the server opens too; see [`platform`].
//!
//! Allocation and per-frame GPU sync are per platform ([`platform`]); choosing what to stream,
//! texture bookkeeping and vsync are shared.

use crate::host::host;
use crate::ovr::*;
use crate::video;
use openlink_protocol::time::monotonic_ns;
use openlink_protocol::video_ipc::{self as vipc, DriverVideoMsg, RawAttachment};
use std::collections::VecDeque;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

#[cfg(target_os = "linux")]
#[path = "direct_mode/linux.rs"]
pub(crate) mod platform;
#[cfg(windows)]
#[path = "direct_mode/windows.rs"]
pub(crate) mod platform;

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
    mem: platform::TextureMem,
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
        self.textures.iter().all(|t| t.mem.streamable())
    }

    /// The announcement for the server, and what rides along with it (Linux: the fds).
    fn announcement(&self, render_node: u64) -> Option<(vipc::TextureSet, Vec<RawAttachment>)> {
        if !self.streamable() {
            return None;
        }
        let mut attachments = Vec::new();
        let textures = self.textures.iter().map(|t| t.mem.describe(t.handle, &mut attachments)).collect();
        let set = vipc::TextureSet {
            render_node,
            width: self.desc.nWidth,
            height: self.desc.nHeight,
            drm_format: self.drm_format,
            modifier: self.textures[0].mem.modifier(),
            vk_format: self.desc.nFormat,
            textures,
        };
        Some((set, attachments))
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

struct Compositor {
    last_layer_shape: Option<Vec<bool>>,
    sets: Vec<TextureSet>,
    pending: Vec<[SubmitLayerPerEye_t; 2]>,
    presents: VecDeque<Instant>,
    layer_counts: VecDeque<usize>,
    last_log: Option<Instant>,
    gpu: platform::Gpu,
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
            gpu: platform::Gpu::default(),
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

/// Called by [`crate::video`] when the server connects: `send` gets the platform's setup
/// messages and every current set, then `publish` runs, all under the compositor lock, so a set
/// created meanwhile is either in the announcement or sent through the published channel,
/// never neither.
pub(crate) fn announce_all(send: &mut dyn FnMut(&DriverVideoMsg, &[RawAttachment]), publish: impl FnOnce()) {
    with(|c| {
        for msg in c.gpu.connect_messages() {
            send(&msg, &[]);
        }
        let rdev = c.gpu.render_node();
        for s in &mut c.sets {
            s.textures.iter_mut().for_each(|t| t.busy = None);
            if let Some((set, attachments)) = s.announcement(rdev) {
                send(&DriverVideoMsg::TextureSet(set), &attachments);
            }
        }
        publish();
    })
}

/// Windows: creates the D3D11 device, returning the adapter SteamVR must render on.
#[cfg(windows)]
pub(crate) fn adapter_luid() -> Option<u64> {
    with(|c| c.gpu.init())
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
            let Some((drm_format, textures)) = c.gpu.create(pid, &desc) else {
                log::error!(
                    "cannot allocate swap textures {}x{} format {} (pid {pid})",
                    desc.nWidth,
                    desc.nHeight,
                    desc.nFormat
                );
                return;
            };
            let textures = textures.into_iter().map(|(handle, mem)| Texture { handle, mem, busy: None }).collect();
            let set = TextureSet { pid, compositor: platform::is_compositor(pid), desc, drm_format, textures };
            log::info!(
                "swap texture set for pid {pid}: {}x{} format {} -> {:x?} ({})",
                desc.nWidth,
                desc.nHeight,
                desc.nFormat,
                set.textures.iter().map(|t| t.handle).collect::<Vec<_>>(),
                set.textures[0].mem.kind(set.streamable())
            );
            for (dst, t) in (*out).rSharedTextureHandles.iter_mut().zip(&set.textures) {
                *dst = t.handle;
            }
            if let Some((ann, attachments)) = set.announcement(c.gpu.render_node()) {
                video::send_set(&ann, &attachments);
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
        // Dropping the textures' memory closes our references (Linux: fds); the server and
        // SteamVR hold their own.
        c.gpu.release(gone.iter().flat_map(|s| s.textures.iter().map(|t| t.handle)));
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

/// Hands the base layer of the frame to the server, with SteamVR's own layer to draw over it
/// (dashboard, laser, overlays) when the base is an application's, marking their textures busy.
fn send_frame(
    c: &mut Compositor,
    base: &[SubmitLayerPerEye_t; 2],
    overlay: Option<&[SubmitLayerPerEye_t; 2]>,
    now_ns: i64,
    sync_texture: SharedTextureHandle_t,
) {
    let streamable = |h: SharedTextureHandle_t| {
        c.sets.iter().any(|s| s.streamable() && s.textures.iter().any(|t| t.handle == h))
    };
    if !base.iter().all(|e| streamable(e.hTexture)) {
        return;
    }
    let frame_id = c.next_frame_id;
    let from_compositor =
        c.sets.iter().any(|s| s.compositor && s.textures.iter().any(|t| t.handle == base[0].hTexture));
    let eye = |e: &SubmitLayerPerEye_t| vipc::EyeLayer {
        texture: e.hTexture,
        bounds: [e.bounds.uMin, e.bounds.vMin, e.bounds.uMax, e.bounds.vMax],
    };
    let msg = vipc::DriverVideoMsg::Frame {
        frame_id,
        eyes: [eye(&base[0]), eye(&base[1])],
        overlay: overlay.map(|o| [eye(&o[0]), eye(&o[1])]),
        // The compositor's own layer reports the pose one frame stale; an application's layer
        // reports the pose it really rendered with (both measured against the mock headset).
        hmd_pose: if from_compositor {
            crate::render_pose::correct(&base[0].mHmdPose, VSYNC_PERIOD_NS.load(Ordering::Acquire))
        } else {
            base[0].mHmdPose
        },
        present_ns: target_vsync_ns(now_ns),
    };
    // Windows: the server's reads wait for the compositor's writes through a fence signalled
    // here. Linux: nothing to do (implicit dmabuf fences).
    if !c.gpu.frame_ready(sync_texture, frame_id) {
        return;
    }
    if !video::send_frame(&msg) {
        return;
    }
    c.next_frame_id += 1;
    c.frames_sent += 1;
    for t in c.sets.iter_mut().flat_map(|s| &mut s.textures) {
        if base.iter().chain(overlay.into_iter().flatten()).any(|e| e.hTexture == t.handle) {
            t.busy = Some(frame_id);
        }
    }
}

unsafe extern "C" fn present(_this: *mut c_void, sync_texture: SharedTextureHandle_t) {
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
                // SteamVR's own layer, over an application's scene. Without an application it
                // is the scene itself, and is not drawn twice.
                let is_compositor = |l: &[SubmitLayerPerEye_t; 2]| {
                    c.sets.iter().any(|s| s.compositor && s.textures.iter().any(|t| t.handle == l[0].hTexture))
                };
                let overlay = if is_compositor(base) {
                    None
                } else {
                    layers
                        .iter()
                        .filter(|l| is_compositor(l) && l.iter().all(|e| streamable(e.hTexture)))
                        .max_by_key(|l| rank(l[0].hTexture).1)
                };
                send_frame(c, base, overlay, now_ns, sync_texture);
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
