//! Safe-ish wrappers around the SteamVR host interfaces.

use crate::ovr::*;
use std::ffi::{c_void, CStr, CString};
use std::sync::OnceLock;

/// Host interfaces fetched once in `Init`. SteamVR documents them as callable from any thread.
pub struct Host {
    server: *mut Iface<IVRServerDriverHost_Vtbl>,
    props: *mut Iface<IVRProperties_Vtbl>,
    input: *mut Iface<IVRDriverInput_Vtbl>,
    log: *mut Iface<IVRDriverLog_Vtbl>,
}

// SAFETY: the pointers are owned by vrserver, outlive the driver, and are thread-safe.
unsafe impl Send for Host {}
unsafe impl Sync for Host {}

static HOST: OnceLock<Host> = OnceLock::new();

pub fn host() -> &'static Host {
    HOST.get().expect("driver used before Init")
}

macro_rules! call {
    ($iface:expr, $method:ident $(, $arg:expr)* $(,)?) => {{
        let p = $iface;
        ((*(*p).vtable).$method)(p $(, $arg)*)
    }};
}

impl Host {
    /// # Safety
    /// `ctx` must be the `IVRDriverContext` SteamVR passed to `Init`.
    pub unsafe fn init(
        ctx: *mut Iface<IVRDriverContext_Vtbl>,
    ) -> Result<&'static Host, EVRInitError> {
        unsafe fn get<V>(
            ctx: *mut Iface<IVRDriverContext_Vtbl>,
            name: &[u8],
        ) -> Result<*mut Iface<V>, EVRInitError> {
            let mut err = VRInitError_None;
            let p = call!(ctx, GetGenericInterface, name.as_ptr().cast(), &mut err);
            if p.is_null() || err != VRInitError_None {
                Err(if err == VRInitError_None {
                    VRInitError_Init_InterfaceNotFound
                } else {
                    err
                })
            } else {
                Ok(p.cast())
            }
        }
        let host = Host {
            server: get(ctx, IVRServerDriverHost_Version)?,
            props: get(ctx, IVRProperties_Version)?,
            input: get(ctx, IVRDriverInput_Version)?,
            log: get(ctx, IVRDriverLog_Version)?,
        };
        Ok(HOST.get_or_init(|| host))
    }

    pub fn log(&self, msg: &str) {
        let line = CString::new(format!("{msg}\n").replace('\0', "")).unwrap();
        // SAFETY: valid interface, NUL-terminated string.
        unsafe { call!(self.log, Log, line.as_ptr()) }
    }

    /// # Safety
    /// `driver` must point at an object laid out as an `ITrackedDeviceServerDriver` that lives
    /// for the rest of the process.
    pub unsafe fn add_device(&self, serial: &str, class: i32, driver: *mut c_void) -> bool {
        let serial = CString::new(serial).unwrap();
        call!(
            self.server,
            TrackedDeviceAdded,
            serial.as_ptr(),
            class,
            driver
        )
    }

    pub fn pose_updated(&self, id: u32, pose: &DriverPose_t) {
        // SAFETY: valid interface and pose.
        unsafe {
            call!(
                self.server,
                TrackedDevicePoseUpdated,
                id,
                pose,
                std::mem::size_of::<DriverPose_t>() as u32
            )
        }
    }

    pub fn poll_event(&self) -> Option<VREvent_t> {
        let mut ev = VREvent_t::zeroed();
        // SAFETY: buffer is VREVENT_SIZE bytes, which is what we report.
        unsafe { call!(self.server, PollNextEvent, &mut ev, VREVENT_SIZE as u32) }.then_some(ev)
    }

    pub fn set_projection(&self, id: u32, left: HmdRect2_t, right: HmdRect2_t) {
        // SAFETY: valid interface and rects.
        unsafe { call!(self.server, SetDisplayProjectionRaw, id, &left, &right) }
    }

    pub fn set_render_target_size(&self, id: u32, w: u32, h: u32) {
        // SAFETY: valid interface.
        unsafe { call!(self.server, SetRecommendedRenderTargetSize, id, w, h) }
    }

    pub fn container(&self, id: u32) -> PropertyContainerHandle_t {
        // SAFETY: valid interface.
        unsafe { call!(self.props, TrackedDeviceToPropertyContainer, id) }
    }

    fn write_prop(&self, c: PropertyContainerHandle_t, prop: i32, tag: u32, buf: &mut [u8]) {
        let mut w = PropertyWrite_t {
            prop,
            writeType: PropertyWrite_Set,
            eSetError: 0,
            pvBuffer: buf.as_mut_ptr().cast(),
            unBufferSize: buf.len() as u32,
            unTag: tag,
            eError: 0,
        };
        // SAFETY: one valid PropertyWrite_t pointing at a live buffer.
        let err = unsafe { call!(self.props, WritePropertyBatch, c, &mut w, 1) };
        if err != 0 || w.eError != 0 {
            self.log(&format!("property {prop} write failed: {err}/{}", w.eError));
        }
    }

    pub fn set_string(&self, c: PropertyContainerHandle_t, prop: i32, v: &str) {
        let mut b = CString::new(v).unwrap().into_bytes_with_nul();
        self.write_prop(c, prop, k_unStringPropertyTag, &mut b);
    }
    pub fn set_f32(&self, c: PropertyContainerHandle_t, prop: i32, v: f32) {
        self.write_prop(c, prop, k_unFloatPropertyTag, &mut v.to_ne_bytes());
    }
    pub fn set_i32(&self, c: PropertyContainerHandle_t, prop: i32, v: i32) {
        self.write_prop(c, prop, k_unInt32PropertyTag, &mut v.to_ne_bytes());
    }
    pub fn set_u64(&self, c: PropertyContainerHandle_t, prop: i32, v: u64) {
        self.write_prop(c, prop, k_unUint64PropertyTag, &mut v.to_ne_bytes());
    }
    pub fn set_bool(&self, c: PropertyContainerHandle_t, prop: i32, v: bool) {
        self.write_prop(c, prop, k_unBoolPropertyTag, &mut [v as u8]);
    }

    pub fn create_bool(
        &self,
        c: PropertyContainerHandle_t,
        path: &CStr,
    ) -> VRInputComponentHandle_t {
        let mut h = 0;
        // SAFETY: valid interface, string and out pointer.
        unsafe { call!(self.input, CreateBooleanComponent, c, path.as_ptr(), &mut h) };
        h
    }
    pub fn create_scalar(
        &self,
        c: PropertyContainerHandle_t,
        path: &CStr,
        two_sided: bool,
    ) -> VRInputComponentHandle_t {
        let mut h = 0;
        let units = if two_sided {
            VRScalarUnits_NormalizedTwoSided
        } else {
            VRScalarUnits_NormalizedOneSided
        };
        // SAFETY: as above.
        unsafe {
            call!(
                self.input,
                CreateScalarComponent,
                c,
                path.as_ptr(),
                &mut h,
                VRScalarType_Absolute,
                units
            )
        };
        h
    }
    pub fn create_haptic(
        &self,
        c: PropertyContainerHandle_t,
        path: &CStr,
    ) -> VRInputComponentHandle_t {
        let mut h = 0;
        // SAFETY: as above.
        unsafe { call!(self.input, CreateHapticComponent, c, path.as_ptr(), &mut h) };
        h
    }
    pub fn update_bool(&self, h: VRInputComponentHandle_t, v: bool, time_offset: f64) {
        // SAFETY: valid interface.
        unsafe { call!(self.input, UpdateBooleanComponent, h, v, time_offset) };
    }
    pub fn update_scalar(&self, h: VRInputComponentHandle_t, v: f32, time_offset: f64) {
        // SAFETY: valid interface.
        unsafe { call!(self.input, UpdateScalarComponent, h, v, time_offset) };
    }
}

/// `log` backend that writes into SteamVR's vrserver.txt.
pub struct VrLogger;

impl log::Log for VrLogger {
    fn enabled(&self, _: &log::Metadata) -> bool {
        true
    }
    fn log(&self, record: &log::Record) {
        if let Some(h) = HOST.get() {
            h.log(&format!("[{}] {}", record.level(), record.args()));
        }
    }
    fn flush(&self) {}
}
