//! Minimal GBM binding, loaded at runtime so a missing `libgbm` only disables dmabuf export
//! instead of stopping the driver from loading.

use std::ffi::{c_char, c_int, c_void, CStr};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

const GBM_BO_USE_RENDERING: u32 = 1 << 2;

#[allow(non_snake_case)]
struct Api {
    create_device: unsafe extern "C" fn(c_int) -> *mut c_void,
    create_with_modifiers2:
        unsafe extern "C" fn(*mut c_void, u32, u32, u32, *const u64, u32, u32) -> *mut c_void,
    bo_destroy: unsafe extern "C" fn(*mut c_void),
    bo_get_plane_count: unsafe extern "C" fn(*mut c_void) -> c_int,
    bo_get_fd_for_plane: unsafe extern "C" fn(*mut c_void, c_int) -> c_int,
    bo_get_stride_for_plane: unsafe extern "C" fn(*mut c_void, c_int) -> u32,
    bo_get_offset: unsafe extern "C" fn(*mut c_void, c_int) -> u32,
    bo_get_modifier: unsafe extern "C" fn(*mut c_void) -> u64,
}

/// A GBM device on one DRM render node.
pub struct Device {
    api: Api,
    dev: *mut c_void,
    /// Keeps the render node open for the device's lifetime.
    node: OwnedFd,
}

// SAFETY: GBM devices may be used from any thread as long as calls are not concurrent; all
// use goes through the driver's compositor mutex.
unsafe impl Send for Device {}

pub struct Plane {
    pub fd: OwnedFd,
    pub offset: u32,
    pub stride: u32,
}

/// A buffer object, destroyed on drop.
pub struct Bo {
    destroy: unsafe extern "C" fn(*mut c_void),
    bo: *mut c_void,
    pub modifier: u64,
    pub planes: Vec<Plane>,
}

// SAFETY: see `Device`.
unsafe impl Send for Bo {}

impl Drop for Bo {
    fn drop(&mut self) {
        // SAFETY: we own the bo.
        unsafe { (self.destroy)(self.bo) }
    }
}

unsafe fn sym<T: Copy>(lib: *mut c_void, name: &CStr) -> Result<T, String> {
    let p = libc::dlsym(lib, name.as_ptr());
    if p.is_null() {
        Err(format!("libgbm has no {}", name.to_string_lossy()))
    } else {
        Ok(std::mem::transmute_copy(&p))
    }
}

impl Device {
    /// Opens `path` (a render node such as `/dev/dri/renderD128`).
    pub fn open(path: &CStr) -> Result<Self, String> {
        // SAFETY: dlopen/dlsym with valid strings; the library is never unloaded.
        let api = unsafe {
            let lib = libc::dlopen(c"libgbm.so.1".as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL);
            if lib.is_null() {
                let err = libc::dlerror();
                let msg = if err.is_null() {
                    "unknown error".into()
                } else {
                    CStr::from_ptr(err as *const c_char).to_string_lossy().into_owned()
                };
                return Err(format!("cannot load libgbm.so.1: {msg}"));
            }
            Api {
                create_device: sym(lib, c"gbm_create_device")?,
                create_with_modifiers2: sym(lib, c"gbm_bo_create_with_modifiers2")?,
                bo_destroy: sym(lib, c"gbm_bo_destroy")?,
                bo_get_plane_count: sym(lib, c"gbm_bo_get_plane_count")?,
                bo_get_fd_for_plane: sym(lib, c"gbm_bo_get_fd_for_plane")?,
                bo_get_stride_for_plane: sym(lib, c"gbm_bo_get_stride_for_plane")?,
                bo_get_offset: sym(lib, c"gbm_bo_get_offset")?,
                bo_get_modifier: sym(lib, c"gbm_bo_get_modifier")?,
            }
        };
        // SAFETY: plain open(2).
        let fd = unsafe { libc::open(path.as_ptr(), libc::O_RDWR | libc::O_CLOEXEC) };
        if fd < 0 {
            return Err(format!(
                "cannot open {}: {}",
                path.to_string_lossy(),
                std::io::Error::last_os_error()
            ));
        }
        // SAFETY: fresh fd we own.
        let node = unsafe { OwnedFd::from_raw_fd(fd) };
        // SAFETY: valid fd; GBM does not take ownership.
        let dev = unsafe { (api.create_device)(node.as_raw_fd()) };
        if dev.is_null() {
            return Err("gbm_create_device failed".into());
        }
        Ok(Self { api, dev, node })
    }

    /// `st_rdev` of the render node, so the server can open the same GPU.
    pub fn rdev(&self) -> u64 {
        // SAFETY: stat is plain data; valid fd.
        unsafe {
            let mut st: libc::stat = std::mem::zeroed();
            if libc::fstat(self.node.as_raw_fd(), &mut st) == 0 {
                st.st_rdev
            } else {
                0
            }
        }
    }

    /// Allocates a renderable buffer with one of `modifiers` (GBM picks the best).
    pub fn create(&self, w: u32, h: u32, drm_format: u32, modifiers: &[u64]) -> Option<Bo> {
        // SAFETY: valid device and modifier array.
        let bo = unsafe {
            (self.api.create_with_modifiers2)(
                self.dev,
                w,
                h,
                drm_format,
                modifiers.as_ptr(),
                modifiers.len() as u32,
                GBM_BO_USE_RENDERING,
            )
        };
        if bo.is_null() {
            return None;
        }
        let mut out = Bo {
            destroy: self.api.bo_destroy,
            bo,
            modifier: 0,
            planes: Vec::new(),
        };
        // SAFETY: valid bo for all calls below.
        unsafe {
            out.modifier = (self.api.bo_get_modifier)(bo);
            for p in 0..(self.api.bo_get_plane_count)(bo) {
                let fd = (self.api.bo_get_fd_for_plane)(bo, p);
                if fd < 0 {
                    return None;
                }
                out.planes.push(Plane {
                    fd: OwnedFd::from_raw_fd(fd),
                    offset: (self.api.bo_get_offset)(bo, p),
                    stride: (self.api.bo_get_stride_for_plane)(bo, p),
                });
            }
        }
        Some(out)
    }
}
