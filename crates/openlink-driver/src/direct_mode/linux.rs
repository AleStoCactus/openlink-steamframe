//! Linux textures: the compositor's are GBM dmabufs we allocate with a DRM modifier it
//! accepts and import into SteamVR (`IVRIPCResourceManagerClient::ImportDmabuf`); applications'
//! are allocated by SteamVR (`NewSharedVulkanImage`) and shared with us as dmabufs. The server
//! imports both; it waits on the dmabufs' implicit fences, so a frame needs no extra sync here.

use super::SET_SIZE;
use crate::gbm;
use crate::host::host;
use crate::ovr::*;
use openlink_protocol::video_ipc::{self as vipc, DmabufPlane, DmabufTexture, DriverVideoMsg, RawAttachment};
use std::ffi::{CStr, CString};
use std::os::fd::{AsRawFd, OwnedFd};

/// One texture's memory.
pub struct TextureMem {
    /// `Some` when we allocated it with GBM (the compositor's own textures).
    bo: Option<gbm::Bo>,
    /// SteamVR's dmabuf for an image it allocated (applications' textures); its layout is in
    /// the buffer's kernel metadata rather than a modifier.
    shared: Option<OwnedFd>,
}

impl TextureMem {
    pub fn streamable(&self) -> bool {
        self.bo.is_some() || self.shared.is_some()
    }

    pub fn modifier(&self) -> u64 {
        self.bo.as_ref().map_or(vipc::DRM_FORMAT_MOD_INVALID, |bo| bo.modifier)
    }

    /// The texture for the server's announcement; its fds go to `attachments`.
    pub fn describe(&self, handle: SharedTextureHandle_t, attachments: &mut Vec<RawAttachment>) -> DmabufTexture {
        match (&self.bo, &self.shared) {
            (Some(bo), _) => {
                attachments.extend(bo.planes.iter().map(|p| p.fd.as_raw_fd()));
                DmabufTexture {
                    handle,
                    planes: bo.planes.iter().map(|p| DmabufPlane { offset: p.offset, stride: p.stride }).collect(),
                }
            }
            (None, Some(fd)) => {
                attachments.push(fd.as_raw_fd());
                DmabufTexture { handle, planes: vec![DmabufPlane { offset: 0, stride: 0 }] }
            }
            (None, None) => unreachable!("streamable() checked"),
        }
    }

    /// For the log.
    pub fn kind(&self, set_streamable: bool) -> String {
        match self.bo.as_ref() {
            Some(bo) => format!("dmabuf, modifier {:#x}, {} plane(s)", bo.modifier, bo.planes.len()),
            None if set_streamable => "SteamVR-allocated, shared as dmabuf".into(),
            None => "SteamVR-allocated, not streamable".into(),
        }
    }
}

#[derive(Default)]
pub enum Gpu {
    #[default]
    Untried,
    Failed,
    Ready(gbm::Device),
}

impl Gpu {
    /// `st_rdev` of the render node the textures live on (0 if unknown).
    pub fn render_node(&self) -> u64 {
        match self {
            Gpu::Ready(d) => d.rdev(),
            _ => 0,
        }
    }

    /// Messages the server needs before any texture set (none on Linux).
    pub fn connect_messages(&self) -> Vec<DriverVideoMsg> {
        Vec::new()
    }

    /// Allocates a swap set: `(DRM format, textures)`.
    pub fn create(
        &mut self,
        pid: u32,
        desc: &SwapTextureSetDesc_t,
    ) -> Option<(u32, Vec<(SharedTextureHandle_t, TextureMem)>)> {
        // Only the compositor can use our GBM dmabufs: applications (e.g. games under
        // Proton) reject them, compressed or not, and ask again in an endless loop.
        let dmabuf = if is_compositor(pid) { self.create_dmabuf_textures(desc, true) } else { None };
        match dmabuf {
            Some(t) => Some(t),
            None => Some((drm_format_for(desc.nFormat).unwrap_or(0), create_steamvr_textures(desc)?)),
        }
    }

    pub fn release(&self, handles: impl IntoIterator<Item = SharedTextureHandle_t>) {
        let h = host();
        handles.into_iter().for_each(|t| h.unref_resource(t));
    }

    /// The frame is about to go to the server. Nothing to do: the server waits on the dmabufs'
    /// implicit fences.
    pub fn frame_ready(&mut self, _sync_texture: SharedTextureHandle_t, _frame_id: u64) -> bool {
        true
    }

    /// Allocates `desc` as dmabufs and imports them into SteamVR. `None` (with a log line) when
    /// this path is unavailable; the caller then falls back to SteamVR's own allocation.
    /// `allow_compression`: AMD DCC-compressed modifiers are fine for the compositor, but
    /// applications (games under Proton) reject textures that use them.
    fn create_dmabuf_textures(
        &mut self,
        desc: &SwapTextureSetDesc_t,
        allow_compression: bool,
    ) -> Option<(u32, Vec<(SharedTextureHandle_t, TextureMem)>)> {
        let h = host();
        let drm_format = drm_format_for(desc.nFormat);
        // The compositor's modifiers also tell which GPU it renders on.
        let compositor_modifiers = drm_format.map(|f| h.dmabuf_modifiers(f)).unwrap_or_default();
        if matches!(self, Gpu::Untried) {
            let node = render_node(&compositor_modifiers);
            *self = match gbm::Device::open(&node) {
                Ok(d) => {
                    log::info!("allocating compositor textures with GBM on {}", node.to_string_lossy());
                    Gpu::Ready(d)
                }
                Err(e) => {
                    log::warn!("GBM unavailable ({e}); frames cannot be streamed");
                    Gpu::Failed
                }
            };
        }
        let Gpu::Ready(dev) = self else { return None };
        if desc.nSampleCount > 1 {
            log::warn!("multisampled swap textures ({}x) cannot be streamed", desc.nSampleCount);
            return None;
        }
        let Some(drm_format) = drm_format else {
            log::warn!("no DRM format for VkFormat {}; this set cannot be streamed", desc.nFormat);
            return None;
        };
        let mut modifiers = compositor_modifiers;
        // On GFX12 (RDNA4) the server reads what the compositor cleared as the texture's old
        // content: SteamVR's layer over a game came out opaque black in two frames of three.
        modifiers.retain(|&m| !amd_dcc(m) || (allow_compression && !amd_gfx12(m)));
        if modifiers.is_empty() {
            log::warn!("compositor reports no dmabuf modifiers for format {drm_format:#x}");
            return None;
        }

        let mut textures: Vec<(SharedTextureHandle_t, TextureMem)> = Vec::with_capacity(SET_SIZE);
        let release = |t: &[(SharedTextureHandle_t, TextureMem)]| t.iter().for_each(|(h2, _)| h.unref_resource(*h2));
        for _ in 0..SET_SIZE {
            let Some(bo) = dev.create(desc.nWidth, desc.nHeight, drm_format, &modifiers) else {
                log::warn!("GBM allocation of {}x{} failed", desc.nWidth, desc.nHeight);
                release(&textures);
                return None;
            };
            if bo.planes.len() > MaxDmabufPlaneCount {
                log::warn!("{} planes is more than SteamVR accepts", bo.planes.len());
                release(&textures);
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
                release(&textures);
                return None;
            };
            textures.push((handle, TextureMem { bo: Some(bo), shared: None }));
        }
        Some((drm_format, textures))
    }
}

/// SteamVR allocates the images itself. The compositor runs, but the server cannot read them.
fn create_steamvr_textures(desc: &SwapTextureSetDesc_t) -> Option<Vec<(SharedTextureHandle_t, TextureMem)>> {
    let h = host();
    let mut textures: Vec<(SharedTextureHandle_t, TextureMem)> = Vec::with_capacity(SET_SIZE);
    for _ in 0..SET_SIZE {
        match h.new_shared_vulkan_image(desc.nFormat, desc.nWidth, desc.nHeight) {
            Some(handle) => {
                // Its dmabuf, so the server can read it (an application's layer).
                let shared = h.shared_fd(handle);
                if shared.is_none() {
                    log::warn!("no fd for SteamVR image {handle:#x}; this set cannot be streamed");
                }
                textures.push((handle, TextureMem { bo: None, shared }))
            }
            None => {
                textures.iter().for_each(|(t, _)| h.unref_resource(*t));
                return None;
            }
        }
    }
    Some(textures)
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

/// The render node of the GPU SteamVR's compositor runs on: `OPENLINK_RENDER_NODE` if set,
/// else the first node made by the vendor of the compositor's modifiers, else renderD128.
/// With an integrated GPU beside a discrete one, renderD128 is often the integrated one,
/// which can neither share buffers with the compositor nor encode.
fn render_node(compositor_modifiers: &[u64]) -> CString {
    const FALLBACK: &CStr = c"/dev/dri/renderD128";
    if let Ok(path) = std::env::var("OPENLINK_RENDER_NODE") {
        return CString::new(path).unwrap_or_else(|_| FALLBACK.into());
    }
    let Some(vendor) = pci_vendor_of(compositor_modifiers) else { return FALLBACK.into() };
    let mut nodes: Vec<u32> = std::fs::read_dir("/sys/class/drm")
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok()?.file_name().to_str()?.strip_prefix("renderD")?.parse().ok())
        .collect();
    nodes.sort_unstable();
    nodes
        .into_iter()
        .find(|n| {
            std::fs::read_to_string(format!("/sys/class/drm/renderD{n}/device/vendor"))
                .is_ok_and(|v| u32::from_str_radix(v.trim().trim_start_matches("0x"), 16) == Ok(vendor))
        })
        .and_then(|n| CString::new(format!("/dev/dri/renderD{n}")).ok())
        .unwrap_or_else(|| FALLBACK.into())
}

/// PCI vendor of the GPU that makes these modifiers, from the vendor byte of the first
/// vendor-specific one (`fourcc_mod_code`). `None` if all are generic, such as LINEAR.
fn pci_vendor_of(modifiers: &[u64]) -> Option<u32> {
    modifiers.iter().find_map(|&m| match m >> 56 {
        0x01 => Some(0x8086), // DRM_FORMAT_MOD_VENDOR_INTEL
        0x02 => Some(0x1002), // DRM_FORMAT_MOD_VENDOR_AMD
        0x03 => Some(0x10de), // DRM_FORMAT_MOD_VENDOR_NVIDIA
        _ => None,
    })
}

/// Whether an AMD format modifier uses DCC compression (`AMD_FMT_MOD_DCC`, bit 13 of an
/// `AMD_FMT_MOD` modifier, vendor 0x02 in the top byte).
fn amd_dcc(modifier: u64) -> bool {
    modifier >> 56 == 0x02 && modifier & (1 << 13) != 0
}

/// Whether an AMD format modifier is for GFX12 (RDNA4) or newer: `AMD_FMT_MOD_TILE_VERSION`
/// (the low byte) 5 or more.
fn amd_gfx12(modifier: u64) -> bool {
    modifier >> 56 == 0x02 && modifier & 0xff >= 5
}

/// Whether `pid` is SteamVR's compositor (`vrcompositor`).
pub fn is_compositor(pid: u32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/comm")).is_ok_and(|c| c.trim() == "vrcompositor")
}

#[cfg(test)]
mod tests {
    use super::{amd_dcc, amd_gfx12, pci_vendor_of};

    #[test]
    fn gfx12_dcc_is_recognised() {
        // What RADV offered for the compositor on an RX 9070 XT: GFX12, 64K_2D, DCC.
        const GFX12_DCC: u64 = 0x0200_0000_0008_2305;
        assert!(amd_dcc(GFX12_DCC) && amd_gfx12(GFX12_DCC));
        // GFX10.3 (RDNA2) with DCC keeps it.
        const GFX10_RBPLUS_DCC: u64 = 0x0200_0000_0000_2003 | (0x18 << 8);
        assert!(amd_dcc(GFX10_RBPLUS_DCC) && !amd_gfx12(GFX10_RBPLUS_DCC));
        assert!(!amd_gfx12(0));
    }

    #[test]
    fn modifier_vendor_names_the_gpu() {
        const LINEAR: u64 = 0;
        const I915_X_TILED: u64 = 0x0100_0000_0000_0001;
        const AMD_GFX12: u64 = 0x0200_0000_1866_bf04;
        // LINEAR says nothing; the first vendor-specific modifier decides.
        assert_eq!(pci_vendor_of(&[LINEAR, AMD_GFX12]), Some(0x1002));
        assert_eq!(pci_vendor_of(&[I915_X_TILED, LINEAR]), Some(0x8086));
        assert_eq!(pci_vendor_of(&[LINEAR]), None);
        assert_eq!(pci_vendor_of(&[]), None);
    }
}
