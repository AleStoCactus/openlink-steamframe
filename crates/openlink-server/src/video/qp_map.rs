//! Bindings for `VK_KHR_video_encode_quantization_map`, written by hand: the `ash` release we
//! use (0.38, Vulkan 1.3.281) predates the extension. Layouts and values are from `vk.xml`
//! (extension 554). Only what a QP delta map for H.265 needs is here.

use ash::vk;
use std::ffi::{c_void, CStr};

pub const NAME: &CStr = c"VK_KHR_video_encode_quantization_map";

const fn st(offset: i32) -> vk::StructureType {
    vk::StructureType::from_raw(1_000_553_000 + offset)
}
pub const ST_CAPABILITIES: vk::StructureType = st(0);
pub const ST_FORMAT_PROPERTIES: vk::StructureType = st(1);
pub const ST_ENCODE_INFO: vk::StructureType = st(2);
pub const ST_H265_CAPABILITIES: vk::StructureType = st(4);
pub const ST_SESSION_PARAMETERS: vk::StructureType = st(5);
pub const ST_H265_FORMAT_PROPERTIES: vk::StructureType = st(6);
pub const ST_FEATURES: vk::StructureType = st(9);

pub const CAPABILITY_DELTA_MAP: vk::VideoEncodeCapabilityFlagsKHR = vk::VideoEncodeCapabilityFlagsKHR::from_raw(1 << 2);
pub const SESSION_ALLOW_DELTA_MAP: vk::VideoSessionCreateFlagsKHR = vk::VideoSessionCreateFlagsKHR::from_raw(1 << 3);
pub const PARAMETERS_MAP_COMPATIBLE: vk::VideoSessionParametersCreateFlagsKHR =
    vk::VideoSessionParametersCreateFlagsKHR::from_raw(1 << 0);
pub const ENCODE_WITH_DELTA_MAP: vk::VideoEncodeFlagsKHR = vk::VideoEncodeFlagsKHR::from_raw(1 << 0);
pub const USAGE_DELTA_MAP: vk::ImageUsageFlags = vk::ImageUsageFlags::from_raw(1 << 25);
pub const LAYOUT: vk::ImageLayout = vk::ImageLayout::from_raw(1_000_553_000);

/// `VkPhysicalDeviceVideoEncodeQuantizationMapFeaturesKHR`
#[repr(C)]
pub struct Features {
    pub s_type: vk::StructureType,
    pub p_next: *mut c_void,
    pub video_encode_quantization_map: vk::Bool32,
}

/// `VkVideoEncodeQuantizationMapCapabilitiesKHR`
#[repr(C)]
pub struct Capabilities {
    pub s_type: vk::StructureType,
    pub p_next: *mut c_void,
    pub max_extent: vk::Extent2D,
}

/// `VkVideoEncodeH265QuantizationMapCapabilitiesKHR`
#[repr(C)]
pub struct H265Capabilities {
    pub s_type: vk::StructureType,
    pub p_next: *mut c_void,
    pub min_qp_delta: i32,
    pub max_qp_delta: i32,
}

/// `VkVideoFormatQuantizationMapPropertiesKHR`
#[repr(C)]
pub struct FormatProperties {
    pub s_type: vk::StructureType,
    pub p_next: *mut c_void,
    pub texel_size: vk::Extent2D,
}

/// `VkVideoFormatH265QuantizationMapPropertiesKHR`
#[repr(C)]
pub struct H265FormatProperties {
    pub s_type: vk::StructureType,
    pub p_next: *mut c_void,
    pub compatible_ctb_sizes: vk::VideoEncodeH265CtbSizeFlagsKHR,
}

/// `VkVideoEncodeQuantizationMapSessionParametersCreateInfoKHR`
#[repr(C)]
pub struct SessionParameters {
    pub s_type: vk::StructureType,
    pub p_next: *const c_void,
    pub texel_size: vk::Extent2D,
}

/// `VkVideoEncodeQuantizationMapInfoKHR`
#[repr(C)]
pub struct EncodeInfo {
    pub s_type: vk::StructureType,
    pub p_next: *const c_void,
    pub quantization_map: vk::ImageView,
    pub extent: vk::Extent2D,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::{offset_of, size_of};

    #[test]
    fn layouts_match_the_c_structs() {
        // 64-bit: sType 4 + pad 4 + pNext 8 = 16 bytes of header.
        assert_eq!(offset_of!(Features, video_encode_quantization_map), 16);
        assert_eq!(size_of::<Capabilities>(), 24);
        assert_eq!(size_of::<H265Capabilities>(), 24);
        assert_eq!(size_of::<FormatProperties>(), 24);
        assert_eq!(size_of::<H265FormatProperties>(), 24);
        assert_eq!(size_of::<SessionParameters>(), 24);
        assert_eq!(offset_of!(EncodeInfo, extent), 24);
        assert_eq!(size_of::<EncodeInfo>(), 32);
    }
}
