//! Signed-distance rasteriser: exact anti-aliased capsules, rings, lines and soft shadows written
//! straight into a premultiplied RGBA8 pixmap. Used for everything on the hot path so a frame costs
//! a few hundred microseconds and needs no path tessellation.
//!
//! All geometry is in *logical* pixels (the 208x120 design space); `scale` maps to device pixels.

use super::theme::Rgba;
use tiny_skia::Pixmap;

/// Logical rectangle (left, top, right, bottom).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub l: f32,
    pub t: f32,
    pub r: f32,
    pub b: f32,
}

impl Rect {
    pub fn new(l: f32, t: f32, r: f32, b: f32) -> Self {
        Rect { l, t, r, b }
    }
    pub fn from_xywh(x: f32, y: f32, w: f32, h: f32) -> Self {
        Rect { l: x, t: y, r: x + w, b: y + h }
    }
    pub fn w(&self) -> f32 {
        self.r - self.l
    }
    pub fn h(&self) -> f32 {
        self.b - self.t
    }
    pub fn cx(&self) -> f32 {
        (self.l + self.r) * 0.5
    }
    pub fn cy(&self) -> f32 {
        (self.t + self.b) * 0.5
    }
    pub fn inflate(&self, d: f32) -> Rect {
        Rect { l: self.l - d, t: self.t - d, r: self.r + d, b: self.b + d }
    }
    pub fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.l && x < self.r && y >= self.t && y < self.b
    }
    pub fn offset(&self, dx: f32, dy: f32) -> Rect {
        Rect { l: self.l + dx, t: self.t + dy, r: self.r + dx, b: self.b + dy }
    }
}

/// Device-pixel rectangle `[x0, x1) x [y0, y1)` (a dirty region of the frame buffer).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DevRect {
    pub x0: i32,
    pub y0: i32,
    pub x1: i32,
    pub y1: i32,
}

impl DevRect {
    /// The whole `w` x `h` buffer.
    pub fn full(w: i32, h: i32) -> DevRect {
        DevRect { x0: 0, y0: 0, x1: w, y1: h }
    }

    /// Device pixels that a logical rect can touch when drawn with the SDF fills (same rounding as
    /// `Canvas`: floor/ceil plus one pixel of slack), clipped to the `w` x `h` buffer.
    pub fn of(r: Rect, scale: f32, w: i32, h: i32) -> DevRect {
        DevRect {
            x0: ((r.l * scale).floor() as i32).clamp(0, w),
            y0: ((r.t * scale).floor() as i32).clamp(0, h),
            x1: ((r.r * scale).ceil() as i32 + 1).clamp(0, w),
            y1: ((r.b * scale).ceil() as i32 + 1).clamp(0, h),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.x1 <= self.x0 || self.y1 <= self.y0
    }

    pub fn is_full(&self, w: i32, h: i32) -> bool {
        *self == DevRect::full(w, h)
    }

    pub fn width(&self) -> i32 {
        (self.x1 - self.x0).max(0)
    }

    #[cfg(test)]
    pub fn height(&self) -> i32 {
        (self.y1 - self.y0).max(0)
    }

    #[cfg(test)]
    pub fn area(&self) -> i64 {
        self.width() as i64 * self.height() as i64
    }

    #[cfg(test)]
    pub fn contains_pixel(&self, x: i32, y: i32) -> bool {
        x >= self.x0 && x < self.x1 && y >= self.y0 && y < self.y1
    }
}

/// Byte range of row `y`, columns `r.x0..r.x1` in a `w`-wide RGBA8 buffer.
#[inline]
fn row_span(w: i32, y: i32, r: DevRect) -> std::ops::Range<usize> {
    let base = (y as usize * w as usize + r.x0 as usize) * 4;
    base..base + r.width() as usize * 4
}

/// `dst[r] = src[r]` for two buffers of the same width.
pub fn copy_rect(dst: &mut [u8], src: &[u8], w: i32, r: DevRect) {
    for y in r.y0..r.y1 {
        let s = row_span(w, y, r);
        dst[s.clone()].copy_from_slice(&src[s]);
    }
}

/// Zero the rect.
pub fn clear_rect(dst: &mut [u8], w: i32, r: DevRect) {
    for y in r.y0..r.y1 {
        dst[row_span(w, y, r)].fill(0);
    }
}

/// `dst[r] = top over under` (premultiplied source-over) for three buffers of the same width.
pub fn over_rect(dst: &mut [u8], top: &[u8], under: &[u8], w: i32, r: DevRect) {
    for y in r.y0..r.y1 {
        let s = row_span(w, y, r);
        let (d, t, u) = (&mut dst[s.clone()], &top[s.clone()], &under[s]);
        for ((d, t), u) in d.chunks_exact_mut(4).zip(t.chunks_exact(4)).zip(u.chunks_exact(4)) {
            let inv = 255 - t[3] as u32;
            if inv == 0 {
                d.copy_from_slice(t);
            } else {
                for c in 0..4 {
                    d[c] = (t[c] as u32 + (u[c] as u32 * inv + 127) / 255).min(255) as u8;
                }
            }
        }
    }
}

/// `dst[r] = top over dst[r]` (premultiplied source-over, in place) for two buffers of the same width.
pub fn over_in_place(dst: &mut [u8], top: &[u8], w: i32, r: DevRect) {
    for y in r.y0..r.y1 {
        let s = row_span(w, y, r);
        for (d, t) in dst[s.clone()].chunks_exact_mut(4).zip(top[s].chunks_exact(4)) {
            let inv = 255 - t[3] as u32;
            if inv == 0 {
                d.copy_from_slice(t);
            } else if t[3] != 0 {
                for c in 0..4 {
                    d[c] = (t[c] as u32 + (d[c] as u32 * inv + 127) / 255).min(255) as u8;
                }
            }
        }
    }
}

/// `true` when any pixel of `r` has non-zero alpha.
pub fn any_alpha(buf: &[u8], w: i32, r: DevRect) -> bool {
    (r.y0..r.y1).any(|y| buf[row_span(w, y, r)].chunks_exact(4).any(|p| p[3] != 0))
}

/// Coverage of a pixel whose centre is `d` device pixels from the edge (negative = inside).
#[inline]
pub fn cov(d_dev: f32) -> f32 {
    (0.5 - d_dev).clamp(0.0, 1.0)
}

/// Signed distance to a rounded box centred at (cx, cy) with half extents (hw, hh) and radius r.
#[inline]
pub fn sd_round_box(px: f32, py: f32, cx: f32, cy: f32, hw: f32, hh: f32, r: f32) -> f32 {
    let rr = r.min(hw.min(hh));
    let qx = (px - cx).abs() - hw + rr;
    let qy = (py - cy).abs() - hh + rr;
    // sqrt, not hypot: the libm hypot guards against overflow and costs several times more, and
    // this runs per pixel (several times in a glass pixel)
    let (ox, oy) = (qx.max(0.0), qy.max(0.0));
    let outside = (ox * ox + oy * oy).sqrt();
    let inside = qx.max(qy).min(0.0);
    outside + inside - rr
}

/// Capsule (fully rounded box) of size `w` x `h` centred at (cx, cy).
#[inline]
pub fn sd_capsule(px: f32, py: f32, cx: f32, cy: f32, w: f32, h: f32) -> f32 {
    let hw = w * 0.5;
    let hh = h * 0.5;
    sd_round_box(px, py, cx, cy, hw, hh, hh.min(hw))
}

/// Distance to a line segment (no radius).
#[inline]
pub fn sd_segment(px: f32, py: f32, ax: f32, ay: f32, bx: f32, by: f32) -> f32 {
    let pax = px - ax;
    let pay = py - ay;
    let bax = bx - ax;
    let bay = by - ay;
    let len2 = bax * bax + bay * bay;
    let h = if len2 > 0.0 { ((pax * bax + pay * bay) / len2).clamp(0.0, 1.0) } else { 0.0 };
    let (dx, dy) = (pax - bax * h, pay - bay * h);
    (dx * dx + dy * dy).sqrt()
}

/// erf (Abramowitz-Stegun 7.1.26, |err| < 1.5e-7).
#[inline]
pub fn erf(x: f32) -> f32 {
    let s = x.signum();
    let x = x.abs();
    let t = 1.0 / (1.0 + 0.327_591_1 * x);
    let y = 1.0
        - (((((1.061_405_4 * t - 1.453_152_1) * t) + 1.421_413_8) * t - 0.284_496_72) * t + 0.254_829_6)
            * t
            * (-x * x).exp();
    s * y
}

/// Standard normal CDF.
#[inline]
pub fn norm_cdf(x: f32) -> f32 {
    0.5 * (1.0 + erf(x * std::f32::consts::FRAC_1_SQRT_2))
}

/// [`norm_cdf`] by table lookup on -3..3 (linear between 1/128 steps: error below 3e-6, far under
/// one 8-bit step), for the per-pixel shadow loops; `erf` costs an `exp` per call.
#[inline]
pub fn norm_cdf_fast(z: f32) -> f32 {
    const STEPS_PER_UNIT: f32 = 128.0;
    const N: usize = 6 * 128 + 2;
    static TABLE: std::sync::OnceLock<[f32; N]> = std::sync::OnceLock::new();
    let t = TABLE.get_or_init(|| {
        let mut t = [0.0f32; N];
        for (i, v) in t.iter_mut().enumerate() {
            *v = norm_cdf(i as f32 / STEPS_PER_UNIT - 3.0);
        }
        t
    });
    let f = (z.clamp(-3.0, 3.0) + 3.0) * STEPS_PER_UNIT;
    let i = f as usize;
    let a = f - i as f32;
    t[i] + (t[i + 1] - t[i]) * a
}

/// Mutable view over a premultiplied RGBA8 pixmap plus the logical->device scale.
pub struct Canvas<'a> {
    pub data: &'a mut [u8],
    pub w: i32,
    pub h: i32,
    pub scale: f32,
}

impl<'a> Canvas<'a> {
    pub fn new(pm: &'a mut Pixmap, scale: f32) -> Self {
        let w = pm.width() as i32;
        let h = pm.height() as i32;
        Canvas { data: pm.data_mut(), w, h, scale }
    }

    /// Source-over a premultiplied float colour onto pixel (x, y).
    #[inline]
    pub fn blend(&mut self, x: i32, y: i32, src: [f32; 4]) {
        if x < 0 || y < 0 || x >= self.w || y >= self.h || src[3] <= 0.0 {
            return;
        }
        let i = ((y * self.w + x) * 4) as usize;
        let inv = 1.0 - src[3];
        let d = &mut self.data[i..i + 4];
        for c in 0..4 {
            let v = src[c] * 255.0 + d[c] as f32 * inv;
            d[c] = (v + 0.5).min(255.0) as u8;
        }
    }

    /// Device pixel range covering a logical rect (clipped to the canvas).
    fn dev_range(&self, r: Rect) -> (i32, i32, i32, i32) {
        let d = DevRect::of(r, self.scale, self.w, self.h);
        (d.x0, d.y0, d.x1, d.y1)
    }

    /// Fill `color * alpha` wherever `sdf` (logical-px distance, negative inside) is inside.
    pub fn fill_sdf(&mut self, bbox: Rect, color: Rgba, alpha: f32, sdf: impl Fn(f32, f32) -> f32) {
        let a = color.a * alpha;
        if a <= 0.0 {
            return;
        }
        let s = self.scale;
        let inv = 1.0 / s;
        let pm = [color.r * a, color.g * a, color.b * a, a];
        let (x0, y0, x1, y1) = self.dev_range(bbox);
        for y in y0..y1 {
            let ly = (y as f32 + 0.5) * inv;
            for x in x0..x1 {
                let lx = (x as f32 + 0.5) * inv;
                let c = cov(sdf(lx, ly) * s);
                if c > 0.0 {
                    self.blend(x, y, [pm[0] * c, pm[1] * c, pm[2] * c, pm[3] * c]);
                }
            }
        }
    }

    /// General shader: `shade(lx, ly, coverage) -> premultiplied colour (already scaled by coverage)`.
    pub fn shade_sdf(
        &mut self,
        bbox: Rect,
        sdf: impl Fn(f32, f32) -> f32,
        mut shade: impl FnMut(f32, f32, f32) -> [f32; 4],
    ) {
        let s = self.scale;
        let inv = 1.0 / s;
        let (x0, y0, x1, y1) = self.dev_range(bbox);
        for y in y0..y1 {
            let ly = (y as f32 + 0.5) * inv;
            for x in x0..x1 {
                let lx = (x as f32 + 0.5) * inv;
                let c = cov(sdf(lx, ly) * s);
                if c > 0.0 {
                    let px = shade(lx, ly, c);
                    self.blend(x, y, px);
                }
            }
        }
    }

    /// Gaussian-blurred shadow of an SDF shape (`CSS box-shadow`): `offset_y`, `blur` (CSS blur radius,
    /// sigma = blur/2), `spread`, black-ish `color` with its alpha. Exact for shapes much larger than
    /// the blur, which holds for the pill/skin chrome.
    pub fn shadow_sdf(
        &mut self,
        bbox: Rect,
        offset_y: f32,
        blur: f32,
        spread: f32,
        color: Rgba,
        alpha: f32,
        sdf: impl Fn(f32, f32) -> f32,
    ) {
        let a = color.a * alpha;
        if a <= 0.001 {
            return;
        }
        let sigma = (blur * 0.5).max(0.01);
        let reach = spread + sigma * 3.0;
        let bb = Rect::new(bbox.l - reach, bbox.t - reach + offset_y.min(0.0), bbox.r + reach, bbox.b + reach + offset_y.max(0.0));
        let s = self.scale;
        let inv = 1.0 / s;
        let (x0, y0, x1, y1) = self.dev_range(bb);
        for y in y0..y1 {
            let ly = (y as f32 + 0.5) * inv;
            for x in x0..x1 {
                let lx = (x as f32 + 0.5) * inv;
                let d = sdf(lx, ly - offset_y) - spread;
                if d > sigma * 3.0 {
                    continue;
                }
                let k = if d < -sigma * 3.0 { 1.0 } else { norm_cdf_fast(-d / sigma) };
                let pa = a * k;
                if pa > 0.0 {
                    self.blend(x, y, [color.r * pa, color.g * pa, color.b * pa, pa]);
                }
            }
        }
    }

    /// Soft shadow of a (rounded) rectangle via separable erf — exact for plain boxes and a good
    /// match for large blurs (error card, tooltip). `radius` rounds the box corner of the *shadow*
    /// shape only coarsely (corners are blended toward the inscribed box).
    pub fn shadow_rect(&mut self, rect: Rect, radius: f32, offset: (f32, f32), blur: f32, spread: f32, color: Rgba, alpha: f32) {
        let a = color.a * alpha;
        if a <= 0.001 {
            return;
        }
        let sigma = (blur * 0.5).max(0.01);
        let r = rect.inflate(spread).offset(offset.0, offset.1);
        let rad = (radius + spread).max(0.0);
        let reach = sigma * 3.0;
        let (x0, y0, x1, y1) = self.dev_range(r.inflate(reach));
        let s = self.scale;
        let inv = 1.0 / s;
        let k = std::f32::consts::FRAC_1_SQRT_2 / sigma;
        for y in y0..y1 {
            let ly = (y as f32 + 0.5) * inv;
            let fy = 0.5 * (erf((ly - r.t) * k) - erf((ly - r.b) * k));
            if fy <= 0.0005 {
                continue;
            }
            for x in x0..x1 {
                let lx = (x as f32 + 0.5) * inv;
                let fx = 0.5 * (erf((lx - r.l) * k) - erf((lx - r.r) * k));
                let mut f = fx * fy;
                if rad > 0.5 {
                    // fade the four corners toward the rounded outline
                    let d = sd_round_box(lx, ly, r.cx(), r.cy(), r.w() * 0.5, r.h() * 0.5, rad);
                    if d > 0.0 {
                        // outside the rounded outline the box product over-estimates; blend with
                        // the 1-D SDF profile
                        let g = norm_cdf(-d / sigma);
                        f = f.min(g.max(0.0));
                    }
                }
                let pa = a * f;
                if pa > 0.0 {
                    self.blend(x, y, [color.r * pa, color.g * pa, color.b * pa, pa]);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capsule_sdf_values() {
        // centre of a 40x40 circle
        assert!((sd_capsule(20.0, 20.0, 20.0, 20.0, 40.0, 40.0) + 20.0).abs() < 1e-4);
        // on the left edge of a 98x36 capsule
        assert!(sd_capsule(0.0, 18.0, 49.0, 18.0, 98.0, 36.0).abs() < 1e-4);
        // 4 px outside the straight top edge
        assert!((sd_capsule(49.0, -4.0, 49.0, 18.0, 98.0, 36.0) - 4.0).abs() < 1e-4);
    }

    #[test]
    fn erf_and_cdf() {
        assert!(erf(0.0).abs() < 1e-6);
        assert!((erf(1.0) - 0.842_700_8).abs() < 1e-5);
        assert!((norm_cdf(0.0) - 0.5).abs() < 1e-6);
        assert!(norm_cdf(3.0) > 0.998);
    }

    #[test]
    fn fill_sdf_antialiases_edge() {
        let mut pm = Pixmap::new(20, 20).unwrap();
        {
            let mut c = Canvas::new(&mut pm, 1.0);
            c.fill_sdf(Rect::new(0.0, 0.0, 20.0, 20.0), Rgba::hex(0xffffff), 1.0, |x, _y| x - 10.25);
        }
        // pixel centre 9.5 -> d=-0.75 -> full; 10.5 -> d=0.25 -> 25%
        assert_eq!(pm.pixel(9, 5).unwrap().alpha(), 255);
        let a = pm.pixel(10, 5).unwrap().alpha();
        assert!((a as i32 - 64).abs() <= 2, "alpha {a}");
        assert_eq!(pm.pixel(11, 5).unwrap().alpha(), 0);
    }

    #[test]
    fn dev_rect_matches_canvas_rounding() {
        let r = Rect::new(10.2, 5.7, 30.1, 20.0);
        let d = DevRect::of(r, 1.5, 100, 60);
        assert_eq!(d, DevRect { x0: 15, y0: 8, x1: 47, y1: 31 });
        assert_eq!(DevRect::of(Rect::new(-9.0, -9.0, 500.0, 500.0), 1.0, 100, 60), DevRect::full(100, 60));
        assert!(DevRect::of(Rect::new(-9.0, -9.0, -3.0, -3.0), 1.0, 100, 60).is_empty());
    }

    #[test]
    fn rect_helpers_touch_only_the_rect() {
        let (w, h) = (6, 4);
        let mut a = vec![10u8; (w * h * 4) as usize];
        let b = vec![200u8; (w * h * 4) as usize];
        let r = DevRect { x0: 1, y0: 1, x1: 3, y1: 3 };
        copy_rect(&mut a, &b, w, r);
        for y in 0..h {
            for x in 0..w {
                let v = a[((y * w + x) * 4) as usize];
                assert_eq!(v, if r.contains_pixel(x, y) { 200 } else { 10 }, "({x},{y})");
            }
        }
        clear_rect(&mut a, w, r);
        assert_eq!(a[((w + 1) * 4) as usize], 0);
        assert_eq!(a[0], 10);
        // source-over: an opaque top wins, a translucent one lets the backdrop through
        let mut out = vec![0u8; (w * h * 4) as usize];
        let mut top = vec![0u8; (w * h * 4) as usize];
        top[0..4].copy_from_slice(&[9, 9, 9, 255]);
        top[4..8].copy_from_slice(&[50, 0, 0, 128]);
        let under = vec![100u8; (w * h * 4) as usize];
        over_rect(&mut out, &top, &under, w, DevRect::full(w, h));
        assert_eq!(&out[0..4], &[9, 9, 9, 255]);
        assert_eq!(&out[8..12], &[100, 100, 100, 100]);
        assert_eq!(out[4], (50 + (100u32 * 127 + 127) / 255) as u8);
    }

    #[test]
    fn scale_maps_logical_to_device() {
        let mut pm = Pixmap::new(40, 40).unwrap();
        {
            let mut c = Canvas::new(&mut pm, 2.0);
            c.fill_sdf(Rect::new(0.0, 0.0, 20.0, 20.0), Rgba::hex(0xffffff), 1.0, |x, y| sd_capsule(x, y, 10.0, 10.0, 10.0, 10.0));
        }
        assert_eq!(pm.pixel(20, 20).unwrap().alpha(), 255);
        assert_eq!(pm.pixel(2, 2).unwrap().alpha(), 0);
    }
}
