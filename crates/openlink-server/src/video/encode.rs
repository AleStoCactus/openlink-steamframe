//! H.265 encode with Vulkan Video (`VK_KHR_video_encode_h265`).
//!
//! Low-latency structure: one IDR, then P frames that each reference only the previous frame
//! (two DPB slots used alternately), no B frames, no reordering. Rate control is CBR when the
//! implementation offers it, with a VBV of about one and a half frames so a frame's size stays
//! close to the average.
//!
//! The implementation writes the slice NAL units; VPS/SPS/PPS come from
//! `vkGetEncodedVideoSessionParametersKHR` and are prepended to every IDR.

use super::import::color_range;
use super::vk::{check, Context, Result};
use ash::vk;
use ash::vk::native::*;

const NO_REFERENCE_PICTURE: u8 = 0xFF;
const BITSTREAM_SIZE: u64 = 16 << 20;
/// `log2_max_pic_order_cnt_lsb_minus4` at its maximum: POC LSBs wrap every 65536 frames.
/// (At 4, POC passed its 8-bit LSB range after 256 frames and the encode hung the GPU.)
const LOG2_MAX_POC_LSB_MINUS4: u8 = 12;
/// Frames between forced IDRs, keeping POC below the LSB range so it never wraps
/// (about 6 minutes at 90 fps).
const MAX_GOP: u32 = 1 << 15;

pub struct EncoderConfig {
    /// Picture size (what the decoder shows), before alignment.
    pub width: u32,
    pub height: u32,
    pub fps: f32,
    pub bitrate_bps: u64,
}

/// Size constraints the converter must honour.
pub struct Layout {
    /// Coded size: the picture size rounded up to the encoder's granularity.
    pub coded_width: u32,
    pub coded_height: u32,
}

struct Profile {
    h265: vk::VideoEncodeH265ProfileInfoKHR<'static>,
    usage: vk::VideoEncodeUsageInfoKHR<'static>,
    info: vk::VideoProfileInfoKHR<'static>,
}

impl Profile {
    /// Boxed so the `p_next` chain stays valid when moved.
    fn new() -> Box<Self> {
        let mut p = Box::new(Self {
            h265: vk::VideoEncodeH265ProfileInfoKHR::default()
                .std_profile_idc(StdVideoH265ProfileIdc_STD_VIDEO_H265_PROFILE_IDC_MAIN),
            usage: vk::VideoEncodeUsageInfoKHR::default()
                .video_usage_hints(vk::VideoEncodeUsageFlagsKHR::STREAMING)
                .video_content_hints(vk::VideoEncodeContentFlagsKHR::RENDERED)
                .tuning_mode(vk::VideoEncodeTuningModeKHR::ULTRA_LOW_LATENCY),
            info: vk::VideoProfileInfoKHR::default()
                .video_codec_operation(vk::VideoCodecOperationFlagsKHR::ENCODE_H265)
                .chroma_subsampling(vk::VideoChromaSubsamplingFlagsKHR::TYPE_420)
                .luma_bit_depth(vk::VideoComponentBitDepthFlagsKHR::TYPE_8)
                .chroma_bit_depth(vk::VideoComponentBitDepthFlagsKHR::TYPE_8),
        });
        p.usage.p_next = (&raw const p.h265).cast();
        p.info.p_next = (&raw const p.usage).cast();
        p
    }

    fn list(&self) -> vk::VideoProfileListInfoKHR<'_> {
        vk::VideoProfileListInfoKHR::default().profiles(std::slice::from_ref(&self.info))
    }
}

struct Caps {
    video: vk::VideoCapabilitiesKHR<'static>,
    encode: vk::VideoEncodeCapabilitiesKHR<'static>,
    h265: vk::VideoEncodeH265CapabilitiesKHR<'static>,
}

fn query_caps(ctx: &Context, profile: &Profile) -> Result<Caps> {
    let mut h265 = vk::VideoEncodeH265CapabilitiesKHR::default();
    let mut encode = vk::VideoEncodeCapabilitiesKHR::default();
    let mut video = vk::VideoCapabilitiesKHR::default();
    encode.p_next = (&raw mut h265).cast();
    video.p_next = (&raw mut encode).cast();
    // SAFETY: valid physical device, profile and output chain.
    let r = unsafe {
        (ctx.video_queue_instance.get_physical_device_video_capabilities_khr)(ctx.pdev, &profile.info, &mut video)
    };
    check(r.result(), "vkGetPhysicalDeviceVideoCapabilitiesKHR")?;
    video.p_next = std::ptr::null_mut();
    encode.p_next = std::ptr::null_mut();
    Ok(Caps { video, encode, h265 })
}

/// Largest picture the H.265 encoder takes.
pub fn max_extent(ctx: &Context) -> Result<vk::Extent2D> {
    Ok(query_caps(ctx, &Profile::new())?.video.max_coded_extent)
}

/// One encode feedback query result: the flags the pool was created with, then the status.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Feedback {
    offset: u32,
    size: u32,
    status: i32,
}

fn align(v: u32, a: u32) -> u32 {
    v.div_ceil(a.max(1)) * a.max(1)
}

pub struct Encoder {
    profile: Box<Profile>,
    session: vk::VideoSessionKHR,
    session_memory: Vec<vk::DeviceMemory>,
    params: vk::VideoSessionParametersKHR,
    /// VPS + SPS + PPS, prepended to IDR frames.
    headers: Vec<u8>,
    /// NV12 source picture, written by the compute queue, read by the encode queue.
    pub src: vk::Image,
    src_memory: vk::DeviceMemory,
    src_view: vk::ImageView,
    dpb: vk::Image,
    dpb_memory: vk::DeviceMemory,
    dpb_views: [vk::ImageView; 2],
    bitstream: vk::Buffer,
    bitstream_memory: vk::DeviceMemory,
    bitstream_ptr: *const u8,
    query_pool: vk::QueryPool,
    pool: vk::CommandPool,
    cmd: vk::CommandBuffer,
    fence: vk::Fence,
    rc_mode: vk::VideoEncodeRateControlModeFlagsKHR,
    rc_layer: vk::VideoEncodeRateControlLayerInfoKHR<'static>,
    rc_h265: vk::VideoEncodeH265RateControlInfoKHR<'static>,
    vbv_ms: u32,
    constant_qp: i32,
    pub layout: Layout,
    /// Frames since the last IDR.
    frame_in_gop: u32,
    idr_requested: bool,
    dpb_initialised: bool,
}

// SAFETY: the mapped pointer is only used by the owning thread.
unsafe impl Send for Encoder {}

impl Encoder {
    pub fn new(ctx: &Context, cfg: &EncoderConfig) -> Result<Self> {
        let profile = Profile::new();
        let caps = query_caps(ctx, &profile)?;
        let granularity = caps.video.picture_access_granularity;
        let coded_width = align(align(cfg.width, 8), granularity.width.max(caps.encode.encode_input_picture_granularity.width));
        let coded_height = align(align(cfg.height, 8), granularity.height.max(caps.encode.encode_input_picture_granularity.height));
        log::info!(
            "H.265 encode caps: coded {}x{}..{}x{}, granularity {}x{}, {} DPB slots, rate control {:?}, max level {}, ctb {:?}, qp {}..{}",
            caps.video.min_coded_extent.width,
            caps.video.min_coded_extent.height,
            caps.video.max_coded_extent.width,
            caps.video.max_coded_extent.height,
            granularity.width,
            granularity.height,
            caps.video.max_dpb_slots,
            caps.encode.rate_control_modes,
            caps.h265.max_level_idc,
            caps.h265.ctb_sizes,
            caps.h265.min_qp,
            caps.h265.max_qp,
        );
        if coded_width > caps.video.max_coded_extent.width || coded_height > caps.video.max_coded_extent.height {
            return Err(format!(
                "{coded_width}x{coded_height} exceeds the encoder's maximum {}x{}",
                caps.video.max_coded_extent.width, caps.video.max_coded_extent.height
            ));
        }
        if caps.video.max_dpb_slots < 2 || caps.video.max_active_reference_pictures < 1 {
            return Err("encoder lacks the 2 DPB slots / 1 reference we need".into());
        }
        let extent = vk::Extent2D { width: coded_width, height: coded_height };
        let format = vk::Format::G8_B8R8_2PLANE_420_UNORM;
        check_format(ctx, &profile, vk::ImageUsageFlags::VIDEO_ENCODE_SRC_KHR, format)?;
        check_format(ctx, &profile, vk::ImageUsageFlags::VIDEO_ENCODE_DPB_KHR, format)?;

        let d = &ctx.device;
        let vq = &ctx.video_queue;

        // Session.
        let session_info = vk::VideoSessionCreateInfoKHR::default()
            .queue_family_index(ctx.encode_family)
            .video_profile(&profile.info)
            .picture_format(format)
            .max_coded_extent(extent)
            .reference_picture_format(format)
            .max_dpb_slots(2)
            .max_active_reference_pictures(1)
            .std_header_version(&caps.video.std_header_version);
        let mut session = vk::VideoSessionKHR::null();
        // SAFETY: valid create info.
        check(
            unsafe { (vq.create_video_session_khr)(d.handle(), &session_info, std::ptr::null(), &mut session) }.result(),
            "vkCreateVideoSessionKHR",
        )?;
        let session_memory = bind_session_memory(ctx, session)?;

        // Parameter sets.
        let ctb_log2 = if caps.h265.ctb_sizes.contains(vk::VideoEncodeH265CtbSizeFlagsKHR::TYPE_64) {
            6
        } else if caps.h265.ctb_sizes.contains(vk::VideoEncodeH265CtbSizeFlagsKHR::TYPE_32) {
            5
        } else {
            4
        };
        let tb_max_log2 = if caps.h265.transform_block_sizes.contains(vk::VideoEncodeH265TransformBlockSizeFlagsKHR::TYPE_32) {
            5
        } else if caps.h265.transform_block_sizes.contains(vk::VideoEncodeH265TransformBlockSizeFlagsKHR::TYPE_16) {
            4
        } else {
            3
        };
        let mut ptl = StdVideoH265ProfileTierLevel {
            flags: unsafe { std::mem::zeroed() },
            general_profile_idc: StdVideoH265ProfileIdc_STD_VIDEO_H265_PROFILE_IDC_MAIN,
            general_level_idc: caps.h265.max_level_idc,
        };
        ptl.flags.set_general_progressive_source_flag(1);
        ptl.flags.set_general_frame_only_constraint_flag(1);
        let mut dpbm: StdVideoH265DecPicBufMgr = unsafe { std::mem::zeroed() };
        dpbm.max_dec_pic_buffering_minus1[0] = 1;
        dpbm.max_num_reorder_pics[0] = 0;

        let mut vps: StdVideoH265VideoParameterSet = unsafe { std::mem::zeroed() };
        vps.flags.set_vps_temporal_id_nesting_flag(1);
        vps.flags.set_vps_sub_layer_ordering_info_present_flag(1);
        vps.pDecPicBufMgr = &dpbm;
        vps.pProfileTierLevel = &ptl;

        let mut vui: StdVideoH265SequenceParameterSetVui = unsafe { std::mem::zeroed() };
        vui.flags.set_video_signal_type_present_flag(1);
        vui.flags.set_video_full_range_flag(0);
        vui.flags.set_colour_description_present_flag(1);
        vui.video_format = 5; // unspecified
        vui.colour_primaries = 1; // BT.709
        vui.transfer_characteristics = 1;
        vui.matrix_coeffs = 1;

        let mut sps: StdVideoH265SequenceParameterSet = unsafe { std::mem::zeroed() };
        sps.flags.set_sps_temporal_id_nesting_flag(1);
        sps.flags.set_sps_sub_layer_ordering_info_present_flag(1);
        sps.flags.set_vui_parameters_present_flag(1);
        sps.chroma_format_idc = StdVideoH265ChromaFormatIdc_STD_VIDEO_H265_CHROMA_FORMAT_IDC_420;
        sps.pic_width_in_luma_samples = coded_width;
        sps.pic_height_in_luma_samples = coded_height;
        sps.log2_max_pic_order_cnt_lsb_minus4 = LOG2_MAX_POC_LSB_MINUS4;
        sps.log2_min_luma_coding_block_size_minus3 = 0;
        sps.log2_diff_max_min_luma_coding_block_size = ctb_log2 - 3;
        sps.log2_min_luma_transform_block_size_minus2 = 0;
        sps.log2_diff_max_min_luma_transform_block_size = tb_max_log2 - 2;
        sps.max_transform_hierarchy_depth_inter = (ctb_log2 - 2).min(3);
        sps.max_transform_hierarchy_depth_intra = (ctb_log2 - 2).min(3);
        if coded_width != cfg.width || coded_height != cfg.height {
            // Offsets are in chroma samples (SubWidthC = SubHeightC = 2 for 4:2:0).
            sps.flags.set_conformance_window_flag(1);
            sps.conf_win_right_offset = (coded_width - cfg.width) / 2;
            sps.conf_win_bottom_offset = (coded_height - cfg.height) / 2;
        }
        sps.pProfileTierLevel = &ptl;
        sps.pDecPicBufMgr = &dpbm;
        sps.pSequenceParameterSetVui = &vui;

        let mut pps: StdVideoH265PictureParameterSet = unsafe { std::mem::zeroed() };
        pps.flags.set_cu_qp_delta_enabled_flag(1);
        pps.flags.set_pps_loop_filter_across_slices_enabled_flag(1);
        pps.num_ref_idx_l0_default_active_minus1 = 0;

        let (vpss, spss, ppss) = ([vps], [sps], [pps]);
        let add = vk::VideoEncodeH265SessionParametersAddInfoKHR::default()
            .std_vp_ss(&vpss)
            .std_sp_ss(&spss)
            .std_pp_ss(&ppss);
        let mut h265_params = vk::VideoEncodeH265SessionParametersCreateInfoKHR::default()
            .max_std_vps_count(1)
            .max_std_sps_count(1)
            .max_std_pps_count(1)
            .parameters_add_info(&add);
        let params_info = vk::VideoSessionParametersCreateInfoKHR::default()
            .video_session(session)
            .push_next(&mut h265_params);
        let mut params = vk::VideoSessionParametersKHR::null();
        // SAFETY: valid create info; the std structs outlive the call.
        check(
            unsafe { (vq.create_video_session_parameters_khr)(d.handle(), &params_info, std::ptr::null(), &mut params) }
                .result(),
            "vkCreateVideoSessionParametersKHR",
        )?;
        let headers = encoded_headers(ctx, params)?;

        // Images and buffers.
        let families = [ctx.compute_family, ctx.encode_family];
        let mut src_list = profile.list();
        let src_info = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(format)
            .extent(vk::Extent3D { width: coded_width, height: coded_height, depth: 1 })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::OPTIMAL)
            .usage(vk::ImageUsageFlags::VIDEO_ENCODE_SRC_KHR | vk::ImageUsageFlags::TRANSFER_DST)
            .sharing_mode(vk::SharingMode::CONCURRENT)
            .queue_family_indices(&families)
            .initial_layout(vk::ImageLayout::UNDEFINED)
            .push_next(&mut src_list);
        let mut dpb_list = profile.list();
        let dpb_info = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(format)
            .extent(vk::Extent3D { width: coded_width, height: coded_height, depth: 1 })
            .mip_levels(1)
            .array_layers(2)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::OPTIMAL)
            .usage(vk::ImageUsageFlags::VIDEO_ENCODE_DPB_KHR)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED)
            .push_next(&mut dpb_list);
        let view = |image, layer| {
            let info = vk::ImageViewCreateInfo::default()
                .image(image)
                .view_type(vk::ImageViewType::TYPE_2D)
                .format(format)
                .subresource_range(vk::ImageSubresourceRange { base_array_layer: layer, ..color_range() });
            // SAFETY: valid create info.
            check(unsafe { d.create_image_view(&info, None) }, "vkCreateImageView (video)")
        };
        let mut bs_list = profile.list();
        let bs_info = vk::BufferCreateInfo::default()
            .size(BITSTREAM_SIZE)
            .usage(vk::BufferUsageFlags::VIDEO_ENCODE_DST_KHR)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .push_next(&mut bs_list);
        let mut feedback = vk::QueryPoolVideoEncodeFeedbackCreateInfoKHR::default().encode_feedback_flags(
            vk::VideoEncodeFeedbackFlagsKHR::BITSTREAM_BUFFER_OFFSET | vk::VideoEncodeFeedbackFlagsKHR::BITSTREAM_BYTES_WRITTEN,
        );
        let mut qp_profile = profile.info;
        qp_profile.p_next = profile.info.p_next;
        feedback.p_next = (&raw const qp_profile).cast();
        let mut qp_info = vk::QueryPoolCreateInfo::default()
            .query_type(vk::QueryType::VIDEO_ENCODE_FEEDBACK_KHR)
            .query_count(1);
        qp_info.p_next = (&raw const feedback).cast();

        // SAFETY: valid create infos throughout.
        let (src, src_memory, src_view, dpb, dpb_memory, dpb_views, bitstream, bitstream_memory, bitstream_ptr, query_pool) = unsafe {
            let src = check(d.create_image(&src_info, None), "vkCreateImage (encode src)")?;
            let src_memory = ctx.bind_image_memory(src, vk::MemoryPropertyFlags::DEVICE_LOCAL)?;
            let src_view = view(src, 0)?;
            let dpb = check(d.create_image(&dpb_info, None), "vkCreateImage (DPB)")?;
            let dpb_memory = ctx.bind_image_memory(dpb, vk::MemoryPropertyFlags::DEVICE_LOCAL)?;
            let dpb_views = [view(dpb, 0)?, view(dpb, 1)?];
            let bitstream = check(d.create_buffer(&bs_info, None), "vkCreateBuffer (bitstream)")?;
            let bitstream_memory = ctx
                .bind_buffer_memory(
                    bitstream,
                    vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT | vk::MemoryPropertyFlags::HOST_CACHED,
                )
                .or_else(|_| {
                    ctx.bind_buffer_memory(bitstream, vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT)
                })?;
            let bitstream_ptr = check(
                d.map_memory(bitstream_memory, 0, vk::WHOLE_SIZE, vk::MemoryMapFlags::empty()),
                "vkMapMemory",
            )? as *const u8;
            let query_pool = check(d.create_query_pool(&qp_info, None), "vkCreateQueryPool")?;
            (src, src_memory, src_view, dpb, dpb_memory, dpb_views, bitstream, bitstream_memory, bitstream_ptr, query_pool)
        };

        let pool = ctx.command_pool(ctx.encode_family)?;
        let cmd = ctx.command_buffer(pool)?;
        let fence = ctx.fence()?;

        let modes = caps.encode.rate_control_modes;
        let rc_mode = [
            vk::VideoEncodeRateControlModeFlagsKHR::CBR,
            vk::VideoEncodeRateControlModeFlagsKHR::VBR,
            vk::VideoEncodeRateControlModeFlagsKHR::DISABLED,
        ]
        .into_iter()
        .find(|m| modes.contains(*m))
        .unwrap_or(vk::VideoEncodeRateControlModeFlagsKHR::DEFAULT);
        let bitrate = cfg.bitrate_bps.min(caps.encode.max_bitrate.max(1));
        let fps_milli = (cfg.fps * 1000.0).round() as u32;
        let rc_layer = vk::VideoEncodeRateControlLayerInfoKHR::default()
            .average_bitrate(bitrate)
            .max_bitrate(bitrate)
            .frame_rate_numerator(fps_milli)
            .frame_rate_denominator(1000);
        let rc_h265 = vk::VideoEncodeH265RateControlInfoKHR::default()
            .flags(vk::VideoEncodeH265RateControlFlagsKHR::REFERENCE_PATTERN_FLAT)
            .gop_frame_count(0)
            .idr_period(0)
            .consecutive_b_frame_count(0)
            .sub_layer_count(1);
        let vbv_ms = ((1500.0 / cfg.fps).ceil() as u32).max(1);
        log::info!(
            "H.265 encoder: {}x{} (coded {coded_width}x{coded_height}) @ {} fps, {:.0} Mbit/s, rate control {rc_mode:?}, VBV {vbv_ms} ms",
            cfg.width,
            cfg.height,
            cfg.fps,
            bitrate as f64 / 1e6
        );

        let mut enc = Self {
            profile,
            session,
            session_memory,
            params,
            headers,
            src,
            src_memory,
            src_view,
            dpb,
            dpb_memory,
            dpb_views,
            bitstream,
            bitstream_memory,
            bitstream_ptr,
            query_pool,
            pool,
            cmd,
            fence,
            rc_mode,
            rc_layer,
            rc_h265,
            vbv_ms,
            constant_qp: 26.clamp(caps.h265.min_qp, caps.h265.max_qp),
            layout: Layout { coded_width, coded_height },
            frame_in_gop: 0,
            idr_requested: true,
            dpb_initialised: false,
        };
        enc.reset(ctx)?;
        Ok(enc)
    }

    /// The next frame will be an IDR (e.g. after the client reports loss).
    pub fn request_idr(&mut self) {
        self.idr_requested = true;
    }

    fn rate_control_info<'a>(
        &'a self,
        layers: &'a [vk::VideoEncodeRateControlLayerInfoKHR<'a>],
    ) -> vk::VideoEncodeRateControlInfoKHR<'a> {
        let mut rc = vk::VideoEncodeRateControlInfoKHR::default().rate_control_mode(self.rc_mode);
        if self.rc_mode != vk::VideoEncodeRateControlModeFlagsKHR::DISABLED
            && self.rc_mode != vk::VideoEncodeRateControlModeFlagsKHR::DEFAULT
        {
            rc = rc.layers(layers).virtual_buffer_size_in_ms(self.vbv_ms).initial_virtual_buffer_size_in_ms(self.vbv_ms);
        }
        rc.p_next = (&raw const self.rc_h265).cast();
        rc
    }

    /// Resets the session and applies rate control (once, before the first frame).
    fn reset(&mut self, ctx: &Context) -> Result<()> {
        let d = &ctx.device;
        let vq = &ctx.video_queue;
        let layers = [self.rc_layer];
        let rc = self.rate_control_info(&layers);
        let begin = vk::VideoBeginCodingInfoKHR::default()
            .video_session(self.session)
            .video_session_parameters(self.params);
        let mut control = vk::VideoCodingControlInfoKHR::default()
            .flags(vk::VideoCodingControlFlagsKHR::RESET | vk::VideoCodingControlFlagsKHR::ENCODE_RATE_CONTROL);
        control.p_next = (&raw const rc).cast();
        // SAFETY: recording into our own command buffer; all handles valid.
        unsafe {
            check(
                d.begin_command_buffer(self.cmd, &vk::CommandBufferBeginInfo::default().flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT)),
                "vkBeginCommandBuffer",
            )?;
            (vq.cmd_begin_video_coding_khr)(self.cmd, &begin);
            (vq.cmd_control_video_coding_khr)(self.cmd, &control);
            (vq.cmd_end_video_coding_khr)(self.cmd, &vk::VideoEndCodingInfoKHR::default());
            check(d.end_command_buffer(self.cmd), "vkEndCommandBuffer")?;
            let cmds = [self.cmd];
            check(
                d.queue_submit(ctx.encode_queue, &[vk::SubmitInfo::default().command_buffers(&cmds)], self.fence),
                "vkQueueSubmit (reset)",
            )?;
            ctx.wait_fence(self.fence, "encoder reset")?;
        }
        Ok(())
    }

    /// Encodes the picture in `src` (in `TRANSFER_DST_OPTIMAL`, written by work that signals
    /// `wait`) and returns the Annex B bitstream and whether it is an IDR. Blocks until the
    /// GPU is done.
    pub fn encode(&mut self, ctx: &Context, wait: vk::Semaphore) -> Result<(Vec<u8>, bool)> {
        let d = &ctx.device;
        let vq = &ctx.video_queue;
        let idr = std::mem::take(&mut self.idr_requested) || self.frame_in_gop == 0 || self.frame_in_gop >= MAX_GOP;
        if idr {
            self.frame_in_gop = 0;
        }
        let n = self.frame_in_gop;
        let setup_slot = (n % 2) as usize;
        let ref_slot = 1 - setup_slot;
        let poc = n as i32;
        let extent = vk::Extent2D { width: self.layout.coded_width, height: self.layout.coded_height };
        let picture = |view| {
            vk::VideoPictureResourceInfoKHR::default().coded_extent(extent).image_view_binding(view)
        };
        let (src_pic, setup_pic, ref_pic) = (picture(self.src_view), picture(self.dpb_views[setup_slot]), picture(self.dpb_views[ref_slot]));

        let pic_type = if idr {
            StdVideoH265PictureType_STD_VIDEO_H265_PICTURE_TYPE_IDR
        } else {
            StdVideoH265PictureType_STD_VIDEO_H265_PICTURE_TYPE_P
        };
        let ref_info = |pic_type, poc| StdVideoEncodeH265ReferenceInfo {
            flags: unsafe { std::mem::zeroed() },
            pic_type,
            PicOrderCntVal: poc,
            TemporalId: 0,
        };
        let setup_std = ref_info(pic_type, poc);
        let prev_type = if n == 1 {
            StdVideoH265PictureType_STD_VIDEO_H265_PICTURE_TYPE_IDR
        } else {
            StdVideoH265PictureType_STD_VIDEO_H265_PICTURE_TYPE_P
        };
        let ref_std = ref_info(prev_type, poc - 1);
        let setup_dpb = vk::VideoEncodeH265DpbSlotInfoKHR::default().std_reference_info(&setup_std);
        let ref_dpb = vk::VideoEncodeH265DpbSlotInfoKHR::default().std_reference_info(&ref_std);
        let mut setup_slot_info = vk::VideoReferenceSlotInfoKHR::default()
            .slot_index(setup_slot as i32)
            .picture_resource(&setup_pic);
        setup_slot_info.p_next = (&raw const setup_dpb).cast();
        let mut ref_slot_info = vk::VideoReferenceSlotInfoKHR::default().slot_index(ref_slot as i32).picture_resource(&ref_pic);
        ref_slot_info.p_next = (&raw const ref_dpb).cast();
        // In vkCmdBeginVideoCoding the slot being (re)written is listed with index -1.
        let mut begin_setup = setup_slot_info;
        begin_setup.slot_index = -1;
        let begin_slots_p = [ref_slot_info, begin_setup];
        let begin_slots_i = [begin_setup];
        let begin_slots: &[vk::VideoReferenceSlotInfoKHR] = if idr { &begin_slots_i } else { &begin_slots_p };

        let mut rps: StdVideoH265ShortTermRefPicSet = unsafe { std::mem::zeroed() };
        let mut lists: StdVideoEncodeH265ReferenceListsInfo = unsafe { std::mem::zeroed() };
        lists.RefPicList0 = [NO_REFERENCE_PICTURE; 15];
        lists.RefPicList1 = [NO_REFERENCE_PICTURE; 15];
        if !idr {
            rps.num_negative_pics = 1;
            rps.delta_poc_s0_minus1[0] = 0;
            rps.used_by_curr_pic_s0_flag = 1;
            lists.num_ref_idx_l0_active_minus1 = 0;
            lists.RefPicList0[0] = ref_slot as u8;
        }
        let mut std_pic: StdVideoEncodeH265PictureInfo = unsafe { std::mem::zeroed() };
        std_pic.pic_type = pic_type;
        std_pic.flags.set_is_reference(1);
        std_pic.flags.set_pic_output_flag(1);
        std_pic.flags.set_IrapPicFlag(idr as u32);
        std_pic.PicOrderCntVal = poc;
        std_pic.pRefLists = &lists;
        std_pic.pShortTermRefPicSet = &rps;

        let mut slice: StdVideoEncodeH265SliceSegmentHeader = unsafe { std::mem::zeroed() };
        slice.flags.set_first_slice_segment_in_pic_flag(1);
        slice.flags.set_slice_loop_filter_across_slices_enabled_flag(1);
        slice.slice_type = if idr {
            StdVideoH265SliceType_STD_VIDEO_H265_SLICE_TYPE_I
        } else {
            StdVideoH265SliceType_STD_VIDEO_H265_SLICE_TYPE_P
        };
        slice.MaxNumMergeCand = 5;
        let mut nalu = vk::VideoEncodeH265NaluSliceSegmentInfoKHR::default().std_slice_segment_header(&slice);
        if self.rc_mode == vk::VideoEncodeRateControlModeFlagsKHR::DISABLED {
            nalu.constant_qp = self.constant_qp;
        }
        let nalus = [nalu];
        let h265_pic = vk::VideoEncodeH265PictureInfoKHR::default()
            .nalu_slice_segment_entries(&nalus)
            .std_picture_info(&std_pic);

        let ref_slots = [ref_slot_info];
        let mut encode_info = vk::VideoEncodeInfoKHR::default()
            .dst_buffer(self.bitstream)
            .dst_buffer_offset(0)
            .dst_buffer_range(BITSTREAM_SIZE)
            .src_picture_resource(src_pic)
            .setup_reference_slot(&setup_slot_info);
        if !idr {
            encode_info = encode_info.reference_slots(&ref_slots);
        }
        encode_info.p_next = (&raw const h265_pic).cast();

        let layers = [self.rc_layer];
        let rc = self.rate_control_info(&layers);
        let mut begin = vk::VideoBeginCodingInfoKHR::default()
            .video_session(self.session)
            .video_session_parameters(self.params)
            .reference_slots(begin_slots);
        begin.p_next = (&raw const rc).cast();

        let src_barrier = vk::ImageMemoryBarrier2::default()
            .image(self.src)
            .subresource_range(color_range())
            .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
            .new_layout(vk::ImageLayout::VIDEO_ENCODE_SRC_KHR)
            .src_stage_mask(vk::PipelineStageFlags2::NONE)
            .src_access_mask(vk::AccessFlags2::NONE)
            .dst_stage_mask(vk::PipelineStageFlags2::VIDEO_ENCODE_KHR)
            .dst_access_mask(vk::AccessFlags2::VIDEO_ENCODE_READ_KHR);
        let dpb_barrier = vk::ImageMemoryBarrier2::default()
            .image(self.dpb)
            .subresource_range(vk::ImageSubresourceRange { layer_count: 2, ..color_range() })
            .old_layout(vk::ImageLayout::UNDEFINED)
            .new_layout(vk::ImageLayout::VIDEO_ENCODE_DPB_KHR)
            .src_stage_mask(vk::PipelineStageFlags2::NONE)
            .src_access_mask(vk::AccessFlags2::NONE)
            .dst_stage_mask(vk::PipelineStageFlags2::VIDEO_ENCODE_KHR)
            .dst_access_mask(vk::AccessFlags2::VIDEO_ENCODE_READ_KHR | vk::AccessFlags2::VIDEO_ENCODE_WRITE_KHR);
        let barriers_first = [src_barrier, dpb_barrier];
        let barriers = [src_barrier];
        let first = !self.dpb_initialised;
        self.dpb_initialised = true;

        // SAFETY: recording into our own command buffer (idle: we wait for its fence every
        // frame); all structures outlive the recording calls.
        unsafe {
            // Reset from the host, keeping the encode command buffer to video commands only.
            d.reset_query_pool(self.query_pool, 0, 1);
            check(
                d.begin_command_buffer(self.cmd, &vk::CommandBufferBeginInfo::default().flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT)),
                "vkBeginCommandBuffer",
            )?;
            d.cmd_pipeline_barrier2(
                self.cmd,
                &vk::DependencyInfo::default().image_memory_barriers(if first { &barriers_first } else { &barriers }),
            );
            (vq.cmd_begin_video_coding_khr)(self.cmd, &begin);
            d.cmd_begin_query(self.cmd, self.query_pool, 0, vk::QueryControlFlags::empty());
            (ctx.video_encode.cmd_encode_video_khr)(self.cmd, &encode_info);
            d.cmd_end_query(self.cmd, self.query_pool, 0);
            (vq.cmd_end_video_coding_khr)(self.cmd, &vk::VideoEndCodingInfoKHR::default());
            check(d.end_command_buffer(self.cmd), "vkEndCommandBuffer")?;

            let cmds = [self.cmd];
            let waits = [wait];
            let stages = [vk::PipelineStageFlags::ALL_COMMANDS];
            let submit = vk::SubmitInfo::default()
                .command_buffers(&cmds)
                .wait_semaphores(&waits)
                .wait_dst_stage_mask(&stages);
            check(d.queue_submit(ctx.encode_queue, &[submit], self.fence), "vkQueueSubmit (encode)")?;
            ctx.wait_fence(self.fence, "encode")?;

            // No WAIT: the fence has signalled, so the feedback is written or never will be
            // (RADV busy-polls for it, and gives up only after its own long timeout).
            // One element = one query: ash passes the slice length as the query count.
            let mut result = [Feedback::default()];
            check(
                d.get_query_pool_results(self.query_pool, 0, &mut result, vk::QueryResultFlags::WITH_STATUS_KHR),
                "vkGetQueryPoolResults (encode feedback)",
            )?;
            let Feedback { offset, size, status } = result[0];
            if status <= 0 {
                return Err(format!("encode failed with query status {status}"));
            }
            let mut out = Vec::with_capacity(size as usize + if idr { self.headers.len() } else { 0 });
            if idr {
                out.extend_from_slice(&self.headers);
            }
            out.extend_from_slice(std::slice::from_raw_parts(self.bitstream_ptr.add(offset as usize), size as usize));
            self.frame_in_gop += 1;
            Ok((out, idr))
        }
    }

    pub fn destroy(self, ctx: &Context) {
        let d = &ctx.device;
        let vq = &ctx.video_queue;
        // SAFETY: the caller guarantees the GPU is idle.
        unsafe {
            d.destroy_fence(self.fence, None);
            d.destroy_command_pool(self.pool, None);
            d.destroy_query_pool(self.query_pool, None);
            d.unmap_memory(self.bitstream_memory);
            d.destroy_buffer(self.bitstream, None);
            d.free_memory(self.bitstream_memory, None);
            for v in self.dpb_views {
                d.destroy_image_view(v, None);
            }
            d.destroy_image(self.dpb, None);
            d.free_memory(self.dpb_memory, None);
            d.destroy_image_view(self.src_view, None);
            d.destroy_image(self.src, None);
            d.free_memory(self.src_memory, None);
            (vq.destroy_video_session_parameters_khr)(d.handle(), self.params, std::ptr::null());
            (vq.destroy_video_session_khr)(d.handle(), self.session, std::ptr::null());
            for m in self.session_memory {
                d.free_memory(m, None);
            }
        }
        drop(self.profile);
    }
}

fn check_format(ctx: &Context, profile: &Profile, usage: vk::ImageUsageFlags, want: vk::Format) -> Result<()> {
    let mut list = profile.list();
    let info = vk::PhysicalDeviceVideoFormatInfoKHR::default().image_usage(usage).push_next(&mut list);
    let f = &ctx.video_queue_instance.get_physical_device_video_format_properties_khr;
    let mut n = 0u32;
    // SAFETY: Vulkan two-call enumeration with valid pointers.
    unsafe {
        check(f(ctx.pdev, &info, &mut n, std::ptr::null_mut()).result(), "vkGetPhysicalDeviceVideoFormatPropertiesKHR")?;
        let mut props = vec![vk::VideoFormatPropertiesKHR::default(); n as usize];
        check(f(ctx.pdev, &info, &mut n, props.as_mut_ptr()).result(), "vkGetPhysicalDeviceVideoFormatPropertiesKHR")?;
        if props.iter().any(|p| p.format == want) {
            Ok(())
        } else {
            Err(format!(
                "encoder does not take {want:?} for {usage:?} (offers {:?})",
                props.iter().map(|p| p.format).collect::<Vec<_>>()
            ))
        }
    }
}

fn bind_session_memory(ctx: &Context, session: vk::VideoSessionKHR) -> Result<Vec<vk::DeviceMemory>> {
    let d = &ctx.device;
    let vq = &ctx.video_queue;
    let mut n = 0u32;
    // SAFETY: two-call enumeration, then allocation and binding of what the driver asked for.
    unsafe {
        check(
            (vq.get_video_session_memory_requirements_khr)(d.handle(), session, &mut n, std::ptr::null_mut()).result(),
            "vkGetVideoSessionMemoryRequirementsKHR",
        )?;
        let mut reqs = vec![vk::VideoSessionMemoryRequirementsKHR::default(); n as usize];
        check(
            (vq.get_video_session_memory_requirements_khr)(d.handle(), session, &mut n, reqs.as_mut_ptr()).result(),
            "vkGetVideoSessionMemoryRequirementsKHR",
        )?;
        let mut memory = Vec::new();
        let mut binds = Vec::new();
        for r in &reqs {
            let req = r.memory_requirements;
            let ty = ctx
                .memory_type(req.memory_type_bits, vk::MemoryPropertyFlags::DEVICE_LOCAL)
                .or_else(|| ctx.memory_type(req.memory_type_bits, vk::MemoryPropertyFlags::empty()))
                .ok_or("no memory type for the video session")?;
            let mem = check(
                d.allocate_memory(&vk::MemoryAllocateInfo::default().allocation_size(req.size).memory_type_index(ty), None),
                "vkAllocateMemory (session)",
            )?;
            memory.push(mem);
            binds.push(
                vk::BindVideoSessionMemoryInfoKHR::default()
                    .memory_bind_index(r.memory_bind_index)
                    .memory(mem)
                    .memory_offset(0)
                    .memory_size(req.size),
            );
        }
        check(
            (vq.bind_video_session_memory_khr)(d.handle(), session, binds.len() as u32, binds.as_ptr()).result(),
            "vkBindVideoSessionMemoryKHR",
        )?;
        Ok(memory)
    }
}

fn encoded_headers(ctx: &Context, params: vk::VideoSessionParametersKHR) -> Result<Vec<u8>> {
    let mut h265 = vk::VideoEncodeH265SessionParametersGetInfoKHR::default()
        .write_std_vps(true)
        .write_std_sps(true)
        .write_std_pps(true);
    let info = vk::VideoEncodeSessionParametersGetInfoKHR::default()
        .video_session_parameters(params)
        .push_next(&mut h265);
    let f = &ctx.video_encode.get_encoded_video_session_parameters_khr;
    let mut size = 0usize;
    // SAFETY: two-call query with valid pointers.
    unsafe {
        check(
            f(ctx.device.handle(), &info, std::ptr::null_mut(), &mut size, std::ptr::null_mut()).result(),
            "vkGetEncodedVideoSessionParametersKHR",
        )?;
        let mut out = vec![0u8; size];
        check(
            f(ctx.device.handle(), &info, std::ptr::null_mut(), &mut size, out.as_mut_ptr().cast()).result(),
            "vkGetEncodedVideoSessionParametersKHR",
        )?;
        out.truncate(size);
        Ok(out)
    }
}
