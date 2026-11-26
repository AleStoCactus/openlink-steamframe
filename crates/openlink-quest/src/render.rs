//! Draws decoded pictures into the eye swapchains.
//!
//! The picture holds both eyes side by side. Each eye's half is drawn full-screen into that
//! eye's swapchain image; the OpenXR compositor then reprojects it from the pose SteamVR
//! rendered with to the current head pose. A foveated picture (squeezed edges, see
//! `openlink_protocol::foveation`) is stretched back to full size on the way.

use crate::egl;
use glow::HasContext;
use ndk::media::image_reader::Image;
use crate::decoder::Picture;
use std::collections::VecDeque;

const VERTEX: &str = r#"#version 300 es
out vec2 uv;
void main() {
    // One triangle covering the viewport.
    vec2 p = vec2(float((gl_VertexID << 1) & 2), float(gl_VertexID & 2));
    uv = p;
    gl_Position = vec4(p * 2.0 - 1.0, 0.0, 1.0);
}
"#;

const FRAGMENT: &str = r#"#version 300 es
#extension GL_OES_EGL_image_external_essl3 : require
// highp: texture coordinates across a 4000-pixel picture need more than mediump's 11 bits.
// In fp16, u in [0.5, 1) moves in steps of 2^-11, i.e. 2 texels: the right eye's half of the
// picture came out visibly pixelated while the left (u < 0.5, finer steps) looked fine.
precision highp float;
uniform samplerExternalOES picture;
// This eye's part of the picture: u0, v0, u1, v1 (v0 = top row).
uniform vec4 rect;
// Foveation, mirrored from openlink_protocol::foveation::Foveation::to_coded, in pixels:
// fov_size is the full-resolution and the encoded eye size (w, h, w, h), fov_band the band's
// length (0: the picture is not foveated) and fov_start where it starts in this eye (full x,
// encoded x, full y, encoded y).
uniform vec4 fov_size;
uniform vec2 fov_band;
uniform vec4 fov_start;
in vec2 uv;
out vec4 color;
vec3 srgb_to_linear(vec3 c) {
    return mix(c / 12.92, pow((c + 0.055) / 1.055, vec3(2.4)), step(vec3(0.04045), c));
}
// Growth of the encoded pixel size along a side e full and l encoded pixels long.
float growth(float e, float l) {
    return 2.0 * max(e - l, 0.0) / max(l * l, 0.25);
}
// Encoded pixels from the band for a point s full pixels from it.
float squeeze(float a, float s) {
    return 2.0 * s / (1.0 + sqrt(1.0 + 2.0 * a * s));
}
// One axis, in pixels: full-resolution position x -> position in the encoded eye.
float foveate(float x, float full, float coded, float band, float b0, float c0) {
    if (x < b0) {
        return c0 - squeeze(growth(b0, c0), b0 - x);
    }
    if (x > b0 + band) {
        return c0 + band + squeeze(growth(full - b0 - band, coded - c0 - band), x - b0 - band);
    }
    return c0 + x - b0;
}
void main() {
    // Position in the full eye image, y down (GL framebuffers are bottom-up; the picture's
    // first row is its top).
    vec2 p = vec2(uv.x, 1.0 - uv.y);
    if (fov_band.x > 0.0) {
        // The swapchain is the full-resolution size, so a fragment's center is a pixel center
        // and, inside the band, lands on an encoded pixel's center.
        vec2 x = p * fov_size.xy;
        p = vec2(foveate(x.x, fov_size.x, fov_size.z, fov_band.x, fov_start.x, fov_start.y),
                 foveate(x.y, fov_size.y, fov_size.w, fov_band.y, fov_start.z, fov_start.w)) / fov_size.zw;
    }
    vec2 t = mix(rect.xy, rect.zw, p);
    // The decoder outputs sRGB-encoded values; the swapchain is sRGB and encodes on write.
    color = vec4(srgb_to_linear(texture(picture, t).rgb), 1.0);
}
"#;

const TEXTURE_EXTERNAL_OES: u32 = 0x8D65;

/// Decoded pictures stay alive until the GPU is surely done with them.
const KEEP: usize = 3;

struct Current {
    image: Image,
    egl_image: Option<egl::Image>,
    info: Picture,
    /// Per eye: u0, v0, u1, v1.
    rects: [[f32; 4]; 2],
}

pub struct Renderer {
    gl: glow::Context,
    program: glow::Program,
    vao: glow::VertexArray,
    texture: glow::Texture,
    fbo: glow::Framebuffer,
    rect_loc: glow::UniformLocation,
    fov_size_loc: Option<glow::UniformLocation>,
    fov_band_loc: Option<glow::UniformLocation>,
    fov_start_loc: Option<glow::UniformLocation>,
    frames: VecDeque<Current>,
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
            let fov_size_loc = gl.get_uniform_location(program, "fov_size");
            let fov_band_loc = gl.get_uniform_location(program, "fov_band");
            let fov_start_loc = gl.get_uniform_location(program, "fov_start");
            gl.use_program(Some(program));
            if let Some(loc) = gl.get_uniform_location(program, "picture") {
                gl.uniform_1_i32(Some(&loc), 0);
            }
            let texture = gl.create_texture()?;
            gl.bind_texture(TEXTURE_EXTERNAL_OES, Some(texture));
            gl.tex_parameter_i32(TEXTURE_EXTERNAL_OES, glow::TEXTURE_MIN_FILTER, glow::LINEAR as i32);
            gl.tex_parameter_i32(TEXTURE_EXTERNAL_OES, glow::TEXTURE_MAG_FILTER, glow::LINEAR as i32);
            gl.tex_parameter_i32(TEXTURE_EXTERNAL_OES, glow::TEXTURE_WRAP_S, glow::CLAMP_TO_EDGE as i32);
            gl.tex_parameter_i32(TEXTURE_EXTERNAL_OES, glow::TEXTURE_WRAP_T, glow::CLAMP_TO_EDGE as i32);
            Ok(Self {
                vao: gl.create_vertex_array()?,
                fbo: gl.create_framebuffer()?,
                program,
                texture,
                rect_loc,
                fov_size_loc,
                fov_band_loc,
                fov_start_loc,
                frames: VecDeque::new(),
                gl,
            })
        }
    }

    /// Switches to a newly decoded picture. Returns the info of the picture now shown.
    pub fn update(&mut self, egl: &egl::Context, new: Option<(Image, Picture)>) -> Option<Picture> {
        if let Some((image, info)) = new {
            match Self::prepare(egl, image, info) {
                Ok(c) => {
                    // SAFETY: GL calls on the render thread.
                    unsafe { self.gl.bind_texture(TEXTURE_EXTERNAL_OES, Some(self.texture)) };
                    egl.bind_external(c.egl_image.as_ref().unwrap());
                    self.frames.push_back(c);
                    while self.frames.len() > KEEP {
                        let mut old = self.frames.pop_front().unwrap();
                        if let Some(i) = old.egl_image.take() {
                            egl.destroy(i);
                        }
                    }
                }
                Err(e) => log::warn!("cannot show picture: {e}"),
            }
        }
        self.frames.back().map(|c| c.info)
    }

    fn prepare(egl: &egl::Context, image: Image, info: Picture) -> Result<Current, String> {
        let buffer = image.hardware_buffer().map_err(|e| format!("hardware buffer: {e:?}"))?;
        let desc = buffer.describe();
        let crop = image.crop_rect().map_err(|e| format!("crop: {e:?}"))?;
        let egl_image = egl.import(buffer.as_ptr().cast())?;
        let (w, h) = (desc.width as f32, desc.height as f32);
        let (left, top) = (crop.left as f32 / w, crop.top as f32 / h);
        let (right, bottom) = (crop.right as f32 / w, crop.bottom as f32 / h);
        let mid = (left + right) / 2.0;
        static LOGGED: std::sync::Once = std::sync::Once::new();
        LOGGED.call_once(|| {
            log::info!(
                "decoded picture: buffer {}x{} (stride {}), crop {:?}; eye rects left {:?} right {:?}",
                desc.width,
                desc.height,
                desc.stride,
                (crop.left, crop.top, crop.right, crop.bottom),
                [left, top, mid, bottom],
                [mid, top, right, bottom]
            )
        });
        Ok(Current {
            image,
            egl_image: Some(egl_image),
            info,
            rects: [[left, top, mid, bottom], [mid, top, right, bottom]],
        })
    }

    /// Draws `eye` of the current picture into a swapchain image.
    pub fn draw(&self, eye: usize, target: u32, width: i32, height: i32) {
        let Some(c) = self.frames.back() else { return };
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
            gl.use_program(Some(self.program));
            let r = c.rects[eye];
            gl.uniform_4_f32(Some(&self.rect_loc), r[0], r[1], r[2], r[3]);
            // Each picture is unwarped with the parameters it was encoded with.
            match c.info.info.foveation {
                Some(f) => {
                    let ([fw, fh], [cw, ch], [bw, bh]) = (f.full, f.coded, f.band);
                    let [[bx, cx], [by, cy]] = f.start[eye];
                    let v = |n: u32| n as f32;
                    gl.uniform_4_f32(self.fov_size_loc.as_ref(), v(fw), v(fh), v(cw), v(ch));
                    gl.uniform_2_f32(self.fov_band_loc.as_ref(), v(bw), v(bh));
                    gl.uniform_4_f32(self.fov_start_loc.as_ref(), v(bx), v(cx), v(by), v(cy));
                }
                None => gl.uniform_2_f32(self.fov_band_loc.as_ref(), 0.0, 0.0),
            }
            gl.active_texture(glow::TEXTURE0);
            gl.bind_texture(TEXTURE_EXTERNAL_OES, Some(self.texture));
            gl.bind_vertex_array(Some(self.vao));
            gl.draw_arrays(glow::TRIANGLES, 0, 3);
            gl.framebuffer_texture_2d(glow::FRAMEBUFFER, glow::COLOR_ATTACHMENT0, glow::TEXTURE_2D, None, 0);
            gl.bind_framebuffer(glow::FRAMEBUFFER, None);
        }
        let _ = &c.image;
    }
}
