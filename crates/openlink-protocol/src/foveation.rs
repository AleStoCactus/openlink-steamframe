//! Foveated encoding: the edges of each eye's picture are squeezed before encoding and
//! stretched back on the headset, so the encoder has fewer pixels to code and spends fewer bits
//! where the lens blurs anyway.
//!
//! Each axis of each eye is warped on its own (the warp is separable). A band around the lens's
//! optical axis keeps one encoded pixel per full-resolution pixel. Outside it, the size of an
//! encoded pixel grows linearly with its distance from the band, to about [`EDGE_SCALE`] full
//! pixels at the image edge: resolution falls off smoothly instead of in a visible step.
//!
//! The band is placed on whole pixels, in both the full-resolution and the encoded picture, and
//! both pictures are even sized. Inside the band an encoded pixel then lands exactly on a
//! displayed pixel and the headset's bilinear sampling copies it instead of blending it with its
//! neighbours. (A first version kept the band in fractions of the image: every encoded pixel
//! sat a fraction of a pixel off its displayed pixel, and the whole picture, center included,
//! came out visibly softer than unfoveated at any setting.)
//!
//! A side of the band spans `e` full pixels and `l` encoded pixels (`l <= e`). With `d` the
//! distance from the band in encoded pixels, the full-resolution distance is
//! `s = d + a d^2 / 2` with `a = 2 (e - l) / l^2`: pixels are 1 wide at the band and
//! `1 + a l = 2 e / l - 1` at the edge, the side ends exactly at the image edge, and both
//! directions have a closed form (the forward one, `d = 2 s / (1 + sqrt(1 + 2 a s))`, is the
//! quadratic formula arranged to stay exact as `a` goes to 0). The server's converter
//! (`rgba_to_nv12.comp`) uses `s(d)`, the Quest renderer (`render.rs`) `d(s)`; both mirror
//! [`Foveation::to_full`] and [`Foveation::to_coded`], which the tests pin.
//!
//! The parameters travel with every frame ([`crate::video::FrameInfo::foveation`]), so the
//! headset unwarps each picture exactly as it was warped, even while a new setting takes over.

use crate::Fov;
use serde::{Deserialize, Serialize};

/// Full-resolution pixels per encoded pixel at the very edge of the image (nominal: each side
/// is rounded to whole pixels).
pub const EDGE_SCALE: f32 = 4.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Foveation {
    /// Per-eye size the picture is shown at (the stream's `eye_width` x `eye_height`).
    pub full: [u32; 2],
    /// Per-eye size of the encoded picture, the same for both eyes.
    pub coded: [u32; 2],
    /// Length of the full-resolution band on each axis, in pixels of either picture.
    pub band: [u32; 2],
    /// Per eye and axis, where the band starts: `[full, coded]` pixels from the left or top.
    pub start: [[[u32; 2]; 2]; 2],
}

/// Rounds to the nearest even number (both pictures stay aligned to 4:2:0 chroma).
fn even(v: f32) -> u32 {
    ((v.max(0.0) / 2.0).round() as u32) * 2
}

impl Foveation {
    /// `fraction` of each eye's width and height (of `full`, the per-eye size shown) kept at
    /// full resolution, centered on the lens's optical axis (where the tangents are 0: off
    /// center, as the Quest's FOV is asymmetric) and moved inward where it would cross the
    /// image edge. `None` (no foveation) when that leaves nothing to squeeze.
    pub fn new(fraction: f32, fov: &[Fov; 2], full: (u32, u32)) -> Option<Self> {
        if fraction.is_nan() || fraction >= 1.0 {
            return None;
        }
        let fraction = fraction.max(0.05);
        let full = [full.0 & !1, full.1 & !1];
        // Encoded length of a side per full-resolution pixel: edge scale 2 e / l - 1.
        let ratio = 2.0 / (EDGE_SCALE + 1.0);
        let mut f = Self { full, coded: [0; 2], band: [0; 2], start: [[[0; 2]; 2]; 2] };
        for axis in 0..2 {
            let w = full[axis];
            if w < 4 {
                return None;
            }
            let n = even(fraction * w as f32).clamp(2, w);
            // Where the optical axis crosses the image, as a fraction (y down).
            let center = fov.map(|f| {
                let (l, r, u, d) = (f.angle_left.tan(), f.angle_right.tan(), f.angle_up.tan(), f.angle_down.tan());
                let c = if axis == 0 { -l / (r - l) } else { u / (u - d) };
                if c.is_finite() { c } else { 0.5 }
            });
            let b0 = center.map(|c| even(c * w as f32 - n as f32 / 2.0).min(w - n));
            // Both eyes get one coded size, so their sides may differ in squeeze if the band
            // had to move inward in one eye and not the other.
            let c = even(n as f32 + (w - n) as f32 * ratio).clamp(n, w);
            // Split what is left of the coded length between the sides in proportion to their
            // full lengths, so both get the same squeeze.
            let s = c - n;
            for eye in 0..2 {
                let (el, er) = (b0[eye], w - n - b0[eye]);
                let l = if el + er == 0 { 0 } else { even(s as f32 * el as f32 / (el + er) as f32) };
                let l = l.clamp(s.saturating_sub(er), el.min(s));
                f.start[eye][axis] = [b0[eye], l];
            }
            f.coded[axis] = c;
            f.band[axis] = n;
        }
        (f.coded != f.full).then_some(f)
    }

    /// Per-eye encoded size.
    pub fn coded_eye_size(&self) -> (u32, u32) {
        (self.coded[0], self.coded[1])
    }

    fn axis(&self, eye: usize, axis: usize) -> Axis {
        let [b0, c0] = self.start[eye][axis];
        Axis {
            full: self.full[axis] as f32,
            coded: self.coded[axis] as f32,
            band: self.band[axis] as f32,
            b0: b0 as f32,
            c0: c0 as f32,
        }
    }

    /// Full-resolution position (fractions, `y` down) -> position in the encoded eye picture.
    pub fn to_coded(&self, eye: usize, p: [f32; 2]) -> [f32; 2] {
        [0, 1].map(|i| {
            let a = self.axis(eye, i);
            a.to_coded(p[i] * a.full) / a.coded
        })
    }

    /// Position in the encoded eye picture -> full-resolution position.
    pub fn to_full(&self, eye: usize, p: [f32; 2]) -> [f32; 2] {
        [0, 1].map(|i| {
            let a = self.axis(eye, i);
            a.to_full(p[i] * a.coded).0 / a.full
        })
    }

    /// How far toward the edge of the view the point at `p` in the encoded eye picture
    /// (fractions, `y` down) lies, for spending fewer bits there: 0 inside the full-quality
    /// circle, rising smoothly to 1 at the edge of the view, 1 beyond (the corners).
    ///
    /// Unlike the resolution warp, which works per axis and so keeps a rectangle sharp, this
    /// is round: it depends only on the angle from the lens's optical axis, as the lens's own
    /// blur does. The circle is the largest around the axis that fits in the full-resolution
    /// band, so it shrinks with the setting. The edge is the average of the four edges'
    /// angles: the Quest's FOV reaches further out on the temple side than the nose side, so
    /// the nose side's edge is not quite at 1.
    pub fn falloff(&self, fov: &[Fov; 2], eye: usize, p: [f32; 2]) -> f32 {
        let f = &fov[eye];
        let (l, r, u, d) = (f.angle_left.tan(), f.angle_right.tan(), f.angle_up.tan(), f.angle_down.tan());
        // Full-resolution fraction -> tangent of the angle from the axis (y up).
        let tan = |x: f32, y: f32| [l + x * (r - l), u - y * (u - d)];
        let [x, y] = self.to_full(eye, p);
        let [tx, ty] = tan(x, y);
        let radius = (tx * tx + ty * ty).sqrt();
        let [[bx, _], [by, _]] = self.start[eye];
        let (bx0, bx1) = (bx as f32 / self.full[0] as f32, (bx + self.band[0]) as f32 / self.full[0] as f32);
        let (by0, by1) = (by as f32 / self.full[1] as f32, (by + self.band[1]) as f32 / self.full[1] as f32);
        let [x0, y0] = tan(bx0, by0);
        let [x1, y1] = tan(bx1, by1);
        let inner = x0.abs().min(x1.abs()).min(y0.abs()).min(y1.abs());
        let outer = (l.abs() + r.abs() + u.abs() + d.abs()) / 4.0;
        if outer.partial_cmp(&inner) != Some(std::cmp::Ordering::Greater) {
            return 0.0;
        }
        let t = ((radius - inner) / (outer - inner)).clamp(0.0, 1.0);
        t * t * (3.0 - 2.0 * t)
    }
}

/// One axis of one eye, in pixels.
struct Axis {
    full: f32,
    coded: f32,
    band: f32,
    /// Band start, full-resolution and encoded.
    b0: f32,
    c0: f32,
}

/// The side's `a` (see the module docs): `e` full and `l` encoded pixels long.
fn growth(e: f32, l: f32) -> f32 {
    2.0 * (e - l).max(0.0) / (l * l).max(0.25)
}

impl Axis {
    /// Sides: full and encoded length before and after the band.
    fn sides(&self) -> [(f32, f32); 2] {
        [
            (self.b0, self.c0),
            (self.full - self.b0 - self.band, self.coded - self.c0 - self.band),
        ]
    }

    /// Encoded position `g` -> full-resolution position, and how many full pixels one encoded
    /// pixel covers there.
    fn to_full(&self, g: f32) -> (f32, f32) {
        let [(el, ll), (er, lr)] = self.sides();
        if g < self.c0 {
            let (a, d) = (growth(el, ll), self.c0 - g);
            (self.b0 - (d + a * d * d / 2.0), 1.0 + a * d)
        } else if g > self.c0 + self.band {
            let (a, d) = (growth(er, lr), g - self.c0 - self.band);
            (self.b0 + self.band + d + a * d * d / 2.0, 1.0 + a * d)
        } else {
            (self.b0 + g - self.c0, 1.0)
        }
    }

    /// Full-resolution position `x` -> encoded position.
    fn to_coded(&self, x: f32) -> f32 {
        let [(el, ll), (er, lr)] = self.sides();
        let d = |a: f32, s: f32| 2.0 * s / (1.0 + (1.0 + 2.0 * a * s).sqrt());
        if x < self.b0 {
            self.c0 - d(growth(el, ll), self.b0 - x)
        } else if x > self.b0 + self.band {
            self.c0 + self.band + d(growth(er, lr), x - self.b0 - self.band)
        } else {
            self.c0 + x - self.b0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Quest 3, as its runtime reports it.
    fn quest3() -> [Fov; 2] {
        let deg = |d: f32| d.to_radians();
        let left = Fov { angle_left: deg(-54.0), angle_right: deg(40.0), angle_up: deg(44.0), angle_down: deg(-55.0) };
        [left, Fov { angle_left: -left.angle_right, angle_right: -left.angle_left, ..left }]
    }

    const PANEL: (u32, u32) = (2064, 2208);

    #[test]
    fn off_at_full_fraction() {
        assert_eq!(Foveation::new(1.0, &quest3(), PANEL), None);
        assert_eq!(Foveation::new(f32::NAN, &quest3(), PANEL), None);
    }

    #[test]
    fn round_trips_and_covers_the_picture() {
        for fraction in [0.2, 0.3, 0.5, 0.8, 0.95] {
            let f = Foveation::new(fraction, &quest3(), PANEL).unwrap();
            for eye in 0..2 {
                for (x, y) in [(0.0, 0.0), (1.0, 1.0), (0.1, 0.9), (0.5, 0.5), (0.62, 0.4), (0.97, 0.03)] {
                    let u = f.to_coded(eye, [x, y]);
                    let back = f.to_full(eye, u);
                    assert!((back[0] - x).abs() < 1e-4 && (back[1] - y).abs() < 1e-4, "{fraction} {eye} {x},{y} -> {u:?} -> {back:?}");
                }
                // The whole picture maps onto the whole encoded picture.
                let (a, b) = (f.to_coded(eye, [0.0, 0.0]), f.to_coded(eye, [1.0, 1.0]));
                assert!(a.iter().all(|v| v.abs() < 1e-5) && b.iter().all(|v| (v - 1.0).abs() < 1e-4), "{a:?} {b:?}");
            }
        }
    }

    #[test]
    fn band_is_pixel_exact() {
        for fraction in [0.2, 0.35, 0.5, 0.75] {
            let f = Foveation::new(fraction, &quest3(), PANEL).unwrap();
            assert!(f.coded.iter().chain(&f.band).all(|v| v % 2 == 0));
            for eye in 0..2 {
                for axis in 0..2 {
                    let a = f.axis(eye, axis);
                    // The center of every displayed pixel in the band samples the center of an
                    // encoded pixel, so the headset's bilinear filter copies it unblurred.
                    for i in [0, 1, f.band[axis] / 2, f.band[axis] - 1] {
                        let g = a.to_coded(a.b0 + i as f32 + 0.5);
                        assert!((g - (a.c0 + i as f32 + 0.5)).abs() < 1e-3, "{fraction} {eye} {axis} {i}: {g}");
                        assert!((g - 0.5).fract().abs() < 1e-3);
                    }
                }
            }
        }
    }

    #[test]
    fn resolution_falls_off_smoothly_to_the_edge_scale() {
        let f = Foveation::new(0.5, &quest3(), PANEL).unwrap();
        for eye in 0..2 {
            for axis in 0..2 {
                let a = f.axis(eye, axis);
                // A quarter of the resolution at the image edges.
                let (_, edge0) = a.to_full(0.0);
                let (_, edge1) = a.to_full(a.coded);
                assert!((edge0 - EDGE_SCALE).abs() < 0.1 && (edge1 - EDGE_SCALE).abs() < 0.1, "{edge0} {edge1}");
                // Continuous at the band edges (no step in resolution).
                assert!((a.to_full(a.c0 - 0.01).1 - 1.0).abs() < 0.01);
                assert!((a.to_full(a.c0 + a.band + 0.01).1 - 1.0).abs() < 0.01);
            }
        }
    }

    #[test]
    fn band_sits_on_the_optical_axis() {
        let f = Foveation::new(0.4, &quest3(), PANEL).unwrap();
        // Left eye: the axis is 62% across (it sees further left than right), 40% down.
        let mid_x = (f.start[0][0][0] as f32 + f.band[0] as f32 / 2.0) / PANEL.0 as f32;
        let mid_y = (f.start[0][1][0] as f32 + f.band[1] as f32 / 2.0) / PANEL.1 as f32;
        assert!((mid_x - 0.621).abs() < 0.01 && (mid_y - 0.403).abs() < 0.01, "{mid_x} {mid_y}");
        // The right eye mirrors it.
        assert_eq!(f.start[1][0][0], PANEL.0 - f.start[0][0][0] - f.band[0]);
        // A band that would cross the image edge is moved inward.
        let wide = Foveation::new(0.9, &quest3(), PANEL).unwrap();
        assert!(wide.start[0][0][0] + wide.band[0] <= PANEL.0);
    }

    #[test]
    fn falloff_is_round_around_the_optical_axis() {
        let fov = quest3();
        let f = Foveation::new(0.4, &fov, PANEL).unwrap();
        for eye in 0..2 {
            let fv = &fov[eye];
            let (l, r, u, d) = (fv.angle_left.tan(), fv.angle_right.tan(), fv.angle_up.tan(), fv.angle_down.tan());
            // The encoded position seen at tangents (tx, ty) from the axis.
            let at = |tx: f32, ty: f32| f.to_coded(eye, [(tx - l) / (r - l), (u - ty) / (u - d)]);
            assert_eq!(f.falloff(&fov, eye, at(0.0, 0.0)), 0.0);
            // Equal angles in any direction give equal values: a circle, not a rectangle.
            let ring = [(0.6, 0.0), (-0.6, 0.0), (0.0, 0.6), (0.0, -0.6), (0.4243, 0.4243), (-0.4243, -0.4243)]
                .map(|(x, y)| f.falloff(&fov, eye, at(x, y)));
            assert!(ring[0] > 0.0 && ring.iter().all(|v| (v - ring[0]).abs() < 0.01), "{ring:?}");
            // Rises outward to 1 at the far edges and corners.
            let ray: Vec<f32> = (0..=10).map(|i| f.falloff(&fov, eye, at(0.0, -0.14 * i as f32))).collect();
            assert!(ray.windows(2).all(|w| w[1] >= w[0]), "{ray:?}");
            assert_eq!(f.falloff(&fov, eye, [0.0, 1.0]), 1.0);
            assert_eq!(f.falloff(&fov, eye, [1.0, 0.0]), 1.0);
        }
    }

    #[test]
    fn smaller_band_encodes_fewer_pixels() {
        let size = |fraction| Foveation::new(fraction, &quest3(), PANEL).unwrap().coded_eye_size();
        // 0.5 + 0.5 * 0.4 = 0.7 of each axis: about half the pixels.
        let (w, h) = size(0.5);
        assert!((1440..1460).contains(&w) && (1540..1560).contains(&h), "{w}x{h}");
        let (w20, h20) = size(0.2);
        assert!(w20 < w && h20 < h, "{w20}x{h20}");
    }
}
