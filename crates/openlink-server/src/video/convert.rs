//! RGBA eye textures -> one side-by-side NV12 frame, on the compute queue.
//!
//! The shader writes separate Y (R8) and CbCr (R8G8) storage images, which are then copied into
//! the planes of the encoder's NV12 source image: encode-source images cannot portably be
//! storage images.

use super::import::color_range;
use super::vk::{check, Context, Result};
use ash::vk;

static SPIRV: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/rgba_to_nv12.comp.spv"));

#[repr(C)]
#[derive(Clone, Copy)]
struct Push {
    bounds: [[f32; 4]; 2],
    overlay_bounds: [[f32; 4]; 2],
    eye_size: [i32; 2],
    has_overlay: i32,
    _pad: i32,
}

struct Plane {
    image: vk::Image,
    view: vk::ImageView,
    memory: vk::DeviceMemory,
}

pub struct Converter {
    set_layout: vk::DescriptorSetLayout,
    layout: vk::PipelineLayout,
    pipeline: vk::Pipeline,
    pool: vk::DescriptorPool,
    set: vk::DescriptorSet,
    sampler: vk::Sampler,
    y: Plane,
    cbcr: Plane,
    /// Coded frame size (even).
    pub width: u32,
    pub height: u32,
    /// Picture size of one eye inside the frame.
    pub eye_width: u32,
    pub eye_height: u32,
    fresh: bool,
}

fn plane(ctx: &Context, format: vk::Format, w: u32, h: u32) -> Result<Plane> {
    let info = vk::ImageCreateInfo::default()
        .image_type(vk::ImageType::TYPE_2D)
        .format(format)
        .extent(vk::Extent3D { width: w, height: h, depth: 1 })
        .mip_levels(1)
        .array_layers(1)
        .samples(vk::SampleCountFlags::TYPE_1)
        .tiling(vk::ImageTiling::OPTIMAL)
        .usage(vk::ImageUsageFlags::STORAGE | vk::ImageUsageFlags::TRANSFER_SRC)
        .sharing_mode(vk::SharingMode::EXCLUSIVE)
        .initial_layout(vk::ImageLayout::UNDEFINED);
    // SAFETY: valid create infos.
    unsafe {
        let image = check(ctx.device.create_image(&info, None), "vkCreateImage (plane)")?;
        let memory = ctx.bind_image_memory(image, vk::MemoryPropertyFlags::DEVICE_LOCAL)?;
        let view = check(
            ctx.device.create_image_view(
                &vk::ImageViewCreateInfo::default()
                    .image(image)
                    .view_type(vk::ImageViewType::TYPE_2D)
                    .format(format)
                    .subresource_range(color_range()),
                None,
            ),
            "vkCreateImageView (plane)",
        )?;
        Ok(Plane { image, view, memory })
    }
}

impl Converter {
    /// `width`x`height` is the coded frame size; the eyes fill `2 * eye_width` x `eye_height`
    /// of it from the top left.
    pub fn new(ctx: &Context, width: u32, height: u32, eye_width: u32, eye_height: u32) -> Result<Self> {
        assert!(width % 2 == 0 && height % 2 == 0 && 2 * eye_width <= width && eye_height <= height);
        let d = &ctx.device;
        let bindings = [
            vk::DescriptorSetLayoutBinding::default()
                .binding(0)
                .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::COMPUTE),
            vk::DescriptorSetLayoutBinding::default()
                .binding(1)
                .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::COMPUTE),
            vk::DescriptorSetLayoutBinding::default()
                .binding(2)
                .descriptor_type(vk::DescriptorType::STORAGE_IMAGE)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::COMPUTE),
            vk::DescriptorSetLayoutBinding::default()
                .binding(3)
                .descriptor_type(vk::DescriptorType::STORAGE_IMAGE)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::COMPUTE),
            vk::DescriptorSetLayoutBinding::default()
                .binding(4)
                .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::COMPUTE),
            vk::DescriptorSetLayoutBinding::default()
                .binding(5)
                .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::COMPUTE),
        ];
        // SAFETY: valid create infos throughout.
        unsafe {
            let set_layout = check(
                d.create_descriptor_set_layout(&vk::DescriptorSetLayoutCreateInfo::default().bindings(&bindings), None),
                "vkCreateDescriptorSetLayout",
            )?;
            let push = [vk::PushConstantRange::default()
                .stage_flags(vk::ShaderStageFlags::COMPUTE)
                .size(std::mem::size_of::<Push>() as u32)];
            let set_layouts = [set_layout];
            let layout = check(
                d.create_pipeline_layout(
                    &vk::PipelineLayoutCreateInfo::default().set_layouts(&set_layouts).push_constant_ranges(&push),
                    None,
                ),
                "vkCreatePipelineLayout",
            )?;
            let code = ash::util::read_spv(&mut std::io::Cursor::new(SPIRV)).map_err(|e| e.to_string())?;
            let module = check(
                d.create_shader_module(&vk::ShaderModuleCreateInfo::default().code(&code), None),
                "vkCreateShaderModule",
            )?;
            let stage = vk::PipelineShaderStageCreateInfo::default()
                .stage(vk::ShaderStageFlags::COMPUTE)
                .module(module)
                .name(c"main");
            let pipeline = d
                .create_compute_pipelines(
                    vk::PipelineCache::null(),
                    &[vk::ComputePipelineCreateInfo::default().stage(stage).layout(layout)],
                    None,
                )
                .map_err(|(_, e)| format!("vkCreateComputePipelines: {e:?}"))?[0];
            d.destroy_shader_module(module, None);

            let sizes = [
                vk::DescriptorPoolSize { ty: vk::DescriptorType::COMBINED_IMAGE_SAMPLER, descriptor_count: 4 },
                vk::DescriptorPoolSize { ty: vk::DescriptorType::STORAGE_IMAGE, descriptor_count: 2 },
            ];
            let pool = check(
                d.create_descriptor_pool(&vk::DescriptorPoolCreateInfo::default().max_sets(1).pool_sizes(&sizes), None),
                "vkCreateDescriptorPool",
            )?;
            let set = check(
                d.allocate_descriptor_sets(
                    &vk::DescriptorSetAllocateInfo::default().descriptor_pool(pool).set_layouts(&set_layouts),
                ),
                "vkAllocateDescriptorSets",
            )?[0];
            let sampler = check(
                d.create_sampler(
                    &vk::SamplerCreateInfo::default()
                        .mag_filter(vk::Filter::LINEAR)
                        .min_filter(vk::Filter::LINEAR)
                        .address_mode_u(vk::SamplerAddressMode::CLAMP_TO_EDGE)
                        .address_mode_v(vk::SamplerAddressMode::CLAMP_TO_EDGE)
                        .address_mode_w(vk::SamplerAddressMode::CLAMP_TO_EDGE),
                    None,
                ),
                "vkCreateSampler",
            )?;
            let y = plane(ctx, vk::Format::R8_UNORM, width, height)?;
            let cbcr = plane(ctx, vk::Format::R8G8_UNORM, width / 2, height / 2)?;
            let storage = |view| [vk::DescriptorImageInfo::default().image_view(view).image_layout(vk::ImageLayout::GENERAL)];
            let (yi, ci) = (storage(y.view), storage(cbcr.view));
            d.update_descriptor_sets(
                &[
                    vk::WriteDescriptorSet::default()
                        .dst_set(set)
                        .dst_binding(2)
                        .descriptor_type(vk::DescriptorType::STORAGE_IMAGE)
                        .image_info(&yi),
                    vk::WriteDescriptorSet::default()
                        .dst_set(set)
                        .dst_binding(3)
                        .descriptor_type(vk::DescriptorType::STORAGE_IMAGE)
                        .image_info(&ci),
                ],
                &[],
            );
            Ok(Self {
                set_layout,
                layout,
                pipeline,
                pool,
                set,
                sampler,
                y,
                cbcr,
                width,
                height,
                eye_width,
                eye_height,
                fresh: true,
            })
        }
    }

    /// Points the shader at this frame's eye textures. Only call while no submitted work uses
    /// the descriptor set.
    /// `overlay`: the left and right views of a layer to draw over the eyes, if any (the eyes
    /// are bound in its place otherwise, so every binding stays valid).
    pub fn bind_eyes(&self, ctx: &Context, left: vk::ImageView, right: vk::ImageView, overlay: Option<(vk::ImageView, vk::ImageView)>) {
        let info = |view| {
            [vk::DescriptorImageInfo::default()
                .sampler(self.sampler)
                .image_view(view)
                .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)]
        };
        let (l, r) = (info(left), info(right));
        let (ol, or) = overlay.map_or((info(left), info(right)), |(a, b)| (info(a), info(b)));
        // SAFETY: valid set; not in use (caller's contract).
        unsafe {
            ctx.device.update_descriptor_sets(
                &[
                    vk::WriteDescriptorSet::default()
                        .dst_set(self.set)
                        .dst_binding(0)
                        .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                        .image_info(&l),
                    vk::WriteDescriptorSet::default()
                        .dst_set(self.set)
                        .dst_binding(1)
                        .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                        .image_info(&r),
                    vk::WriteDescriptorSet::default()
                        .dst_set(self.set)
                        .dst_binding(4)
                        .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                        .image_info(&ol),
                    vk::WriteDescriptorSet::default()
                        .dst_set(self.set)
                        .dst_binding(5)
                        .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                        .image_info(&or),
                ],
                &[],
            )
        };
    }

    /// Records the conversion, then the copy of Y and CbCr into `nv12`'s planes. The eye
    /// textures must already be in `SHADER_READ_ONLY_OPTIMAL`, and `nv12` in
    /// `TRANSFER_DST_OPTIMAL`.
    pub fn record(
        &mut self,
        ctx: &Context,
        cmd: vk::CommandBuffer,
        bounds: [[f32; 4]; 2],
        overlay_bounds: Option<[[f32; 4]; 2]>,
        nv12: vk::Image,
    ) {
        let d = &ctx.device;
        let to = |p: &Plane, old, new, src_stage, src_access, dst_stage, dst_access| {
            vk::ImageMemoryBarrier2::default()
                .image(p.image)
                .subresource_range(color_range())
                .old_layout(old)
                .new_layout(new)
                .src_stage_mask(src_stage)
                .src_access_mask(src_access)
                .dst_stage_mask(dst_stage)
                .dst_access_mask(dst_access)
        };
        // Previous frame's copy reads -> this frame's shader writes.
        let old = if self.fresh { vk::ImageLayout::UNDEFINED } else { vk::ImageLayout::TRANSFER_SRC_OPTIMAL };
        self.fresh = false;
        let pre = [&self.y, &self.cbcr].map(|p| {
            to(
                p,
                old,
                vk::ImageLayout::GENERAL,
                vk::PipelineStageFlags2::COPY,
                vk::AccessFlags2::TRANSFER_READ,
                vk::PipelineStageFlags2::COMPUTE_SHADER,
                vk::AccessFlags2::SHADER_STORAGE_WRITE,
            )
        });
        let post = [&self.y, &self.cbcr].map(|p| {
            to(
                p,
                vk::ImageLayout::GENERAL,
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                vk::PipelineStageFlags2::COMPUTE_SHADER,
                vk::AccessFlags2::SHADER_STORAGE_WRITE,
                vk::PipelineStageFlags2::COPY,
                vk::AccessFlags2::TRANSFER_READ,
            )
        });
        let push = Push {
            bounds,
            overlay_bounds: overlay_bounds.unwrap_or(bounds),
            eye_size: [self.eye_width as i32, self.eye_height as i32],
            has_overlay: overlay_bounds.is_some() as i32,
            _pad: 0,
        };
        // SAFETY: recording into a command buffer in the recording state; all handles valid.
        unsafe {
            d.cmd_pipeline_barrier2(cmd, &vk::DependencyInfo::default().image_memory_barriers(&pre));
            d.cmd_bind_pipeline(cmd, vk::PipelineBindPoint::COMPUTE, self.pipeline);
            d.cmd_bind_descriptor_sets(cmd, vk::PipelineBindPoint::COMPUTE, self.layout, 0, &[self.set], &[]);
            d.cmd_push_constants(
                cmd,
                self.layout,
                vk::ShaderStageFlags::COMPUTE,
                0,
                std::slice::from_raw_parts((&raw const push).cast(), std::mem::size_of::<Push>()),
            );
            d.cmd_dispatch(cmd, (self.width / 2).div_ceil(8), (self.height / 2).div_ceil(8), 1);
            d.cmd_pipeline_barrier2(cmd, &vk::DependencyInfo::default().image_memory_barriers(&post));
            let copy = |aspect, w, h| {
                [vk::ImageCopy {
                    src_subresource: vk::ImageSubresourceLayers {
                        aspect_mask: vk::ImageAspectFlags::COLOR,
                        mip_level: 0,
                        base_array_layer: 0,
                        layer_count: 1,
                    },
                    src_offset: vk::Offset3D::default(),
                    dst_subresource: vk::ImageSubresourceLayers {
                        aspect_mask: aspect,
                        mip_level: 0,
                        base_array_layer: 0,
                        layer_count: 1,
                    },
                    dst_offset: vk::Offset3D::default(),
                    extent: vk::Extent3D { width: w, height: h, depth: 1 },
                }]
            };
            d.cmd_copy_image(
                cmd,
                self.y.image,
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                nv12,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                &copy(vk::ImageAspectFlags::PLANE_0, self.width, self.height),
            );
            d.cmd_copy_image(
                cmd,
                self.cbcr.image,
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                nv12,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                &copy(vk::ImageAspectFlags::PLANE_1, self.width / 2, self.height / 2),
            );
        }
    }

    pub fn destroy(self, ctx: &Context) {
        let d = &ctx.device;
        // SAFETY: the caller guarantees the GPU is idle.
        unsafe {
            for p in [self.y, self.cbcr] {
                d.destroy_image_view(p.view, None);
                d.destroy_image(p.image, None);
                d.free_memory(p.memory, None);
            }
            d.destroy_sampler(self.sampler, None);
            d.destroy_descriptor_pool(self.pool, None);
            d.destroy_pipeline(self.pipeline, None);
            d.destroy_pipeline_layout(self.layout, None);
            d.destroy_descriptor_set_layout(self.set_layout, None);
        }
    }
}
