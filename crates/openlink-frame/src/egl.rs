//! EGL: a desktop OpenGL context for the OpenXR session (through `XR_MNDX_egl_enable`), and
//! dmabuf import, which turns the decoder's picture buffers into GL textures without a copy.
//!
//! `libEGL.so.1` is loaded at run time, so the binary has no link-time GL dependency. On the
//! Steam Frame, Mesa runs OpenGL on Zink over Turnip (Vulkan on the Adreno 750).

use khronos_egl as egl;
use std::ffi::{c_char, c_void, CStr};
use std::os::fd::{AsRawFd, BorrowedFd};
use std::sync::OnceLock;

pub type Egl = egl::DynamicInstance<egl::EGL1_5>;

const PLATFORM_SURFACELESS_MESA: egl::Enum = 0x31DD;
const LINUX_DMA_BUF_EXT: egl::Enum = 0x3270;
const LINUX_DRM_FOURCC_EXT: egl::Int = 0x3271;
const DMA_BUF_PLANE0_FD_EXT: egl::Int = 0x3272;
const DMA_BUF_PLANE0_OFFSET_EXT: egl::Int = 0x3273;
const DMA_BUF_PLANE0_PITCH_EXT: egl::Int = 0x3274;
const DMA_BUF_PLANE0_MODIFIER_LO_EXT: egl::Int = 0x3443;
const DMA_BUF_PLANE0_MODIFIER_HI_EXT: egl::Int = 0x3444;

/// `DRM_FORMAT_R8`: one 8-bit channel (the luma plane).
pub const DRM_FORMAT_R8: u32 = u32::from_le_bytes(*b"R8  ");
/// `DRM_FORMAT_GR88`: two 8-bit channels, R in the first byte (interleaved CbCr).
pub const DRM_FORMAT_GR88: u32 = u32::from_le_bytes(*b"GR88");

type CreateImageKhr =
    unsafe extern "C" fn(egl::EGLDisplay, egl::EGLContext, egl::Enum, *mut c_void, *const egl::Int) -> *mut c_void;
type DestroyImageKhr = unsafe extern "C" fn(egl::EGLDisplay, *mut c_void) -> egl::Boolean;
type ImageTargetTexture2dOes = unsafe extern "C" fn(target: u32, image: *mut c_void);

/// Loaded once: OpenXR needs a plain function to look up GL entry points.
static EGL: OnceLock<Egl> = OnceLock::new();

/// `eglGetProcAddress`, in the form `XrGraphicsBindingEGLMNDX` takes.
pub unsafe extern "system" fn get_proc_address(name: *const c_char) -> Option<unsafe extern "system" fn()> {
    let egl = EGL.get()?;
    // SAFETY: the runtime passes a NUL-terminated name.
    let name = unsafe { CStr::from_ptr(name) }.to_str().ok()?;
    egl.get_proc_address(name).map(|f| f as unsafe extern "system" fn())
}

pub struct Context {
    pub egl: &'static Egl,
    pub display: egl::Display,
    pub config: egl::Config,
    pub context: egl::Context,
    surface: Option<egl::Surface>,
    /// Whether dmabufs can be imported (`EGL_EXT_image_dma_buf_import`), and with an explicit
    /// (linear) modifier.
    pub dmabuf: bool,
    modifiers: bool,
    create_image: Option<CreateImageKhr>,
    destroy_image: Option<DestroyImageKhr>,
    image_target_texture: Option<ImageTargetTexture2dOes>,
}

/// An `EGLImage` over one plane of a dmabuf.
pub struct Image(*mut c_void);

impl Context {
    /// A desktop GL context of at least `version` (what the OpenXR runtime asks for), current on
    /// this thread.
    pub fn new(version: (u16, u16)) -> Result<Self, String> {
        let egl = match EGL.get() {
            Some(e) => e,
            None => {
                // SAFETY: loading the system's EGL library.
                let e = unsafe { Egl::load_required() }.map_err(|e| format!("cannot load libEGL: {e}"))?;
                EGL.get_or_init(|| e)
            }
        };
        let client = egl.query_string(None, egl::EXTENSIONS).map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        let display = open_display(egl, &client)?;
        let display_exts = egl
            .query_string(Some(display), egl::EXTENSIONS)
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let has = |e: &str| display_exts.split_ascii_whitespace().any(|x| x == e);
        egl.bind_api(egl::OPENGL_API).map_err(|e| format!("eglBindAPI(OpenGL): {e}"))?;

        let config = [egl::PBUFFER_BIT, 0]
            .into_iter()
            .find_map(|surface_type| {
                let attribs = [
                    egl::RED_SIZE, 8,
                    egl::GREEN_SIZE, 8,
                    egl::BLUE_SIZE, 8,
                    egl::ALPHA_SIZE, 8,
                    egl::RENDERABLE_TYPE, egl::OPENGL_BIT,
                    egl::SURFACE_TYPE, surface_type,
                    egl::NONE,
                ];
                egl.choose_first_config(display, &attribs).ok().flatten()
            })
            .ok_or("no RGBA8 desktop OpenGL EGL config")?;
        let (major, minor) = (version.0.max(3), if version.0 > 3 { version.1 } else { version.1.max(3) });
        let ctx_attribs = [
            egl::CONTEXT_MAJOR_VERSION, major as egl::Int,
            egl::CONTEXT_MINOR_VERSION, minor as egl::Int,
            egl::CONTEXT_OPENGL_PROFILE_MASK, egl::CONTEXT_OPENGL_CORE_PROFILE_BIT,
            egl::NONE,
        ];
        let context = egl
            .create_context(display, config, None, &ctx_attribs)
            .map_err(|e| format!("eglCreateContext (OpenGL {major}.{minor} core): {e}"))?;
        // OpenXR renders into its own swapchains; no surface is needed if the display allows it.
        let surface = if has("EGL_KHR_surfaceless_context") {
            None
        } else {
            Some(
                egl.create_pbuffer_surface(display, config, &[egl::WIDTH, 16, egl::HEIGHT, 16, egl::NONE])
                    .map_err(|e| format!("eglCreatePbufferSurface: {e}"))?,
            )
        };
        egl.make_current(display, surface, surface, Some(context))
            .map_err(|e| format!("eglMakeCurrent: {e}"))?;

        let dmabuf = has("EGL_EXT_image_dma_buf_import");
        // SAFETY: the signatures match the EGL/GL extension specifications.
        let (create_image, destroy_image, image_target_texture) = unsafe {
            (
                egl.get_proc_address("eglCreateImageKHR").map(|f| std::mem::transmute::<extern "system" fn(), CreateImageKhr>(f)),
                egl.get_proc_address("eglDestroyImageKHR").map(|f| std::mem::transmute::<extern "system" fn(), DestroyImageKhr>(f)),
                egl.get_proc_address("glEGLImageTargetTexture2DOES")
                    .map(|f| std::mem::transmute::<extern "system" fn(), ImageTargetTexture2dOes>(f)),
            )
        };
        let dmabuf = dmabuf && create_image.is_some() && destroy_image.is_some() && image_target_texture.is_some();
        log::info!(
            "EGL {}: OpenGL {major}.{minor} core, {}, dmabuf import {}",
            egl.query_string(Some(display), egl::VENDOR).map(|s| s.to_string_lossy().into_owned()).unwrap_or_default(),
            if surface.is_none() { "surfaceless" } else { "pbuffer" },
            if dmabuf { "available" } else { "unavailable (pictures are uploaded)" }
        );
        Ok(Self {
            egl,
            display,
            config,
            context,
            surface,
            dmabuf,
            modifiers: has("EGL_EXT_image_dma_buf_import_modifiers"),
            create_image,
            destroy_image,
            image_target_texture,
        })
    }

    pub fn gl(&self) -> glow::Context {
        // SAFETY: the context is current on this thread.
        unsafe {
            glow::Context::from_loader_function(|name| {
                self.egl.get_proc_address(name).map_or(std::ptr::null(), |f| f as *const c_void)
            })
        }
    }

    /// Wraps one linear plane of a dmabuf. The descriptor may be closed afterwards.
    pub fn import_plane(
        &self,
        fd: BorrowedFd,
        fourcc: u32,
        (width, height): (u32, u32),
        offset: usize,
        pitch: usize,
    ) -> Result<Image, String> {
        let (Some(create), true) = (self.create_image, self.dmabuf) else {
            return Err("dmabuf import is unavailable".into());
        };
        let mut attribs = vec![
            egl::WIDTH, width as egl::Int,
            egl::HEIGHT, height as egl::Int,
            LINUX_DRM_FOURCC_EXT, fourcc as egl::Int,
            DMA_BUF_PLANE0_FD_EXT, fd.as_raw_fd(),
            DMA_BUF_PLANE0_OFFSET_EXT, offset as egl::Int,
            DMA_BUF_PLANE0_PITCH_EXT, pitch as egl::Int,
        ];
        if self.modifiers {
            // DRM_FORMAT_MOD_LINEAR: V4L2's NV12 is linear by definition.
            attribs.extend([DMA_BUF_PLANE0_MODIFIER_LO_EXT, 0, DMA_BUF_PLANE0_MODIFIER_HI_EXT, 0]);
        }
        attribs.push(egl::NONE);
        // SAFETY: valid display and attribute list; EGL_NO_CONTEXT as dmabuf import requires.
        let image = unsafe {
            create(self.display.as_ptr(), std::ptr::null_mut(), LINUX_DMA_BUF_EXT, std::ptr::null_mut(), attribs.as_ptr())
        };
        if image.is_null() {
            return Err(format!("eglCreateImageKHR (dmabuf): {:?}", self.egl.get_error()));
        }
        Ok(Image(image))
    }

    /// Points the bound `GL_TEXTURE_2D` texture at `image`.
    pub fn bind_texture(&self, image: &Image) {
        const GL_TEXTURE_2D: u32 = 0x0DE1;
        if let Some(f) = self.image_target_texture {
            // SAFETY: valid image; a texture is bound to GL_TEXTURE_2D.
            unsafe { f(GL_TEXTURE_2D, image.0) }
        }
    }

    pub fn destroy(&self, image: Image) {
        if let Some(f) = self.destroy_image {
            // SAFETY: an image we created on this display.
            unsafe { f(self.display.as_ptr(), image.0) };
        }
    }
}

impl Drop for Context {
    fn drop(&mut self) {
        let _ = self.egl.make_current(self.display, None, None, None);
        if let Some(s) = self.surface {
            let _ = self.egl.destroy_surface(self.display, s);
        }
        let _ = self.egl.destroy_context(self.display, self.context);
        let _ = self.egl.terminate(self.display);
    }
}

/// The platform display (X11 or Wayland, whatever the environment offers), else Mesa's
/// surfaceless one. `OPENLINK_EGL_SURFACELESS=1` goes straight to surfaceless.
fn open_display(egl: &Egl, client_exts: &str) -> Result<egl::Display, String> {
    let surfaceless_ok = client_exts.split_ascii_whitespace().any(|e| e == "EGL_MESA_platform_surfaceless");
    let force = std::env::var("OPENLINK_EGL_SURFACELESS").as_deref() == Ok("1");
    if !force {
        // SAFETY: the default display.
        if let Some(d) = unsafe { egl.get_display(egl::DEFAULT_DISPLAY) } {
            match egl.initialize(d) {
                Ok(_) => return Ok(d),
                Err(e) => log::info!("default EGL display unavailable ({e}); trying surfaceless"),
            }
        }
    }
    if !surfaceless_ok {
        return Err("no usable EGL display".into());
    }
    // SAFETY: Mesa's surfaceless platform takes no native display.
    let d = unsafe { egl.get_platform_display(PLATFORM_SURFACELESS_MESA, egl::DEFAULT_DISPLAY, &[egl::ATTRIB_NONE]) }
        .map_err(|e| format!("eglGetPlatformDisplay(surfaceless): {e}"))?;
    egl.initialize(d).map_err(|e| format!("eglInitialize(surfaceless): {e}"))?;
    Ok(d)
}
