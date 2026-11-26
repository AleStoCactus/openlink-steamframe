//! Windows: the D3D11 half of importing the driver's textures.
//!
//! The driver's swap textures have legacy shared handles, which Vulkan implementations do not
//! reliably import. So the server opens them with D3D11 and copies each frame's textures into
//! textures of its own that carry NT handles, which Vulkan does import
//! (`VK_EXTERNAL_MEMORY_HANDLE_TYPE_D3D11_TEXTURE_BIT`). The copy is a few hundred microseconds
//! of GPU bandwidth per frame.
//!
//! Ordering is GPU-side throughout: the copy waits for the driver's frame fence (signalled once
//! the compositor is done with the frame), and Vulkan waits for our fence, signalled after the
//! copy, as a timeline semaphore. Where that import is unsupported the CPU waits for the copy
//! instead.

use super::vk::Result;
use std::cell::{Cell, RefCell};
use std::ffi::c_void;
use std::os::windows::io::{FromRawHandle, OwnedHandle};
use windows::core::{Interface, PCWSTR};
use windows::Win32::Foundation::{CloseHandle, DuplicateHandle, DUPLICATE_SAME_ACCESS, HANDLE, HMODULE, LUID};
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_1};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11Device5, ID3D11DeviceContext4, ID3D11Fence, ID3D11Texture2D,
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_FENCE_FLAG_SHARED,
    D3D11_RESOURCE_MISC_SHARED, D3D11_RESOURCE_MISC_SHARED_NTHANDLE, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_DEFAULT,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_B8G8R8X8_UNORM, DXGI_FORMAT_R10G10B10A2_UNORM,
    DXGI_FORMAT_R8G8B8A8_UNORM, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIAdapter, IDXGIFactory4, IDXGIResource1};
use windows::Win32::System::Threading::{
    CreateEventW, GetCurrentProcess, OpenProcess, WaitForSingleObject, PROCESS_DUP_HANDLE,
};

/// `GENERIC_ALL`, the only access a shared fence handle may be created with.
const GENERIC_ALL: u32 = 0x1000_0000;
/// `DXGI_SHARED_RESOURCE_READ | DXGI_SHARED_RESOURCE_WRITE`.
const SHARED_READ_WRITE: u32 = 0x8000_0000 | 1;

fn err(what: &str) -> impl Fn(windows::core::Error) -> String + '_ {
    move |e| format!("{what}: {e}")
}

/// One of the driver's textures and our importable copy of it.
pub struct Texture {
    pub src: ID3D11Texture2D,
    pub copy: ID3D11Texture2D,
    /// NT handle to `copy`, for Vulkan to import (it does not take ownership).
    pub copy_handle: OwnedHandle,
}

pub struct Bridge {
    device: ID3D11Device,
    context: ID3D11DeviceContext4,
    /// Ours: signalled after each frame's copies.
    fence: ID3D11Fence,
    value: Cell<u64>,
    /// The driver's frame fence.
    driver_fence: RefCell<Option<ID3D11Fence>>,
}

impl Bridge {
    /// A D3D11 device on the adapter with this LUID.
    pub fn new(luid: u64) -> Result<Self> {
        // SAFETY: plain DXGI/D3D11 object creation with valid arguments.
        unsafe {
            let factory: IDXGIFactory4 = CreateDXGIFactory1().map_err(err("CreateDXGIFactory1"))?;
            let adapter: IDXGIAdapter = factory
                .EnumAdapterByLuid(LUID { LowPart: luid as u32, HighPart: (luid >> 32) as i32 })
                .map_err(err("EnumAdapterByLuid"))?;
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
            )
            .map_err(err("D3D11CreateDevice"))?;
            let device: ID3D11Device = device.ok_or("D3D11CreateDevice returned no device")?;
            let context: ID3D11DeviceContext4 = context
                .ok_or("D3D11CreateDevice returned no context")?
                .cast()
                .map_err(err("ID3D11DeviceContext4"))?;
            let mut fence: Option<ID3D11Fence> = None;
            device
                .cast::<ID3D11Device5>()
                .and_then(|d| d.CreateFence(0, D3D11_FENCE_FLAG_SHARED, &mut fence))
                .map_err(err("CreateFence"))?;
            let fence = fence.ok_or("CreateFence returned no fence")?;
            Ok(Self { device, context, fence, value: Cell::new(0), driver_fence: RefCell::new(None) })
        }
    }

    /// An NT handle to our copy fence, for Vulkan to import as a timeline semaphore.
    pub fn fence_handle(&self) -> Result<OwnedHandle> {
        // SAFETY: a fence created with D3D11_FENCE_FLAG_SHARED.
        let h = unsafe { self.fence.CreateSharedHandle(None, GENERIC_ALL, PCWSTR::null()) }
            .map_err(err("ID3D11Fence::CreateSharedHandle"))?;
        // SAFETY: a fresh handle we own.
        Ok(unsafe { OwnedHandle::from_raw_handle(h.0) })
    }

    /// Opens the driver's frame fence: `handle` is valid in process `pid` (vrserver).
    pub fn set_driver_fence(&self, pid: u32, handle: u64) -> Result<()> {
        // SAFETY: duplicating a handle out of a process we may open; every handle is closed.
        unsafe {
            let process = OpenProcess(PROCESS_DUP_HANDLE, false, pid)
                .map_err(err("cannot open vrserver to share its frame fence (is SteamVR elevated?)"))?;
            let mut dup = HANDLE::default();
            let r = DuplicateHandle(
                process,
                HANDLE(handle as *mut c_void),
                GetCurrentProcess(),
                &mut dup,
                0,
                false,
                DUPLICATE_SAME_ACCESS,
            );
            let _ = CloseHandle(process);
            r.map_err(err("DuplicateHandle (frame fence)"))?;
            let mut fence: Option<ID3D11Fence> = None;
            let opened = self.device.cast::<ID3D11Device5>().and_then(|d| d.OpenSharedFence(dup, &mut fence));
            let _ = CloseHandle(dup);
            opened.map_err(err("OpenSharedFence"))?;
            *self.driver_fence.borrow_mut() = fence;
        }
        Ok(())
    }

    pub fn has_driver_fence(&self) -> bool {
        self.driver_fence.borrow().is_some()
    }

    /// Opens the driver's texture `handle` and creates our copy of it.
    pub fn open(&self, handle: u64, drm_format: u32) -> Result<Texture> {
        let format = copy_format(drm_format).ok_or_else(|| format!("unsupported DRM format {drm_format:#x}"))?;
        // SAFETY: a legacy shared handle (global), opened on the adapter the driver created it on.
        unsafe {
            let mut src: Option<ID3D11Texture2D> = None;
            self.device
                .OpenSharedResource(HANDLE(handle as *mut c_void), &mut src)
                .map_err(err("OpenSharedResource"))?;
            let src = src.ok_or("OpenSharedResource returned no texture")?;
            let mut desc = D3D11_TEXTURE2D_DESC::default();
            src.GetDesc(&mut desc);
            let copy_desc = D3D11_TEXTURE2D_DESC {
                Width: desc.Width,
                Height: desc.Height,
                MipLevels: 1,
                ArraySize: 1,
                Format: format,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: (D3D11_BIND_SHADER_RESOURCE.0 | D3D11_BIND_RENDER_TARGET.0) as u32,
                CPUAccessFlags: 0,
                MiscFlags: (D3D11_RESOURCE_MISC_SHARED_NTHANDLE.0 | D3D11_RESOURCE_MISC_SHARED.0) as u32,
            };
            let mut copy: Option<ID3D11Texture2D> = None;
            self.device.CreateTexture2D(&copy_desc, None, Some(&mut copy)).map_err(err("CreateTexture2D (copy)"))?;
            let copy = copy.ok_or("CreateTexture2D returned no texture")?;
            let h = copy
                .cast::<IDXGIResource1>()
                .and_then(|r| r.CreateSharedHandle(None, SHARED_READ_WRITE, PCWSTR::null()))
                .map_err(err("IDXGIResource1::CreateSharedHandle"))?;
            Ok(Texture { src, copy, copy_handle: OwnedHandle::from_raw_handle(h.0) })
        }
    }

    /// Copies each `(copy, source)` pair once the driver's fence reaches `frame_id`. Returns
    /// the value our fence reaches when the copies are done.
    pub fn copy(&self, frame_id: u64, pairs: &[(&ID3D11Texture2D, &ID3D11Texture2D)]) -> Result<u64> {
        let value = self.value.get() + 1;
        // SAFETY: valid context, textures and fences; the context is used by this thread only.
        unsafe {
            if let Some(f) = self.driver_fence.borrow().as_ref() {
                self.context.Wait(f, frame_id).map_err(err("ID3D11DeviceContext4::Wait"))?;
            }
            for (dst, src) in pairs {
                self.context.CopyResource(*dst, *src);
            }
            self.context.Signal(&self.fence, value).map_err(err("ID3D11DeviceContext4::Signal"))?;
            self.context.Flush();
        }
        self.value.set(value);
        Ok(value)
    }

    /// Waits on the CPU until our fence reaches `value` (when Vulkan cannot wait for it).
    pub fn wait_cpu(&self, value: u64) -> Result<()> {
        // SAFETY: an event we create, wait on and close.
        unsafe {
            let event = CreateEventW(None, false, false, PCWSTR::null()).map_err(err("CreateEventW"))?;
            let set = self.fence.SetEventOnCompletion(value, event);
            if set.is_ok() {
                WaitForSingleObject(event, 1000);
            }
            let _ = CloseHandle(event);
            set.map_err(err("ID3D11Fence::SetEventOnCompletion"))
        }
    }
}

/// The typed (UNORM) DXGI format of our copy for a DRM format the driver translated from the
/// swap texture's DXGI format; the same family, so `CopyResource` may copy between them.
fn copy_format(drm_format: u32) -> Option<DXGI_FORMAT> {
    Some(match &drm_format.to_le_bytes() {
        b"AB24" => DXGI_FORMAT_R8G8B8A8_UNORM,
        b"AR24" => DXGI_FORMAT_B8G8R8A8_UNORM,
        b"XR24" => DXGI_FORMAT_B8G8R8X8_UNORM,
        b"AB30" => DXGI_FORMAT_R10G10B10A2_UNORM,
        _ => return None,
    })
}
