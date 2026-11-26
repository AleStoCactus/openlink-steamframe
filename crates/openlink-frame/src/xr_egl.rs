//! OpenGL over EGL as an `openxr` graphics API: `XR_KHR_opengl_enable` swapchains with the
//! `XR_MNDX_egl_enable` session binding. The `openxr` crate only offers GLX and Wayland
//! bindings for desktop GL; SteamVR's arm64 runtime on the Steam Frame accepts EGL.

use openxr as xr;
use std::ffi::c_void;
use std::ptr;
use xr::sys;
use xr::sys::Handle as _;

pub enum EglGl {}

pub struct SessionCreateInfo {
    pub get_proc_address: sys::platform::EglGetProcAddressMNDX,
    pub display: *mut c_void,
    pub config: *mut c_void,
    pub context: *mut c_void,
}

fn cvt(r: sys::Result) -> xr::Result<sys::Result> {
    if r.into_raw() >= 0 {
        Ok(r)
    } else {
        Err(r)
    }
}

impl xr::Graphics for EglGl {
    type Requirements = xr::opengl::Requirements;
    type SessionCreateInfo = SessionCreateInfo;
    type Format = u32;
    type SwapchainImage = u32;

    fn raise_format(x: i64) -> u32 {
        x as _
    }

    fn lower_format(x: u32) -> i64 {
        x.into()
    }

    fn requirements(instance: &xr::Instance, system: xr::SystemId) -> xr::Result<Self::Requirements> {
        let ext = instance.exts().khr_opengl_enable.ok_or(sys::Result::ERROR_EXTENSION_NOT_PRESENT)?;
        let mut out = sys::GraphicsRequirementsOpenGLKHR::out(ptr::null_mut());
        // SAFETY: valid instance, system and out struct (type and next set by `out`).
        let out = unsafe {
            cvt((ext.get_open_gl_graphics_requirements)(instance.as_raw(), system, out.as_mut_ptr()))?;
            out.assume_init()
        };
        Ok(xr::opengl::Requirements {
            min_api_version_supported: out.min_api_version_supported,
            max_api_version_supported: out.max_api_version_supported,
        })
    }

    unsafe fn create_session(
        instance: &xr::Instance,
        system: xr::SystemId,
        info: &Self::SessionCreateInfo,
    ) -> xr::Result<sys::Session> {
        let binding = sys::GraphicsBindingEGLMNDX {
            ty: sys::GraphicsBindingEGLMNDX::TYPE,
            next: ptr::null(),
            get_proc_address: Some(info.get_proc_address),
            display: info.display,
            config: info.config,
            context: info.context,
        };
        let create = sys::SessionCreateInfo {
            ty: sys::SessionCreateInfo::TYPE,
            next: &binding as *const _ as *const _,
            create_flags: Default::default(),
            system_id: system,
        };
        let mut out = sys::Session::NULL;
        // SAFETY: per the caller (a current context matching the binding); valid structs.
        unsafe { cvt((instance.fp().create_session)(instance.as_raw(), &create, &mut out))? };
        Ok(out)
    }

    fn enumerate_swapchain_images(swapchain: &xr::Swapchain<Self>) -> xr::Result<Vec<u32>> {
        let fp = swapchain.instance().fp();
        let mut count = 0u32;
        // SAFETY: a size query.
        unsafe { cvt((fp.enumerate_swapchain_images)(swapchain.as_raw(), 0, &mut count, ptr::null_mut()))? };
        let blank = sys::SwapchainImageOpenGLKHR { ty: sys::SwapchainImageOpenGLKHR::TYPE, next: ptr::null_mut(), image: 0 };
        let mut images = vec![blank; count as usize];
        // SAFETY: `images` holds `count` initialised structs of the type GL swapchains use.
        unsafe {
            cvt((fp.enumerate_swapchain_images)(
                swapchain.as_raw(),
                count,
                &mut count,
                images.as_mut_ptr() as *mut sys::SwapchainImageBaseHeader,
            ))?
        };
        images.truncate(count as usize);
        Ok(images.into_iter().map(|i| i.image).collect())
    }
}
