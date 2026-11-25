//! EGL: an OpenGL ES 3 context (made current on the render thread) for the OpenXR session,
//! plus the extensions that turn decoder output (`AHardwareBuffer`s) into GL textures.

use khronos_egl as egl;
use std::ffi::c_void;

pub type Egl = egl::Instance<egl::Static>;

const EGL_NATIVE_BUFFER_ANDROID: egl::Enum = 0x3140;
const EGL_IMAGE_PRESERVED_KHR: egl::Int = 0x30D2;

type GetNativeClientBufferAndroid = unsafe extern "C" fn(buffer: *const c_void) -> *mut c_void;
type CreateImageKhr =
    unsafe extern "C" fn(egl::EGLDisplay, egl::EGLContext, egl::Enum, *mut c_void, *const egl::Int) -> *mut c_void;
type DestroyImageKhr = unsafe extern "C" fn(egl::EGLDisplay, *mut c_void) -> egl::Boolean;
type ImageTargetTexture2dOes = unsafe extern "C" fn(target: u32, image: *mut c_void);

pub struct Context {
    pub egl: Egl,
    pub display: egl::Display,
    pub config: egl::Config,
    pub context: egl::Context,
    surface: egl::Surface,
    get_native_client_buffer: GetNativeClientBufferAndroid,
    create_image: CreateImageKhr,
    destroy_image: DestroyImageKhr,
    image_target_texture: ImageTargetTexture2dOes,
}

/// An `EGLImage` wrapping one decoded picture.
pub struct Image(*mut c_void);

impl Context {
    pub fn new() -> Result<Self, String> {
        let egl = Egl::new(egl::Static);
        let display = unsafe { egl.get_display(egl::DEFAULT_DISPLAY) }.ok_or("no EGL display")?;
        egl.initialize(display).map_err(|e| format!("eglInitialize: {e}"))?;
        let attribs = [
            egl::RED_SIZE, 8,
            egl::GREEN_SIZE, 8,
            egl::BLUE_SIZE, 8,
            egl::ALPHA_SIZE, 8,
            egl::DEPTH_SIZE, 0,
            egl::RENDERABLE_TYPE, egl::OPENGL_ES3_BIT,
            egl::SURFACE_TYPE, egl::PBUFFER_BIT,
            egl::NONE,
        ];
        let config = egl
            .choose_first_config(display, &attribs)
            .map_err(|e| format!("eglChooseConfig: {e}"))?
            .ok_or("no RGBA8 ES3 EGL config")?;
        let ctx_attribs = [egl::CONTEXT_MAJOR_VERSION, 3, egl::CONTEXT_MINOR_VERSION, 2, egl::NONE];
        let context = egl
            .create_context(display, config, None, &ctx_attribs)
            .map_err(|e| format!("eglCreateContext: {e}"))?;
        // OpenXR renders into its swapchains; this tiny surface only lets us make the context current.
        let surface = egl
            .create_pbuffer_surface(display, config, &[egl::WIDTH, 16, egl::HEIGHT, 16, egl::NONE])
            .map_err(|e| format!("eglCreatePbufferSurface: {e}"))?;
        egl.make_current(display, Some(surface), Some(surface), Some(context))
            .map_err(|e| format!("eglMakeCurrent: {e}"))?;

        let proc = |name: &str| {
            egl.get_proc_address(name).ok_or_else(|| format!("{name} is not available"))
        };
        // SAFETY: the signatures match the EGL/GLES extension specifications.
        unsafe {
            Ok(Self {
                get_native_client_buffer: std::mem::transmute::<extern "system" fn(), GetNativeClientBufferAndroid>(
                    proc("eglGetNativeClientBufferANDROID")?,
                ),
                create_image: std::mem::transmute::<extern "system" fn(), CreateImageKhr>(proc("eglCreateImageKHR")?),
                destroy_image: std::mem::transmute::<extern "system" fn(), DestroyImageKhr>(proc("eglDestroyImageKHR")?),
                image_target_texture: std::mem::transmute::<extern "system" fn(), ImageTargetTexture2dOes>(
                    proc("glEGLImageTargetTexture2DOES")?,
                ),
                egl,
                display,
                config,
                context,
                surface,
            })
        }
    }

    pub fn gl(&self) -> glow::Context {
        // SAFETY: the context is current on this thread.
        unsafe {
            glow::Context::from_loader_function(|name| {
                self.egl.get_proc_address(name).map_or(std::ptr::null(), |f| f as *const c_void)
            })
        }
    }

    /// Wraps a hardware buffer (kept alive by the caller while the image exists).
    pub fn import(&self, buffer: *const c_void) -> Result<Image, String> {
        let attribs = [EGL_IMAGE_PRESERVED_KHR, egl::TRUE as egl::Int, egl::NONE];
        // SAFETY: valid display and hardware buffer.
        let image = unsafe {
            let client = (self.get_native_client_buffer)(buffer);
            (self.create_image)(
                self.display.as_ptr(),
                std::ptr::null_mut(),
                EGL_NATIVE_BUFFER_ANDROID,
                client,
                attribs.as_ptr(),
            )
        };
        if image.is_null() {
            return Err(format!("eglCreateImageKHR failed: {:?}", self.egl.get_error()));
        }
        Ok(Image(image))
    }

    /// Points the bound `GL_TEXTURE_EXTERNAL_OES` texture at `image`.
    pub fn bind_external(&self, image: &Image) {
        const GL_TEXTURE_EXTERNAL_OES: u32 = 0x8D65;
        // SAFETY: valid image; a texture is bound to the external target.
        unsafe { (self.image_target_texture)(GL_TEXTURE_EXTERNAL_OES, image.0) }
    }

    pub fn destroy(&self, image: Image) {
        // SAFETY: an image we created on this display.
        unsafe {
            (self.destroy_image)(self.display.as_ptr(), image.0);
        }
    }
}

impl Drop for Context {
    fn drop(&mut self) {
        let _ = self.egl.make_current(self.display, None, None, None);
        let _ = self.egl.destroy_surface(self.display, self.surface);
        let _ = self.egl.destroy_context(self.display, self.context);
        let _ = self.egl.terminate(self.display);
    }
}
