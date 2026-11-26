//! Vulkan device for import, colour conversion and encode, on the GPU the driver allocated the
//! compositor's textures on.

use ash::vk;
use std::ffi::CStr;

pub type Result<T> = std::result::Result<T, String>;

pub fn check<T>(r: std::result::Result<T, vk::Result>, what: &str) -> Result<T> {
    r.map_err(|e| format!("{what}: {e:?}"))
}

pub struct Context {
    _entry: ash::Entry,
    pub instance: ash::Instance,
    pub pdev: vk::PhysicalDevice,
    pub device: ash::Device,
    pub mem_props: vk::PhysicalDeviceMemoryProperties,
    pub compute_family: u32,
    pub compute_queue: vk::Queue,
    pub encode_family: u32,
    pub encode_queue: vk::Queue,
    #[cfg(target_os = "linux")]
    pub ext_mem_fd: ash::khr::external_memory_fd::Device,
    #[cfg(target_os = "linux")]
    pub ext_sem_fd: ash::khr::external_semaphore_fd::Device,
    /// Windows: the D3D11 side of the import (opens the driver's textures, copies them into
    /// textures Vulkan can import), on the same adapter.
    #[cfg(windows)]
    pub d3d: super::d3d::Bridge,
    /// Windows: `d3d`'s copy fence as a timeline semaphore, or `None` if this implementation
    /// cannot import it (the CPU then waits for the copies).
    #[cfg(windows)]
    pub copied: Option<vk::Semaphore>,
    pub video_queue: ash::khr::video_queue::DeviceFn,
    pub video_queue_instance: ash::khr::video_queue::InstanceFn,
    pub video_encode: ash::khr::video_encode_queue::DeviceFn,
    pub video_encode_instance: ash::khr::video_encode_queue::InstanceFn,
    pub name: String,
    /// `VK_KHR_video_encode_quantization_map` is enabled: the encoder can lower the quality of
    /// chosen regions (the edges of a foveated picture). Optional.
    pub quantization_map: bool,
    /// Set when a GPU wait timed out or the device was lost. Nothing may wait on the
    /// device after that (it could block forever): its objects are leaked instead.
    lost: std::cell::Cell<bool>,
}

/// Longest a single convert or encode may take before the GPU is presumed hung.
pub const GPU_TIMEOUT_NS: u64 = 1_000_000_000;

#[cfg(target_os = "linux")]
const DEVICE_EXTENSIONS: &[&CStr] = &[
    ash::khr::external_memory_fd::NAME,
    ash::ext::external_memory_dma_buf::NAME,
    ash::ext::image_drm_format_modifier::NAME,
    ash::ext::queue_family_foreign::NAME,
    ash::khr::external_semaphore_fd::NAME,
    ash::khr::video_queue::NAME,
    ash::khr::video_encode_queue::NAME,
    ash::khr::video_encode_h265::NAME,
];

#[cfg(windows)]
const DEVICE_EXTENSIONS: &[&CStr] = &[
    ash::khr::external_memory_win32::NAME,
    ash::khr::external_semaphore_win32::NAME,
    ash::khr::video_queue::NAME,
    ash::khr::video_encode_queue::NAME,
    ash::khr::video_encode_h265::NAME,
];

/// The GPU the driver's textures are on, for messages.
#[cfg(target_os = "linux")]
fn describe_gpu(render_node: u64) -> String {
    format!("render node {}:{}", libc::major(render_node), libc::minor(render_node))
}
#[cfg(windows)]
fn describe_gpu(luid: u64) -> String {
    format!("adapter LUID {luid:#x}")
}

/// Whether physical device `pdev` is the GPU `render_node` names (0: any GPU).
fn gpu_matches(instance: &ash::Instance, pdev: vk::PhysicalDevice, render_node: u64, has_drm: bool) -> bool {
    if render_node == 0 {
        return true;
    }
    #[cfg(target_os = "linux")]
    {
        let mut drm = vk::PhysicalDeviceDrmPropertiesEXT::default();
        let mut props = vk::PhysicalDeviceProperties2::default().push_next(&mut drm);
        if !has_drm {
            return false;
        }
        // SAFETY: the DRM struct is supported.
        unsafe { instance.get_physical_device_properties2(pdev, &mut props) };
        drm.has_render != 0
            && drm.render_major as u64 == libc::major(render_node) as u64
            && drm.render_minor as u64 == libc::minor(render_node) as u64
    }
    #[cfg(windows)]
    {
        let _ = has_drm;
        luid(instance, pdev) == Some(render_node)
    }
}

/// The DXGI adapter LUID of `pdev` (`HighPart << 32 | LowPart`, as the driver sends it).
#[cfg(windows)]
pub fn luid(instance: &ash::Instance, pdev: vk::PhysicalDevice) -> Option<u64> {
    let mut id = vk::PhysicalDeviceIDProperties::default();
    let mut props = vk::PhysicalDeviceProperties2::default().push_next(&mut id);
    // SAFETY: core 1.1 query.
    unsafe { instance.get_physical_device_properties2(pdev, &mut props) };
    // LUID is { LowPart: u32, HighPart: i32 } in memory, so little-endian bytes are exactly that.
    (id.device_luid_valid != 0).then(|| u64::from_le_bytes(id.device_luid))
}

impl Context {
    /// Opens the Vulkan device on the GPU `render_node` names (Linux: a DRM render node's
    /// `st_rdev`; Windows: a DXGI adapter LUID), or the first device with H.265 encode if
    /// `render_node` is 0.
    pub fn new(render_node: u64) -> Result<Self> {
        // SAFETY: loading the system Vulkan loader.
        let entry = unsafe { ash::Entry::load() }.map_err(|e| format!("cannot load Vulkan: {e}"))?;
        let app = vk::ApplicationInfo::default()
            .application_name(c"openlink")
            .api_version(vk::API_VERSION_1_3);
        // SAFETY: valid create info.
        let instance = check(
            unsafe { entry.create_instance(&vk::InstanceCreateInfo::default().application_info(&app), None) },
            "vkCreateInstance",
        )?;
        let video_queue_instance = ash::khr::video_queue::InstanceFn::load(|name| unsafe {
            std::mem::transmute(entry.get_instance_proc_addr(instance.handle(), name.as_ptr()))
        });
        let video_encode_instance = ash::khr::video_encode_queue::InstanceFn::load(|name| unsafe {
            std::mem::transmute(entry.get_instance_proc_addr(instance.handle(), name.as_ptr()))
        });

        // SAFETY: valid instance.
        let pdevs = check(unsafe { instance.enumerate_physical_devices() }, "vkEnumeratePhysicalDevices")?;
        let mut chosen = None;
        for pdev in pdevs {
            // SAFETY: valid physical device.
            let exts = unsafe { instance.enumerate_device_extension_properties(pdev) }.unwrap_or_default();
            let has = |n: &CStr| exts.iter().any(|e| e.extension_name_as_c_str() == Ok(n));
            // SAFETY: valid physical device.
            let props = unsafe { instance.get_physical_device_properties(pdev) };
            let name =
                props.device_name_as_c_str().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
            let missing: Vec<_> = DEVICE_EXTENSIONS.iter().filter(|n| !has(n)).collect();
            let node_matches = gpu_matches(&instance, pdev, render_node, has(ash::ext::physical_device_drm::NAME));
            log::debug!("Vulkan device {name}: GPU match {node_matches}, missing {missing:?}");
            // Without a GPU named, skip devices that cannot encode (e.g. an integrated GPU
            // listed first) rather than failing on them.
            if render_node == 0 && !missing.is_empty() {
                continue;
            }
            if node_matches {
                if !missing.is_empty() {
                    return Err(format!("{name} lacks {missing:?}"));
                }
                let mut qp_features = super::qp_map::Features {
                    s_type: super::qp_map::ST_FEATURES,
                    p_next: std::ptr::null_mut(),
                    video_encode_quantization_map: vk::FALSE,
                };
                if has(super::qp_map::NAME) {
                    let mut features =
                        vk::PhysicalDeviceFeatures2 { p_next: (&raw mut qp_features).cast(), ..Default::default() };
                    // SAFETY: the extension is supported, so its feature struct may be chained.
                    unsafe { instance.get_physical_device_features2(pdev, &mut features) };
                }
                chosen = Some((pdev, name, qp_features.video_encode_quantization_map == vk::TRUE));
                break;
            }
        }
        let (pdev, name, quantization_map) = chosen.ok_or_else(|| {
            if render_node == 0 {
                format!("no Vulkan device has {DEVICE_EXTENSIONS:?}")
            } else {
                format!("no Vulkan device on {}", describe_gpu(render_node))
            }
        })?;

        // Queue families: one with compute, one with H.265 encode.
        let n = unsafe { instance.get_physical_device_queue_family_properties2_len(pdev) };
        let mut video_props = vec![vk::QueueFamilyVideoPropertiesKHR::default(); n];
        let mut families: Vec<vk::QueueFamilyProperties2> = video_props
            .iter_mut()
            .map(|v| vk::QueueFamilyProperties2::default().push_next(v))
            .collect();
        // SAFETY: arrays sized from the _len call.
        unsafe { instance.get_physical_device_queue_family_properties2(pdev, &mut families) };
        let flags: Vec<vk::QueueFlags> = families.iter().map(|f| f.queue_family_properties.queue_flags).collect();
        drop(families);
        let compute_family = flags
            .iter()
            .position(|f| f.contains(vk::QueueFlags::COMPUTE) && !f.contains(vk::QueueFlags::GRAPHICS))
            .or_else(|| flags.iter().position(|f| f.contains(vk::QueueFlags::COMPUTE)))
            .ok_or("no compute queue")? as u32;
        let encode_family = flags
            .iter()
            .zip(&video_props)
            .position(|(f, v)| {
                f.contains(vk::QueueFlags::VIDEO_ENCODE_KHR)
                    && v.video_codec_operations.contains(vk::VideoCodecOperationFlagsKHR::ENCODE_H265)
            })
            .ok_or("no H.265 encode queue")? as u32;

        let prio = [1.0];
        let queues = [
            vk::DeviceQueueCreateInfo::default().queue_family_index(compute_family).queue_priorities(&prio),
            vk::DeviceQueueCreateInfo::default().queue_family_index(encode_family).queue_priorities(&prio),
        ];
        let mut ext_ptrs: Vec<_> = DEVICE_EXTENSIONS.iter().map(|e| e.as_ptr()).collect();
        let mut qp_features = super::qp_map::Features {
            s_type: super::qp_map::ST_FEATURES,
            p_next: std::ptr::null_mut(),
            video_encode_quantization_map: vk::TRUE,
        };
        // Windows: the D3D11 copy's fence is imported as a timeline semaphore.
        let mut v12 = vk::PhysicalDeviceVulkan12Features::default()
            .host_query_reset(true)
            .timeline_semaphore(cfg!(windows));
        let mut v13 = vk::PhysicalDeviceVulkan13Features::default().synchronization2(true);
        if quantization_map {
            ext_ptrs.push(super::qp_map::NAME.as_ptr());
            // Hand-written struct, so chained by hand: v13 -> qp_features.
            v13.p_next = (&raw mut qp_features).cast();
        }
        let info = vk::DeviceCreateInfo::default()
            .queue_create_infos(&queues)
            .enabled_extension_names(&ext_ptrs)
            .push_next(&mut v12)
            .push_next(&mut v13);
        // SAFETY: valid create info; extensions checked above.
        let device = check(unsafe { instance.create_device(pdev, &info, None) }, "vkCreateDevice")?;
        // SAFETY: families/indices requested above.
        let (compute_queue, encode_queue) =
            unsafe { (device.get_device_queue(compute_family, 0), device.get_device_queue(encode_family, 0)) };
        let load = |name: &CStr| unsafe {
            std::mem::transmute(instance.get_device_proc_addr(device.handle(), name.as_ptr()))
        };
        let video_queue = ash::khr::video_queue::DeviceFn::load(load);
        let video_encode = ash::khr::video_encode_queue::DeviceFn::load(load);
        #[cfg(target_os = "linux")]
        let ext_mem_fd = ash::khr::external_memory_fd::Device::new(&instance, &device);
        #[cfg(target_os = "linux")]
        let ext_sem_fd = ash::khr::external_semaphore_fd::Device::new(&instance, &device);
        #[cfg(windows)]
        let ext_sem_win32 = ash::khr::external_semaphore_win32::Device::new(&instance, &device);
        #[cfg(windows)]
        let d3d = {
            let luid = luid(&instance, pdev).ok_or("the Vulkan device has no LUID")?;
            match super::d3d::Bridge::new(luid) {
                Ok(b) => b,
                Err(e) => {
                    // SAFETY: nothing was created from the device yet.
                    unsafe {
                        device.destroy_device(None);
                        instance.destroy_instance(None);
                    }
                    return Err(e);
                }
            }
        };
        #[cfg(windows)]
        let copied = match import_fence(&device, &ext_sem_win32, &d3d) {
            Ok(s) => Some(s),
            Err(e) => {
                log::warn!("video: {e}; the CPU waits for each frame's copy instead");
                None
            }
        };
        // SAFETY: valid physical device.
        let mem_props = unsafe { instance.get_physical_device_memory_properties(pdev) };
        log::info!(
            "video: Vulkan device {name} (compute family {compute_family}, encode family {encode_family}, \
             quantization maps {quantization_map})"
        );
        Ok(Self {
            lost: std::cell::Cell::new(false),
            _entry: entry,
            instance,
            pdev,
            device,
            mem_props,
            compute_family,
            compute_queue,
            encode_family,
            encode_queue,
            #[cfg(target_os = "linux")]
            ext_mem_fd,
            #[cfg(target_os = "linux")]
            ext_sem_fd,
            #[cfg(windows)]
            d3d,
            #[cfg(windows)]
            copied,
            video_queue,
            video_queue_instance,
            video_encode,
            video_encode_instance,
            name,
            quantization_map,
        })
    }

    pub fn memory_type(&self, bits: u32, want: vk::MemoryPropertyFlags) -> Option<u32> {
        (0..self.mem_props.memory_type_count).find(|&i| {
            bits & (1 << i) != 0 && self.mem_props.memory_types[i as usize].property_flags.contains(want)
        })
    }

    /// Allocates and binds memory for `image`.
    pub fn bind_image_memory(&self, image: vk::Image, want: vk::MemoryPropertyFlags) -> Result<vk::DeviceMemory> {
        // SAFETY: valid image.
        let req = unsafe { self.device.get_image_memory_requirements(image) };
        let ty = self.memory_type(req.memory_type_bits, want).ok_or("no memory type for image")?;
        let info = vk::MemoryAllocateInfo::default().allocation_size(req.size).memory_type_index(ty);
        // SAFETY: valid allocation info; image unbound.
        unsafe {
            let mem = check(self.device.allocate_memory(&info, None), "vkAllocateMemory")?;
            check(self.device.bind_image_memory(image, mem, 0), "vkBindImageMemory")?;
            Ok(mem)
        }
    }

    pub fn bind_buffer_memory(&self, buffer: vk::Buffer, want: vk::MemoryPropertyFlags) -> Result<vk::DeviceMemory> {
        // SAFETY: valid buffer.
        let req = unsafe { self.device.get_buffer_memory_requirements(buffer) };
        let ty = self.memory_type(req.memory_type_bits, want).ok_or("no memory type for buffer")?;
        let info = vk::MemoryAllocateInfo::default().allocation_size(req.size).memory_type_index(ty);
        // SAFETY: as above.
        unsafe {
            let mem = check(self.device.allocate_memory(&info, None), "vkAllocateMemory")?;
            check(self.device.bind_buffer_memory(buffer, mem, 0), "vkBindBufferMemory")?;
            Ok(mem)
        }
    }

    pub fn command_pool(&self, family: u32) -> Result<vk::CommandPool> {
        let info = vk::CommandPoolCreateInfo::default()
            .queue_family_index(family)
            .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER);
        // SAFETY: valid create info.
        check(unsafe { self.device.create_command_pool(&info, None) }, "vkCreateCommandPool")
    }

    pub fn command_buffer(&self, pool: vk::CommandPool) -> Result<vk::CommandBuffer> {
        let info = vk::CommandBufferAllocateInfo::default()
            .command_pool(pool)
            .level(vk::CommandBufferLevel::PRIMARY)
            .command_buffer_count(1);
        // SAFETY: valid pool.
        Ok(check(unsafe { self.device.allocate_command_buffers(&info) }, "vkAllocateCommandBuffers")?[0])
    }

    pub fn fence(&self) -> Result<vk::Fence> {
        // SAFETY: trivial create info.
        check(unsafe { self.device.create_fence(&vk::FenceCreateInfo::default(), None) }, "vkCreateFence")
    }

    /// Waits for `fence` (bounded), then resets it. A timeout or device loss marks the
    /// context lost.
    pub fn wait_fence(&self, fence: vk::Fence, what: &str) -> Result<()> {
        // SAFETY: valid fence owned by the caller.
        match unsafe { self.device.wait_for_fences(&[fence], true, GPU_TIMEOUT_NS) } {
            Ok(()) => check(unsafe { self.device.reset_fences(&[fence]) }, "vkResetFences"),
            Err(e) => {
                if matches!(e, vk::Result::TIMEOUT | vk::Result::ERROR_DEVICE_LOST) {
                    self.lost.set(true);
                }
                Err(format!("{what}: {e:?}"))
            }
        }
    }

    /// A wait elsewhere (an encode finished on another thread) timed out or lost the device.
    pub fn mark_lost(&self) {
        self.lost.set(true);
    }

    pub fn is_lost(&self) -> bool {
        self.lost.get()
    }

    /// `vkDeviceWaitIdle`, skipped once the device is lost.
    pub fn wait_idle(&self) {
        if !self.lost.get() {
            // SAFETY: valid device.
            unsafe {
                let _ = self.device.device_wait_idle();
            }
        }
    }

    pub fn semaphore(&self) -> Result<vk::Semaphore> {
        // SAFETY: trivial create info.
        check(
            unsafe { self.device.create_semaphore(&vk::SemaphoreCreateInfo::default(), None) },
            "vkCreateSemaphore",
        )
    }
}

/// Imports the D3D11 copy fence as a timeline semaphore (a D3D11 fence is a D3D12 fence
/// underneath, which is the handle type Vulkan names).
#[cfg(windows)]
fn import_fence(
    device: &ash::Device,
    ext: &ash::khr::external_semaphore_win32::Device,
    d3d: &super::d3d::Bridge,
) -> Result<vk::Semaphore> {
    use std::os::windows::io::AsRawHandle;
    let handle = d3d.fence_handle()?;
    let mut ty = vk::SemaphoreTypeCreateInfo::default().semaphore_type(vk::SemaphoreType::TIMELINE).initial_value(0);
    // SAFETY: valid create info.
    let sem = check(
        unsafe { device.create_semaphore(&vk::SemaphoreCreateInfo::default().push_next(&mut ty), None) },
        "vkCreateSemaphore (timeline)",
    )?;
    let info = vk::ImportSemaphoreWin32HandleInfoKHR::default()
        .semaphore(sem)
        .handle_type(vk::ExternalSemaphoreHandleTypeFlags::D3D12_FENCE)
        .handle(handle.as_raw_handle() as _);
    // SAFETY: valid semaphore and NT handle; Vulkan does not take ownership of the handle.
    match unsafe { ext.import_semaphore_win32_handle(&info) } {
        Ok(()) => Ok(sem),
        Err(e) => {
            // SAFETY: unused semaphore.
            unsafe { device.destroy_semaphore(sem, None) };
            Err(format!("cannot import the D3D11 copy fence: {e:?}"))
        }
    }
}

impl Drop for Context {
    fn drop(&mut self) {
        if self.lost.get() {
            // Destroying a device with work still queued is undefined; leak it instead.
            log::warn!("video: leaking the lost Vulkan device");
            return;
        }
        // SAFETY: every object created from the device is destroyed by its owner first
        // (they borrow the context).
        unsafe {
            let _ = self.device.device_wait_idle();
            #[cfg(windows)]
            if let Some(s) = self.copied {
                self.device.destroy_semaphore(s, None);
            }
            self.device.destroy_device(None);
            self.instance.destroy_instance(None);
        }
    }
}
