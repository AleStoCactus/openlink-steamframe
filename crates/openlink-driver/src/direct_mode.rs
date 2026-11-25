//! `IVRDriverDirectModeComponent`: SteamVR's compositor renders into textures we own and hands
//! them to us every frame, instead of driving a physical display. This is how frames reach the
//! encoder, and on Linux it is also the only way the compositor starts at all for a headset
//! without a real (DRM-leasable) display.
//!
//! Linux: textures are allocated by SteamVR itself through `IVRIPCResourceManagerClient::
//! NewSharedVulkanImage`, so the driver needs no Vulkan device of its own to get the compositor
//! running. To encode, the server imports them via `RefResource` + `ReceiveSharedFd` (next step).
//!
//! Windows: not wired up yet (needs D3D11 shared textures created by the driver).

use crate::host::host;
use crate::ovr::*;
use std::collections::VecDeque;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
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

#[derive(Debug, Clone)]
struct TextureSet {
    pid: u32,
    handles: [SharedTextureHandle_t; 3],
    desc: SwapTextureSetDesc_t,
}

/// One composited frame as SteamVR handed it to us.
#[derive(Debug, Clone)]
pub struct Frame {
    pub layers: Vec<[SubmitLayerPerEye_t; 2]>,
    pub presented_at: Instant,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct CompositorStats {
    pub presents_per_s: f32,
    pub layers_per_frame: f32,
    pub width: u32,
    pub height: u32,
    pub vk_format: u32,
}

#[derive(Default)]
struct Compositor {
    sets: Vec<TextureSet>,
    pending: Vec<[SubmitLayerPerEye_t; 2]>,
    presents: VecDeque<Instant>,
    layer_counts: VecDeque<usize>,
    last_frame: Option<Frame>,
    last_log: Option<Instant>,
}

static COMPOSITOR: Mutex<Option<Compositor>> = Mutex::new(None);

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
        let h = host();
        let mut handles = [0; 3];
        for slot in &mut handles {
            match h.new_shared_vulkan_image(desc.nFormat, desc.nWidth, desc.nHeight) {
                Some(t) => *slot = t,
                None => {
                    log::error!(
                        "NewSharedVulkanImage failed for {}x{} format {} (pid {pid})",
                        desc.nWidth,
                        desc.nHeight,
                        desc.nFormat
                    );
                    handles.iter().filter(|&&t| t != 0).for_each(|&t| h.unref_resource(t));
                    return;
                }
            }
        }
        log::info!(
            "swap texture set for pid {pid}: {}x{} format {} samples {} -> {handles:x?}",
            desc.nWidth,
            desc.nHeight,
            desc.nFormat,
            desc.nSampleCount
        );
        (*out).rSharedTextureHandles = handles;
        with(|c| c.sets.push(TextureSet { pid, handles, desc }));
    })
}

unsafe extern "C" fn destroy_swap_texture_set(_this: *mut c_void, handle: SharedTextureHandle_t) {
    crate::guard((), || {
        let removed: Vec<TextureSet> = with(|c| {
            let (gone, keep) = c.sets.drain(..).partition(|s| s.handles.contains(&handle));
            c.sets = keep;
            gone
        });
        release(&removed);
    })
}

unsafe extern "C" fn destroy_all_swap_texture_sets(_this: *mut c_void, pid: u32) {
    crate::guard((), || {
        let removed: Vec<TextureSet> = with(|c| {
            let (gone, keep) = c.sets.drain(..).partition(|s| s.pid == pid);
            c.sets = keep;
            gone
        });
        release(&removed);
    })
}

fn release(sets: &[TextureSet]) {
    let h = host();
    for s in sets {
        log::info!("releasing swap texture set {:x?} (pid {})", s.handles, s.pid);
        s.handles.iter().for_each(|&t| h.unref_resource(t));
    }
}

unsafe extern "C" fn get_next_swap_texture_set_index(
    _this: *mut c_void,
    _handles: *const SharedTextureHandle_t,
    indices: *mut [u32; 2],
) {
    let idx = &mut *indices;
    idx[0] = (idx[0] + 1) % 3;
    idx[1] = (idx[1] + 1) % 3;
}

unsafe extern "C" fn submit_layer(_this: *mut c_void, per_eye: *const [SubmitLayerPerEye_t; 2]) {
    let layer = *per_eye;
    with(|c| c.pending.push(layer));
}

unsafe extern "C" fn present(_this: *mut c_void, _sync_texture: SharedTextureHandle_t) {
    crate::guard((), || {
        let now = Instant::now();
        let should_log = with(|c| {
            let layers = std::mem::take(&mut c.pending);
            c.layer_counts.push_back(layers.len());
            c.presents.push_back(now);
            while c.presents.len() > 512 {
                c.presents.pop_front();
            }
            while c.layer_counts.len() > 128 {
                c.layer_counts.pop_front();
            }
            c.last_frame = Some(Frame { layers, presented_at: now });
            let due = c.last_log.is_none_or(|t| now - t > Duration::from_secs(5));
            if due {
                c.last_log = Some(now);
            }
            due
        });
        if should_log {
            let s = stats();
            log::info!(
                "compositor: {:.0} presents/s, {:.1} layers/frame, {}x{} format {}",
                s.presents_per_s,
                s.layers_per_frame,
                s.width,
                s.height,
                s.vk_format
            );
        }
    })
}

unsafe extern "C" fn post_present(_this: *mut c_void, _throttling: *const Throttling_t) {}

unsafe extern "C" fn get_frame_timing(_this: *mut c_void, timing: *mut DriverDirectMode_FrameTiming) {
    // Per the header: clear the reprojection flags or they are read as throttling requests.
    (*timing).m_nReprojectionFlags = 0;
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
                        let now = Instant::now();
                        if next > now {
                            std::thread::sleep(next - now);
                        } else {
                            next = now;
                        }
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
