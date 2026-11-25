//! Compositor dmabufs as Vulkan images.

use super::vk::{check, Context, Result};
use ash::vk;
use openlink_protocol::video_ipc as vipc;
use std::os::fd::{AsRawFd, IntoRawFd, OwnedFd};

/// One imported compositor texture.
pub struct Imported {
    pub handle: u64,
    pub image: vk::Image,
    pub view: vk::ImageView,
    memory: vk::DeviceMemory,
    /// Kept to export the compositor's implicit write fence each frame.
    fd: OwnedFd,
    pub width: u32,
    pub height: u32,
}

/// The format to read a DRM format as. Always UNORM: the bytes are what the compositor wrote
/// (sRGB-encoded for `_SRGB` swap textures), and video is coded in that same gamma domain.
pub fn vk_format_for(drm_format: u32) -> Option<vk::Format> {
    Some(match &drm_format.to_le_bytes() {
        b"AB24" | b"XB24" => vk::Format::R8G8B8A8_UNORM,
        b"AR24" | b"XR24" => vk::Format::B8G8R8A8_UNORM,
        b"AB30" | b"XB30" => vk::Format::A2B10G10R10_UNORM_PACK32,
        b"AR30" | b"XR30" => vk::Format::A2R10G10B10_UNORM_PACK32,
        _ => return None,
    })
}

impl Imported {
    /// `fds` holds one fd per plane of `tex`; plane 0's fd names the buffer.
    pub fn new(ctx: &Context, set: &vipc::TextureSet, tex: &vipc::DmabufTexture, fds: Vec<OwnedFd>) -> Result<Self> {
        let format = vk_format_for(set.drm_format)
            .ok_or_else(|| format!("unsupported DRM format {:#x}", set.drm_format))?;
        let fd = fds.into_iter().next().ok_or("texture without fds")?;
        let layouts: Vec<vk::SubresourceLayout> = tex
            .planes
            .iter()
            .map(|p| vk::SubresourceLayout {
                offset: p.offset as u64,
                row_pitch: p.stride as u64,
                ..Default::default()
            })
            .collect();
        let mut modifier = vk::ImageDrmFormatModifierExplicitCreateInfoEXT::default()
            .drm_format_modifier(set.modifier)
            .plane_layouts(&layouts);
        let mut external = vk::ExternalMemoryImageCreateInfo::default()
            .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
        // No modifier (an image SteamVR allocated for an application): the layout lives in the
        // buffer's kernel metadata, which RADV applies to an optimal-tiling image bound to a
        // dedicated import.
        let implicit = set.modifier == vipc::DRM_FORMAT_MOD_INVALID;
        let info = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(format)
            .extent(vk::Extent3D { width: set.width, height: set.height, depth: 1 })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(if implicit { vk::ImageTiling::OPTIMAL } else { vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT })
            .usage(vk::ImageUsageFlags::SAMPLED)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED)
            .push_next(&mut external);
        let info = if implicit { info } else { info.push_next(&mut modifier) };
        let d = &ctx.device;
        // SAFETY: valid create info; every object is destroyed on error or in Drop.
        unsafe {
            let image = check(d.create_image(&info, None), "vkCreateImage (dmabuf)")?;
            let fail = |e: String| {
                d.destroy_image(image, None);
                e
            };
            let req = d.get_image_memory_requirements(image);
            let mut fd_props = vk::MemoryFdPropertiesKHR::default();
            check(
                ctx.ext_mem_fd.get_memory_fd_properties(
                    vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT,
                    fd.as_raw_fd(),
                    &mut fd_props,
                ),
                "vkGetMemoryFdPropertiesKHR",
            )
            .map_err(fail)?;
            let ty = ctx
                .memory_type(req.memory_type_bits & fd_props.memory_type_bits, vk::MemoryPropertyFlags::empty())
                .ok_or_else(|| fail("no memory type can import this dmabuf".into()))?;
            // Vulkan takes ownership of the fd it imports, so give it a duplicate.
            let dup = fd.try_clone().map_err(|e| fail(format!("dup: {e}")))?;
            let mut import = vk::ImportMemoryFdInfoKHR::default()
                .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT)
                .fd(dup.as_raw_fd());
            let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(image);
            let alloc = vk::MemoryAllocateInfo::default()
                .allocation_size(req.size)
                .memory_type_index(ty)
                .push_next(&mut import)
                .push_next(&mut dedicated);
            let memory = check(d.allocate_memory(&alloc, None), "vkAllocateMemory (dmabuf)").map_err(fail)?;
            // Success: the driver owns the duplicate now.
            let _ = dup.into_raw_fd();
            if let Err(e) = check(d.bind_image_memory(image, memory, 0), "vkBindImageMemory (dmabuf)") {
                d.free_memory(memory, None);
                return Err(fail(e));
            }
            let view_info = vk::ImageViewCreateInfo::default()
                .image(image)
                .view_type(vk::ImageViewType::TYPE_2D)
                .format(format)
                .subresource_range(color_range());
            let view = match check(d.create_image_view(&view_info, None), "vkCreateImageView (dmabuf)") {
                Ok(v) => v,
                Err(e) => {
                    d.free_memory(memory, None);
                    return Err(fail(e));
                }
            };
            Ok(Self { handle: tex.handle, image, view, memory, fd, width: set.width, height: set.height })
        }
    }

    /// A sync file that signals when the compositor's pending writes to this texture are done
    /// (the dmabuf's implicit fences). `None` if the kernel cannot export one.
    pub fn write_fence(&self) -> Option<OwnedFd> {
        #[repr(C)]
        struct ExportSyncFile {
            flags: u32,
            fd: i32,
        }
        const DMA_BUF_SYNC_READ: u32 = 1;
        // _IOWR('b', 2, struct dma_buf_export_sync_file)
        const DMA_BUF_IOCTL_EXPORT_SYNC_FILE: libc::c_ulong = 0xC008_6202;
        let mut arg = ExportSyncFile { flags: DMA_BUF_SYNC_READ, fd: -1 };
        // SAFETY: valid dmabuf fd and argument struct.
        let r = unsafe { libc::ioctl(self.fd.as_raw_fd(), DMA_BUF_IOCTL_EXPORT_SYNC_FILE, &mut arg) };
        if r < 0 || arg.fd < 0 {
            return None;
        }
        // SAFETY: the kernel handed us this fd.
        Some(unsafe { std::os::fd::FromRawFd::from_raw_fd(arg.fd) })
    }

    pub fn destroy(self, ctx: &Context) {
        // SAFETY: the caller guarantees the GPU no longer uses the image.
        unsafe {
            ctx.device.destroy_image_view(self.view, None);
            ctx.device.destroy_image(self.image, None);
            ctx.device.free_memory(self.memory, None);
        }
    }
}

pub fn color_range() -> vk::ImageSubresourceRange {
    vk::ImageSubresourceRange {
        aspect_mask: vk::ImageAspectFlags::COLOR,
        base_mip_level: 0,
        level_count: 1,
        base_array_layer: 0,
        layer_count: 1,
    }
}
