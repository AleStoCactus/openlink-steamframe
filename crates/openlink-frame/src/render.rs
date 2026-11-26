//! Draws decoded pictures into the eye swapchains.
//!
//! A picture is NV12 with both eyes side by side. The luma plane is sampled as an R8 texture,
//! the chroma plane as RG8 at half resolution, and the shader turns them back into RGB (BT.709,
//! limited range, as the server's encoder writes). Each eye's half is drawn full-screen into
//! that eye's swapchain image; the OpenXR compositor then reprojects it from the pose SteamVR
//! rendered with to the current head pose.
//!
//! Planes reach GL in one of two ways: imported as dmabufs (no copy; the buffer stays lent
//! from the decoder while it may be read), or, where import is unavailable, uploaded into the
//! renderer's own textures (one copy; the buffer goes back at once).

use crate::decoder::{Decoded, Layout, Picture, PlaneRef};
use crate::egl;
use glow::HasContext;
use std::collections::{HashMap, VecDeque};
use std::os::fd::AsFd;

const VERTEX: &str = r#"#version 330 core
out vec2 uv;
void main() {
    // One triangle covering the viewport.
    vec2 p = vec2(float((gl_VertexID << 1) & 2), float(gl_VertexID & 2));
    uv = p;
    gl_Position = vec4(p * 2.0 - 1.0, 0.0, 1.0);
}
"#;

const FRAGMENT: &str = r#"#version 330 core
uniform sampler2D luma;
uniform sampler2D chroma;
// This eye's part of the picture: u0, v0, u1, v1 (v0 = top row).
uniform vec4 rect;
// Whether to undo the sRGB encoding (the swapchain is sRGB and encodes again on write).
uniform int linearize;
in vec2 uv;
out vec4 color;
vec3 srgb_to_linear(vec3 c) {
    return mix(c / 12.92, pow((c + 0.055) / 1.055, vec3(2.4)), step(vec3(0.04045), c));
}
void main() {
    // GL framebuffers are bottom-up; the picture's first row (t = 0) is its top.
    vec2 t = vec2(mix(rect.x, rect.z, uv.x), mix(rect.w, rect.y, uv.y));
    float y = (texture(luma, t).r * 255.0 - 16.0) / 219.0;
    vec2 c = (texture(chroma, t).rg * 255.0 - 128.0) / 224.0;
    vec3 rgb = clamp(vec3(
        y + 1.5748 * c.y,
        y - 0.18732 * c.x - 0.46812 * c.y,
        y + 1.8556 * c.x), 0.0, 1.0);
    // The picture holds sRGB-encoded values, as SteamVR rendered them.
    color = vec4(linearize != 0 ? srgb_to_linear(rgb) : rgb, 1.0);
}
"#;

/// Lent pictures kept until the GPU is surely done reading them.
const KEEP: usize = 3;

/// A capture buffer imported as two textures.
struct Imported {
    textures: [glow::Texture; 2],
    images: Vec<egl::Image>,
}

struct Current {
    info: Picture,
    textures: [glow::Texture; 2],
    /// Per eye: u0, v0, u1, v1.
    rects: [[f32; 4]; 2],
}

pub struct Renderer {
    gl: glow::Context,
    program: glow::Program,
    vao: glow::VertexArray,
    fbo: glow::Framebuffer,
    rect_loc: glow::UniformLocation,
    linearize_loc: glow::UniformLocation,
    /// The upload path's textures and their size.
    uploaded: [glow::Texture; 2],
    uploaded_size: (u32, u32),
    /// dmabuf imports, per (decoder generation, buffer index).
    imported: HashMap<(u64, u32), Imported>,
    /// Set once an import failed; from then on pictures are uploaded.
    import_failed: bool,
    /// Pictures still lent from the decoder (imported path), newest last.
    held: VecDeque<Decoded>,
    current: Option<Current>,
}

impl Renderer {
    pub fn new(gl: glow::Context) -> Result<Self, String> {
        // SAFETY: GL calls on the thread where the context is current.
        unsafe {
            let program = gl.create_program()?;
            for (ty, src) in [(glow::VERTEX_SHADER, VERTEX), (glow::FRAGMENT_SHADER, FRAGMENT)] {
                let s = gl.create_shader(ty)?;
                gl.shader_source(s, src);
                gl.compile_shader(s);
                if !gl.get_shader_compile_status(s) {
                    return Err(format!("shader: {}", gl.get_shader_info_log(s)));
                }
                gl.attach_shader(program, s);
            }
            gl.link_program(program);
            if !gl.get_program_link_status(program) {
                return Err(format!("program: {}", gl.get_program_info_log(program)));
            }
            let rect_loc = gl.get_uniform_location(program, "rect").ok_or("no rect uniform")?;
            let linearize_loc = gl.get_uniform_location(program, "linearize").ok_or("no linearize uniform")?;
            gl.use_program(Some(program));
            for (name, unit) in [("luma", 0), ("chroma", 1)] {
                if let Some(loc) = gl.get_uniform_location(program, name) {
                    gl.uniform_1_i32(Some(&loc), unit);
                }
            }
            let uploaded = [new_texture(&gl)?, new_texture(&gl)?];
            Ok(Self {
                vao: gl.create_vertex_array()?,
                fbo: gl.create_framebuffer()?,
                program,
                rect_loc,
                linearize_loc,
                uploaded,
                uploaded_size: (0, 0),
                imported: HashMap::new(),
                import_failed: false,
                held: VecDeque::new(),
                current: None,
                gl,
            })
        }
    }

    /// Switches to a newly decoded picture. Returns the info of the picture now shown.
    pub fn update(&mut self, egl: &egl::Context, new: Option<Decoded>) -> Option<Picture> {
        let Some(d) = new else { return self.current.as_ref().map(|c| c.info) };
        let layout = d.layout;
        let rects = eye_rects(&layout);
        // Imports belong to one decoder; a new one means new buffers.
        let generation = d.generation;
        let stale: Vec<_> = self.imported.keys().filter(|(g, _)| *g != generation).copied().collect();
        for key in stale {
            let old = self.imported.remove(&key).unwrap();
            self.free(egl, old);
        }

        let textures = if egl.dmabuf && !self.import_failed && d.buffer().dmabufs.is_some() {
            match self.import(egl, &d) {
                Ok(t) => Some(t),
                Err(e) => {
                    log::warn!("cannot import decoded pictures ({e}); uploading them instead");
                    self.import_failed = true;
                    None
                }
            }
        } else {
            None
        };
        let info = d.picture;
        let textures = match textures {
            Some(t) => {
                self.held.push_back(d);
                while self.held.len() > KEEP {
                    self.held.pop_front(); // back to the decoder
                }
                t
            }
            None => {
                self.held.clear();
                self.upload(&d);
                self.uploaded
                // `d` drops here: the copy is made, the buffer goes back to the decoder.
            }
        };
        static LOGGED: std::sync::Once = std::sync::Once::new();
        LOGGED.call_once(|| log::info!("first picture: {layout:?}, eye rects {rects:?}"));
        self.current = Some(Current { info, textures, rects });
        Some(info)
    }

    fn import(&mut self, egl: &egl::Context, d: &Decoded) -> Result<[glow::Texture; 2], String> {
        let key = (d.generation, d.index);
        if let Some(i) = self.imported.get(&key) {
            return Ok(i.textures);
        }
        let fds = d.buffer().dmabufs.as_ref().ok_or("buffer not exported")?;
        let l = &d.layout;
        let plane = |p: PlaneRef, fourcc: u32, size: (u32, u32)| {
            let fd = fds.get(p.memory).ok_or("missing plane")?;
            egl.import_plane(fd.as_fd(), fourcc, size, p.offset, p.pitch)
        };
        let luma = plane(l.luma, egl::DRM_FORMAT_R8, (l.width, l.height))?;
        let chroma = match plane(l.chroma, egl::DRM_FORMAT_GR88, (l.width / 2, l.height / 2)) {
            Ok(c) => c,
            Err(e) => {
                egl.destroy(luma);
                return Err(e);
            }
        };
        let gl = &self.gl;
        // SAFETY: GL calls on the render thread.
        let textures = unsafe { [new_texture(gl)?, new_texture(gl)?] };
        for (t, image) in textures.iter().zip([&luma, &chroma]) {
            // SAFETY: GL calls on the render thread; a live image.
            unsafe { gl.bind_texture(glow::TEXTURE_2D, Some(*t)) };
            egl.bind_texture(image);
        }
        // SAFETY: GL call on the render thread.
        let error = unsafe { gl.get_error() };
        let imported = Imported { textures, images: vec![luma, chroma] };
        if error != glow::NO_ERROR {
            self.free(egl, imported);
            return Err(format!("binding the imported planes failed (GL error {error:#x})"));
        }
        self.imported.insert(key, imported);
        Ok(textures)
    }

    fn upload(&mut self, d: &Decoded) {
        let gl = &self.gl;
        let l = &d.layout;
        // SAFETY: GL calls on the render thread; the slices cover the rows read (checked).
        unsafe {
            if self.uploaded_size != (l.width, l.height) {
                for (t, format, (w, h)) in [
                    (self.uploaded[0], glow::R8, (l.width, l.height)),
                    (self.uploaded[1], glow::RG8, (l.width / 2, l.height / 2)),
                ] {
                    gl.bind_texture(glow::TEXTURE_2D, Some(t));
                    gl.tex_storage_2d(glow::TEXTURE_2D, 1, format, w as i32, h as i32);
                }
                self.uploaded_size = (l.width, l.height);
            }
            gl.pixel_store_i32(glow::UNPACK_ALIGNMENT, 1);
            for (t, plane, format, bytes_per_pixel, (w, h)) in [
                (self.uploaded[0], l.luma, glow::RED, 1, (l.width, l.height)),
                (self.uploaded[1], l.chroma, glow::RG, 2, (l.width / 2, l.height / 2)),
            ] {
                let data = d.plane(plane);
                let needed = plane.pitch * (h as usize - 1) + w as usize * bytes_per_pixel;
                if data.len() < needed {
                    log::warn!("decoded plane is {} bytes, {needed} needed; picture skipped", data.len());
                    return;
                }
                gl.pixel_store_i32(glow::UNPACK_ROW_LENGTH, (plane.pitch / bytes_per_pixel) as i32);
                gl.bind_texture(glow::TEXTURE_2D, Some(t));
                gl.tex_sub_image_2d(
                    glow::TEXTURE_2D,
                    0,
                    0,
                    0,
                    w as i32,
                    h as i32,
                    format,
                    glow::UNSIGNED_BYTE,
                    glow::PixelUnpackData::Slice(Some(&data[..needed])),
                );
            }
            gl.pixel_store_i32(glow::UNPACK_ROW_LENGTH, 0);
        }
    }

    fn free(&self, egl: &egl::Context, i: Imported) {
        // SAFETY: GL calls on the render thread.
        unsafe {
            for t in i.textures {
                self.gl.delete_texture(t);
            }
        }
        for image in i.images {
            egl.destroy(image);
        }
    }

    /// Draws `eye` of the current picture into a swapchain image. `srgb`: the swapchain
    /// encodes to sRGB on write.
    pub fn draw(&self, eye: usize, target: u32, width: i32, height: i32, srgb: bool) {
        let Some(c) = &self.current else { return };
        let gl = &self.gl;
        // SAFETY: GL calls on the render thread; `target` is a live swapchain texture.
        unsafe {
            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(self.fbo));
            gl.framebuffer_texture_2d(
                glow::FRAMEBUFFER,
                glow::COLOR_ATTACHMENT0,
                glow::TEXTURE_2D,
                Some(glow::NativeTexture(std::num::NonZeroU32::new(target).unwrap())),
                0,
            );
            gl.viewport(0, 0, width, height);
            gl.disable(glow::BLEND);
            gl.disable(glow::DEPTH_TEST);
            gl.disable(glow::SCISSOR_TEST);
            // Desktop GL only encodes to sRGB when asked (GLES always does).
            if srgb {
                gl.enable(glow::FRAMEBUFFER_SRGB);
            } else {
                gl.disable(glow::FRAMEBUFFER_SRGB);
            }
            gl.use_program(Some(self.program));
            let r = c.rects[eye];
            gl.uniform_4_f32(Some(&self.rect_loc), r[0], r[1], r[2], r[3]);
            gl.uniform_1_i32(Some(&self.linearize_loc), srgb as i32);
            for (unit, t) in [glow::TEXTURE0, glow::TEXTURE1].into_iter().zip(c.textures) {
                gl.active_texture(unit);
                gl.bind_texture(glow::TEXTURE_2D, Some(t));
            }
            gl.bind_vertex_array(Some(self.vao));
            gl.draw_arrays(glow::TRIANGLES, 0, 3);
            gl.framebuffer_texture_2d(glow::FRAMEBUFFER, glow::COLOR_ATTACHMENT0, glow::TEXTURE_2D, None, 0);
            gl.bind_framebuffer(glow::FRAMEBUFFER, None);
            gl.active_texture(glow::TEXTURE0);
        }
    }
}

/// # Safety
/// GL calls on the thread where the context is current.
unsafe fn new_texture(gl: &glow::Context) -> Result<glow::Texture, String> {
    // SAFETY: per the caller.
    unsafe {
        let t = gl.create_texture()?;
        gl.bind_texture(glow::TEXTURE_2D, Some(t));
        gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MIN_FILTER, glow::LINEAR as i32);
        gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MAG_FILTER, glow::LINEAR as i32);
        gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_S, glow::CLAMP_TO_EDGE as i32);
        gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_T, glow::CLAMP_TO_EDGE as i32);
        Ok(t)
    }
}

/// Each eye's half of the visible picture, in texture coordinates (v = 0 is the top row).
fn eye_rects(l: &Layout) -> [[f32; 4]; 2] {
    let (w, h) = (l.width as f32, l.height as f32);
    let v = l.visible;
    let (left, top) = (v.left as f32 / w, v.top as f32 / h);
    let (right, bottom) = ((v.left as f32 + v.width as f32) / w, (v.top as f32 + v.height as f32) / h);
    let mid = (left + right) / 2.0;
    [[left, top, mid, bottom], [mid, top, right, bottom]]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::v4l2::Rect;

    #[test]
    fn eye_rects_split_the_visible_picture() {
        let layout = Layout {
            width: 4224,
            height: 2208,
            visible: Rect { left: 0, top: 0, width: 4128, height: 2200 },
            luma: PlaneRef { memory: 0, offset: 0, pitch: 4224 },
            chroma: PlaneRef { memory: 0, offset: 4224 * 2208, pitch: 4224 },
        };
        let [l, r] = eye_rects(&layout);
        assert_eq!(l[0], 0.0);
        assert!((l[2] - 2064.0 / 4224.0).abs() < 1e-6);
        assert_eq!(l[2], r[0]);
        assert!((r[2] - 4128.0 / 4224.0).abs() < 1e-6);
        assert!((l[3] - 2200.0 / 2208.0).abs() < 1e-6);
    }

    /// Uploads a synthetic NV12 picture (each eye one colour, padded rows), draws both eyes
    /// into sRGB targets and reads them back. Needs EGL with desktop GL (Mesa's llvmpipe will
    /// do); skipped where there is none.
    #[test]
    fn draws_a_known_picture() {
        use crate::decoder::{CaptureBuffer, Decoded, Picture};
        use crate::v4l2::Mapping;
        use openlink_protocol::{video::FrameInfo, Pose};

        let egl = match egl::Context::new((3, 3)) {
            Ok(e) => e,
            Err(e) => {
                eprintln!("skipped: no EGL/OpenGL here ({e})");
                return;
            }
        };
        let mut renderer = Renderer::new(egl.gl()).unwrap();
        // Two 32x16 eyes in a 64x16 picture, coded 64x24 with a 96-byte pitch.
        let (w, h, coded_h, pitch) = (64usize, 16usize, 24usize, 96usize);
        let colours = [[200u8, 60, 30], [20, 120, 220]];
        let yuv = |c: [u8; 3]| {
            let [r, g, b] = c.map(|v| v as f32 / 255.0);
            let y = 0.2126 * r + 0.7152 * g + 0.0722 * b;
            let (cb, cr) = ((b - y) / 1.8556, (r - y) / 1.5748);
            [16.0 + 219.0 * y, 128.0 + 224.0 * cb, 128.0 + 224.0 * cr].map(|v| v.round() as u8)
        };
        let mut data = vec![0u8; pitch * coded_h * 3 / 2];
        for row in 0..coded_h {
            for x in 0..w {
                data[row * pitch + x] = yuv(colours[x * 2 / w])[0];
            }
        }
        for row in 0..coded_h / 2 {
            for x in 0..w / 2 {
                let [_, u, v] = yuv(colours[x * 4 / w]);
                data[pitch * coded_h + row * pitch + 2 * x] = u;
                data[pitch * coded_h + row * pitch + 2 * x + 1] = v;
            }
        }
        // SAFETY: an anonymous memory file, sized and filled before mapping.
        let map = unsafe {
            let fd = libc::memfd_create(c"picture".as_ptr(), 0);
            assert!(fd >= 0);
            let fd = <std::os::fd::OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(fd);
            let mut file = std::fs::File::from(fd);
            std::io::Write::write_all(&mut file, &data).unwrap();
            Mapping::new(file.as_fd(), 0, data.len() as u32).unwrap()
        };
        let layout = Layout {
            width: w as u32,
            height: coded_h as u32,
            visible: crate::v4l2::Rect { left: 0, top: 0, width: w as u32, height: h as u32 },
            luma: PlaneRef { memory: 0, offset: 0, pitch },
            chroma: PlaneRef { memory: 0, offset: pitch * coded_h, pitch },
        };
        let info = FrameInfo { idr: true, render_pose: Pose::IDENTITY, present_ns: 7 };
        let picture = Picture { info, received_ns: 0, queued_ns: 0, decoded_ns: 0 };
        let (d, released) = Decoded::for_test(picture, layout, vec![CaptureBuffer { maps: vec![map], dmabufs: None }]);
        assert_eq!(renderer.update(&egl, Some(d)).unwrap().info.present_ns, 7);
        assert_eq!(released.try_recv(), Ok((1, 0)), "an uploaded picture goes straight back");

        let gl = &renderer.gl;
        for (eye, colour) in colours.iter().enumerate() {
            // SAFETY: GL calls on this thread, where the context is current.
            unsafe {
                let target = gl.create_texture().unwrap();
                gl.bind_texture(glow::TEXTURE_2D, Some(target));
                gl.tex_storage_2d(glow::TEXTURE_2D, 1, glow::SRGB8_ALPHA8, 8, 8);
                renderer.draw(eye, target.0.get(), 8, 8, true);
                let fbo = gl.create_framebuffer().unwrap();
                gl.bind_framebuffer(glow::FRAMEBUFFER, Some(fbo));
                gl.framebuffer_texture_2d(glow::FRAMEBUFFER, glow::COLOR_ATTACHMENT0, glow::TEXTURE_2D, Some(target), 0);
                let mut px = [0u8; 4];
                gl.read_pixels(4, 4, 1, 1, glow::RGBA, glow::UNSIGNED_BYTE, glow::PixelPackData::Slice(Some(&mut px)));
                for (got, want) in px[..3].iter().zip(colour) {
                    assert!((*got as i32 - *want as i32).abs() <= 3, "eye {eye}: {px:?} vs {colour:?}");
                }
            }
        }
    }

    /// The shader's inverse of the server's BT.709 limited-range conversion.
    #[test]
    fn yuv_round_trip() {
        let encode = |r: f32, g: f32, b: f32| {
            let y = 0.2126 * r + 0.7152 * g + 0.0722 * b;
            let cb = (b - y) / 1.8556;
            let cr = (r - y) / 1.5748;
            ((16.0 + 219.0 * y) / 255.0, (128.0 + 224.0 * cb) / 255.0, (128.0 + 224.0 * cr) / 255.0)
        };
        let decode = |y: f32, u: f32, v: f32| {
            let y = (y * 255.0 - 16.0) / 219.0;
            let (u, v) = ((u * 255.0 - 128.0) / 224.0, (v * 255.0 - 128.0) / 224.0);
            (y + 1.5748 * v, y - 0.18732 * u - 0.46812 * v, y + 1.8556 * u)
        };
        for rgb in [(1.0, 0.0, 0.0), (0.0, 1.0, 0.0), (0.0, 0.0, 1.0), (0.3, 0.6, 0.9), (1.0, 1.0, 1.0)] {
            let (y, u, v) = encode(rgb.0, rgb.1, rgb.2);
            let back = decode(y, u, v);
            assert!((back.0 - rgb.0).abs() < 1e-3 && (back.1 - rgb.1).abs() < 1e-3 && (back.2 - rgb.2).abs() < 1e-3, "{rgb:?} -> {back:?}");
        }
    }
}
