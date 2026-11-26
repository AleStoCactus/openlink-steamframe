//! Windows textures: D3D11 textures we create with a legacy shared handle, which is what
//! SteamVR's `SharedTextureHandle_t` is on Windows. The compositor and applications open them
//! by that handle (whatever their graphics API: SteamVR translates), and so does the server,
//! because a legacy shared handle is global rather than per process.
//!
//! D3D11 has no implicit fences to say when the compositor is done writing. `Present` gives us
//! a sync texture whose keyed mutex the compositor holds until its work on the frame is
//! submitted; acquiring it on our device orders our commands after that work. We then signal a
//! shared `ID3D11Fence` to the frame's id, and the server's GPU waits for that value before
//! reading the frame (`DriverVideoMsg::SyncFence`).
//!
//! The device is created on the adapter we tell SteamVR to render on
//! (`Prop_GraphicsAdapterLuid_Uint64`), so the textures, the compositor and the server's encoder
//! are all on one GPU.

use super::SET_SIZE;
use crate::ovr::*;
use openlink_protocol::video_ipc::{self as vipc, DmabufTexture, DriverVideoMsg, RawAttachment};
use std::collections::HashMap;
use std::ffi::c_void;
use windows::core::{Interface, PCWSTR, PWSTR};
use windows::Win32::Foundation::{CloseHandle, E_FAIL, HANDLE, HMODULE, S_OK};
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_1};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11Device5, ID3D11DeviceContext4, ID3D11Fence, ID3D11Texture2D,
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_FENCE_FLAG_SHARED,
    D3D11_RESOURCE_MISC_SHARED, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIAdapter1, IDXGIFactory1, IDXGIKeyedMutex, IDXGIResource, DXGI_ADAPTER_DESC1,
    DXGI_ADAPTER_FLAG_SOFTWARE,
};
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};

/// `GENERIC_ALL`, the only access a shared fence handle may be created with.
const GENERIC_ALL: u32 = 0x1000_0000;
/// How long `Present` waits for the compositor to release the sync texture (as other
/// streaming drivers do); a frame that misses it is not streamed.
const SYNC_TIMEOUT_MS: u32 = 10;

/// One texture: kept alive here; SteamVR and the server hold their own references.
pub struct TextureMem {
    _texture: ID3D11Texture2D,
    streamable: bool,
}

impl TextureMem {
    pub fn streamable(&self) -> bool {
        self.streamable
    }

    pub fn modifier(&self) -> u64 {
        vipc::DRM_FORMAT_MOD_INVALID
    }

    /// The texture for the server's announcement: the handle says it all.
    pub fn describe(&self, handle: SharedTextureHandle_t, _attachments: &mut Vec<RawAttachment>) -> DmabufTexture {
        DmabufTexture { handle, planes: Vec::new() }
    }

    /// For the log.
    pub fn kind(&self, _set_streamable: bool) -> String {
        if self.streamable {
            "D3D11 shared texture".into()
        } else {
            "D3D11 shared texture, not streamable (multisampled or unsupported format)".into()
        }
    }
}

pub struct D3d {
    device: ID3D11Device,
    context: ID3D11DeviceContext4,
    luid: u64,
    fence: ID3D11Fence,
    /// NT handle to `fence`, for the server to duplicate.
    fence_handle: HANDLE,
    /// Keyed mutexes of the sync textures `Present` has handed us, by handle.
    sync: HashMap<SharedTextureHandle_t, IDXGIKeyedMutex>,
    sync_failures: u64,
}

// SAFETY: the device is free-threaded; the immediate context is not, but every use goes
// through the compositor mutex.
unsafe impl Send for D3d {}

impl Drop for D3d {
    fn drop(&mut self) {
        // SAFETY: our own handle, closed once.
        unsafe {
            let _ = CloseHandle(self.fence_handle);
        }
    }
}

#[derive(Default)]
pub enum Gpu {
    #[default]
    Untried,
    Failed,
    Ready(D3d),
}

impl Gpu {
    /// Creates the D3D11 device on first use. Returns the adapter LUID, for SteamVR.
    pub fn init(&mut self) -> Option<u64> {
        if matches!(self, Gpu::Untried) {
            *self = match open() {
                Ok(d) => {
                    // Reaches the server now if it is already connected; otherwise it goes out
                    // with the announcement when it connects.
                    crate::video::send_msg(&DriverVideoMsg::SyncFence { handle: d.fence_handle.0 as u64 });
                    Gpu::Ready(d)
                }
                Err(e) => {
                    log::error!("cannot create a D3D11 device: {e}; frames cannot be streamed");
                    Gpu::Failed
                }
            };
        }
        match self {
            Gpu::Ready(d) => Some(d.luid),
            _ => None,
        }
    }

    /// The adapter LUID the textures live on (0 if unknown).
    pub fn render_node(&self) -> u64 {
        match self {
            Gpu::Ready(d) => d.luid,
            _ => 0,
        }
    }

    /// Messages the server needs before any texture set: the fence.
    pub fn connect_messages(&self) -> Vec<DriverVideoMsg> {
        match self {
            Gpu::Ready(d) => vec![DriverVideoMsg::SyncFence { handle: d.fence_handle.0 as u64 }],
            _ => Vec::new(),
        }
    }

    /// Allocates a swap set: `(DRM format, textures)`.
    pub fn create(
        &mut self,
        _pid: u32,
        desc: &SwapTextureSetDesc_t,
    ) -> Option<(u32, Vec<(SharedTextureHandle_t, TextureMem)>)> {
        self.init()?;
        let Gpu::Ready(d) = self else { return None };
        let drm_format = drm_format_for(desc.nFormat);
        let streamable = desc.nSampleCount <= 1 && drm_format.is_some();
        if desc.nSampleCount > 1 {
            log::warn!("multisampled swap textures ({}x) cannot be streamed", desc.nSampleCount);
        } else if drm_format.is_none() {
            log::warn!("DXGI format {} is not supported; this set cannot be streamed", desc.nFormat);
        }
        let td = D3D11_TEXTURE2D_DESC {
            Width: desc.nWidth,
            Height: desc.nHeight,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT(desc.nFormat as i32),
            SampleDesc: DXGI_SAMPLE_DESC { Count: desc.nSampleCount.max(1), Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: (D3D11_BIND_SHADER_RESOURCE.0 | D3D11_BIND_RENDER_TARGET.0) as u32,
            CPUAccessFlags: 0,
            MiscFlags: D3D11_RESOURCE_MISC_SHARED.0 as u32,
        };
        let mut textures = Vec::with_capacity(SET_SIZE);
        for _ in 0..SET_SIZE {
            let mut tex = None;
            // SAFETY: valid description and out pointer.
            let created = unsafe { d.device.CreateTexture2D(&td, None, Some(&mut tex)) };
            let tex = match (created, tex) {
                (Ok(()), Some(t)) => t,
                (r, _) => {
                    log::error!("CreateTexture2D {}x{} format {}: {r:?}", desc.nWidth, desc.nHeight, desc.nFormat);
                    return None;
                }
            };
            // SAFETY: a texture created with D3D11_RESOURCE_MISC_SHARED has a legacy handle.
            let handle = match tex.cast::<IDXGIResource>().and_then(|r| unsafe { r.GetSharedHandle() }) {
                Ok(h) => h.0 as u64,
                Err(e) => {
                    log::error!("no shared handle for a swap texture: {e}");
                    return None;
                }
            };
            textures.push((handle, TextureMem { _texture: tex, streamable }));
        }
        Some((drm_format.unwrap_or(0), textures))
    }

    /// Nothing to do: the textures go when their `TextureMem` is dropped.
    pub fn release(&self, handles: impl IntoIterator<Item = SharedTextureHandle_t>) {
        let _ = handles;
    }

    /// The frame is about to go to the server: once the compositor's work on it is done, the
    /// fence reaches `frame_id`. `false`: the compositor did not release the sync texture in
    /// time, and the frame is not streamed.
    pub fn frame_ready(&mut self, sync_texture: SharedTextureHandle_t, frame_id: u64) -> bool {
        let Gpu::Ready(d) = self else { return false };
        if !crate::video::connected() {
            // Nobody to read it: skip the GPU work (sending fails anyway).
            return true;
        }
        let mutex = if sync_texture == 0 {
            None
        } else {
            match d.sync.get(&sync_texture) {
                Some(m) => Some(m.clone()),
                None => {
                    // SAFETY: SteamVR's legacy shared handle, opened on the same adapter.
                    let mut tex: Option<ID3D11Texture2D> = None;
                    let opened = unsafe { d.device.OpenSharedResource(HANDLE(sync_texture as *mut c_void), &mut tex) }
                        .and_then(|()| tex.ok_or_else(|| windows::core::Error::new(E_FAIL, "no texture")))
                        .and_then(|t| t.cast::<IDXGIKeyedMutex>());
                    match opened {
                        Ok(m) => {
                            if d.sync.len() > 16 {
                                d.sync.clear();
                            }
                            d.sync.insert(sync_texture, m.clone());
                            Some(m)
                        }
                        Err(e) => {
                            log::warn!("cannot open the sync texture {sync_texture:#x}: {e}");
                            None
                        }
                    }
                }
            }
        };
        if let Some(m) = &mutex {
            // Called through the vtable: the wrapper folds WAIT_TIMEOUT, a success code, into Ok.
            // SAFETY: a live keyed mutex.
            let hr = unsafe { (Interface::vtable(m).AcquireSync)(Interface::as_raw(m), 0, SYNC_TIMEOUT_MS) };
            if hr != S_OK {
                d.sync_failures += 1;
                if d.sync_failures.is_power_of_two() {
                    log::warn!("compositor sync texture not released in time ({hr:?}, {} times)", d.sync_failures);
                }
                return false;
            }
        }
        // SAFETY: valid context and fence; the context is only used under the compositor lock.
        unsafe {
            let signaled = d.context.Signal(&d.fence, frame_id);
            d.context.Flush();
            if let Some(m) = &mutex {
                let _ = m.ReleaseSync(0);
            }
            if let Err(e) = signaled {
                log::warn!("cannot signal the frame fence: {e}");
                return false;
            }
        }
        true
    }
}

fn open() -> windows::core::Result<D3d> {
    // SAFETY: plain DXGI/D3D11 object creation with valid arguments throughout.
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1()?;
        // OPENLINK_ADAPTER: a DXGI adapter index; otherwise the hardware adapter with the most
        // dedicated memory (the discrete GPU, not an integrated one or a virtual display).
        let wanted = std::env::var("OPENLINK_ADAPTER").ok().and_then(|v| v.parse::<u32>().ok());
        let mut best: Option<(IDXGIAdapter1, DXGI_ADAPTER_DESC1)> = None;
        let mut i = 0;
        while let Ok(adapter) = factory.EnumAdapters1(i) {
            let desc = adapter.GetDesc1()?;
            let software = desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0;
            if wanted == Some(i) {
                best = Some((adapter, desc));
                break;
            }
            if wanted.is_none()
                && !software
                && best.as_ref().is_none_or(|(_, b)| desc.DedicatedVideoMemory > b.DedicatedVideoMemory)
            {
                best = Some((adapter, desc));
            }
            i += 1;
        }
        let (adapter, desc) = best.ok_or_else(|| windows::core::Error::new(E_FAIL, "no hardware adapter"))?;
        let mut device = None;
        let mut context = None;
        D3D11CreateDevice(
            &adapter,
            D3D_DRIVER_TYPE_UNKNOWN,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            Some(&[D3D_FEATURE_LEVEL_11_1]),
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )?;
        let device: ID3D11Device = device.ok_or_else(|| windows::core::Error::new(E_FAIL, "no device"))?;
        let context: ID3D11DeviceContext4 =
            context.ok_or_else(|| windows::core::Error::new(E_FAIL, "no context"))?.cast()?;
        let mut fence: Option<ID3D11Fence> = None;
        device.cast::<ID3D11Device5>()?.CreateFence(0, D3D11_FENCE_FLAG_SHARED, &mut fence)?;
        let fence = fence.ok_or_else(|| windows::core::Error::new(E_FAIL, "no fence"))?;
        let fence_handle = fence.CreateSharedHandle(None, GENERIC_ALL, PCWSTR::null())?;
        let luid = ((desc.AdapterLuid.HighPart as u32 as u64) << 32) | desc.AdapterLuid.LowPart as u64;
        let name_len = desc.Description.iter().position(|&c| c == 0).unwrap_or(desc.Description.len());
        log::info!(
            "swap textures on {} (adapter LUID {luid:#x})",
            String::from_utf16_lossy(&desc.Description[..name_len])
        );
        Ok(D3d { device, context, luid, fence, fence_handle, sync: HashMap::new(), sync_failures: 0 })
    }
}

const fn fourcc(s: &[u8; 4]) -> u32 {
    u32::from_le_bytes(*s)
}

/// The DRM format with the same memory layout as a compositor `DXGI_FORMAT`.
fn drm_format_for(dxgi_format: u32) -> Option<u32> {
    Some(match dxgi_format {
        27..=29 => fourcc(b"AB24"),       // R8G8B8A8_TYPELESS / _UNORM / _UNORM_SRGB
        87 | 90 | 91 => fourcc(b"AR24"),  // B8G8R8A8_UNORM / _TYPELESS / _UNORM_SRGB
        88 | 92 | 93 => fourcc(b"XR24"),  // B8G8R8X8_UNORM / _TYPELESS / _UNORM_SRGB
        23 | 24 => fourcc(b"AB30"),       // R10G10B10A2_TYPELESS / _UNORM
        _ => return None,
    })
}

/// Whether `pid` is SteamVR's compositor (`vrcompositor.exe`).
pub fn is_compositor(pid: u32) -> bool {
    // SAFETY: plain process queries on a handle we close.
    unsafe {
        let Ok(process) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else { return false };
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(process, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len).is_ok();
        let _ = CloseHandle(process);
        ok && String::from_utf16_lossy(&buf[..len as usize])
            .rsplit(['\\', '/'])
            .next()
            .is_some_and(|n| n.eq_ignore_ascii_case("vrcompositor.exe"))
    }
}
