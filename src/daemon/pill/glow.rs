//! The "Signal" glow of the thinking state: a conic gradient ring (analytic, per pixel), a breathing
//! radial halo and a comet bloom. Halo and comet are blurred once into sprites (per DPI scale) and
//! then only scaled/rotated each frame.

use super::anim::{signal_angle_deg, signal_breathe};
use super::raster::{Canvas, Rect, cov, sd_capsule};
use super::theme::{Palette, Rgba, SIGNAL_BRIGHT, SIGNAL_CORE, SIGNAL_DEEP, SIGNAL_HEAD};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use tiny_skia::{FilterQuality, Pixmap, PixmapPaint, Transform};

/// Logical radius of the halo/comet box on the 40x40 circle: inset -4 => 24.
pub const GLOW_RADIUS: f32 = 24.0;
pub const HALO_BLUR: f32 = 5.0;
pub const COMET_BLUR: f32 = 4.0;
/// Margin around the circle kept in the sprites so the blur tails are not cut (logical px).
const SPRITE_PAD: f32 = 16.0;
/// Ring thickness (CSS padding) and outset from the pill box.
pub const RING_THICKNESS: f32 = 2.5;
pub const RING_OUTSET: f32 = 2.0;

type P = [f32; 4];

fn pm(c: Rgba, a: f32) -> P {
    [c.r * a, c.g * a, c.b * a, a]
}

fn mix(a: P, b: P, t: f32) -> P {
    [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t, a[2] + (b[2] - a[2]) * t, a[3] + (b[3] - a[3]) * t]
}

fn gradient(stops: &[(f32, P)], t: f32) -> P {
    if t <= stops[0].0 {
        return stops[0].1;
    }
    for w in stops.windows(2) {
        if t <= w[1].0 {
            let span = (w[1].0 - w[0].0).max(1e-6);
            return mix(w[0].1, w[1].1, (t - w[0].0) / span);
        }
    }
    stops[stops.len() - 1].1
}

/// Conic ring gradient (`from <angle>`: core@25% 0%, deep@55% 45%, core 75%, bright 94%, head 100%).
pub fn ring_color(frac: f32) -> P {
    let stops = [
        (0.0, pm(SIGNAL_CORE, 0.25)),
        (0.45, pm(SIGNAL_DEEP, 0.55)),
        (0.75, pm(SIGNAL_CORE, 1.0)),
        (0.94, pm(SIGNAL_BRIGHT, 1.0)),
        (1.0, pm(SIGNAL_HEAD, 1.0)),
    ];
    gradient(&stops, frac)
}

/// Comet conic gradient (transparent 0-55%, core@70% 82%, bright 100%).
pub fn comet_color(frac: f32) -> P {
    let stops = [
        (0.0, [0.0; 4]),
        (0.55, [0.0; 4]),
        (0.82, pm(SIGNAL_CORE, 0.7)),
        (1.0, pm(SIGNAL_BRIGHT, 1.0)),
    ];
    gradient(&stops, frac)
}

/// Halo radial gradient over `r / closest-side` (core@90% 45%, deep@70% 75%, transparent).
pub fn halo_color(t: f32) -> P {
    let stops = [(0.0, pm(SIGNAL_CORE, 0.9)), (0.45, pm(SIGNAL_CORE, 0.9)), (0.75, pm(SIGNAL_DEEP, 0.7)), (1.0, [0.0; 4])];
    if t >= 1.0 { [0.0; 4] } else { gradient(&stops, t) }
}

/// Clockwise angle from 12 o'clock, 0..360.
#[inline]
fn angle_from_top(dx: f32, dy: f32) -> f32 {
    let a = dx.atan2(-dy).to_degrees();
    if a < 0.0 { a + 360.0 } else { a }
}

/// Separable gaussian blur on premultiplied f32 RGBA.
fn gaussian_blur(buf: &mut [f32], w: usize, h: usize, sigma: f32) {
    if sigma < 0.3 {
        return;
    }
    let radius = (sigma * 3.0).ceil() as i32;
    let mut k: Vec<f32> = (-radius..=radius).map(|i| (-(i * i) as f32 / (2.0 * sigma * sigma)).exp()).collect();
    let sum: f32 = k.iter().sum();
    k.iter_mut().for_each(|v| *v /= sum);
    let mut tmp = vec![0.0f32; buf.len()];
    // horizontal
    for y in 0..h {
        for x in 0..w {
            let mut acc = [0.0f32; 4];
            for (ki, kv) in k.iter().enumerate() {
                let xx = x as i32 + ki as i32 - radius;
                if xx < 0 || xx >= w as i32 {
                    continue;
                }
                let i = (y * w + xx as usize) * 4;
                for c in 0..4 {
                    acc[c] += buf[i + c] * kv;
                }
            }
            let o = (y * w + x) * 4;
            tmp[o..o + 4].copy_from_slice(&acc);
        }
    }
    // vertical
    for y in 0..h {
        for x in 0..w {
            let mut acc = [0.0f32; 4];
            for (ki, kv) in k.iter().enumerate() {
                let yy = y as i32 + ki as i32 - radius;
                if yy < 0 || yy >= h as i32 {
                    continue;
                }
                let i = (yy as usize * w + x) * 4;
                for c in 0..4 {
                    acc[c] += tmp[i + c] * kv;
                }
            }
            let o = (y * w + x) * 4;
            buf[o..o + 4].copy_from_slice(&acc);
        }
    }
}

/// Pre-blurred halo and comet for one device scale.
pub struct Sprites {
    pub halo: Pixmap,
    pub comet: Pixmap,
    /// Sprite centre in device px (both sprites share the size).
    pub center: f32,
}

impl Sprites {
    fn build(scale: f32) -> Option<Sprites> {
        let side_logical = GLOW_RADIUS * 2.0 + SPRITE_PAD * 2.0;
        let side = ((side_logical * scale).ceil() as usize).max(1);
        let center = side as f32 * 0.5;
        let r_dev = GLOW_RADIUS * scale;
        let make = |blur: f32, color: &dyn Fn(f32, f32, f32) -> P| -> Option<Pixmap> {
            let mut buf = vec![0.0f32; side * side * 4];
            for y in 0..side {
                for x in 0..side {
                    let dx = x as f32 + 0.5 - center;
                    let dy = y as f32 + 0.5 - center;
                    let d = dx.hypot(dy);
                    // circle clip with 1px AA
                    let c = cov(d - r_dev);
                    if c <= 0.0 {
                        continue;
                    }
                    let col = color(d / r_dev, dx, dy);
                    let o = (y * side + x) * 4;
                    for k in 0..4 {
                        buf[o + k] = col[k] * c;
                    }
                }
            }
            gaussian_blur(&mut buf, side, side, blur * scale);
            let mut pmx = Pixmap::new(side as u32, side as u32)?;
            for (dst, src) in pmx.data_mut().chunks_exact_mut(4).zip(buf.chunks_exact(4)) {
                let a = src[3].clamp(0.0, 1.0);
                dst[0] = (src[0].min(a) * 255.0 + 0.5) as u8;
                dst[1] = (src[1].min(a) * 255.0 + 0.5) as u8;
                dst[2] = (src[2].min(a) * 255.0 + 0.5) as u8;
                dst[3] = (a * 255.0 + 0.5) as u8;
            }
            Some(pmx)
        };
        let halo = make(HALO_BLUR, &|t, _dx, _dy| halo_color(t))?;
        let comet = make(COMET_BLUR, &|_t, dx, dy| comet_color(angle_from_top(dx, dy) / 360.0))?;
        Some(Sprites { halo, comet, center })
    }

    /// Cached sprites for `scale` (built once, normally at start-up).
    /// `None` when no sprite of that size can exist (the glow is then skipped).
    pub fn get(scale: f32) -> Option<Arc<Sprites>> {
        static C: OnceLock<Mutex<HashMap<u32, Arc<Sprites>>>> = OnceLock::new();
        let key = (super::model::sane_scale(scale) * 64.0).round() as u32;
        let mut m = C.get_or_init(|| Mutex::new(HashMap::new())).lock().unwrap_or_else(|e| e.into_inner());
        if let Some(s) = m.get(&key) {
            return Some(s.clone());
        }
        if m.len() > 4 {
            m.clear();
        }
        let s = Arc::new(Sprites::build(key as f32 / 64.0)?);
        m.insert(key, s.clone());
        Some(s)
    }
}

/// Everything the glow needs for one frame.
pub struct GlowFrame {
    /// Pill box in logical px (the glow follows its current size).
    pub pill: Rect,
    /// Container opacity (the 220 ms fade).
    pub opacity: f32,
    /// ms since the glow became active, or `None` when not active (animations stopped: angle 0,
    /// halo at its base opacity/scale, like CSS after the animation is removed).
    pub elapsed: Option<f64>,
}

/// Draw halo, ring and comet *behind* the pill (call before drawing the pill body).
pub fn draw(canvas: &mut Canvas, pal: &Palette, g: &GlowFrame, sprites: &Sprites) {
    if g.opacity <= 0.002 {
        return;
    }
    let (angle, breathe_op, breathe_scale) = match g.elapsed {
        Some(e) => {
            let (o, s) = signal_breathe(e);
            (signal_angle_deg(e), o, s)
        }
        None => (0.0, 1.0, 1.0),
    };
    let s = canvas.scale;
    let cx = g.pill.cx() * s;
    let cy = g.pill.cy() * s;
    let box_w = (g.pill.w() + 8.0) / (GLOW_RADIUS * 2.0);
    let box_h = (g.pill.h() + 8.0) / (GLOW_RADIUS * 2.0);

    // halo: scale about the centre, breathe opacity
    {
        let sx = box_w * breathe_scale;
        let sy = box_h * breathe_scale;
        let t = Transform::from_translate(-sprites.center, -sprites.center)
            .post_scale(sx, sy)
            .post_translate(cx, cy);
        blit(canvas, &sprites.halo, t, pal.halo_opacity * breathe_op * g.opacity);
    }
    // ring
    draw_ring(canvas, g.pill, angle, g.opacity);
    // comet: rotates with the orbit
    {
        let t = Transform::from_translate(-sprites.center, -sprites.center)
            .post_scale(box_w, box_h)
            .post_rotate(angle)
            .post_translate(cx, cy);
        blit(canvas, &sprites.comet, t, pal.comet_opacity * g.opacity);
    }
}

fn blit(canvas: &mut Canvas, sprite: &Pixmap, t: Transform, opacity: f32) {
    if opacity <= 0.002 {
        return;
    }
    let (w, h) = (canvas.w as u32, canvas.h as u32);
    let Some(mut dst) = tiny_skia::PixmapMut::from_bytes(&mut *canvas.data, w, h) else { return };
    let paint = PixmapPaint { opacity: opacity.clamp(0.0, 1.0), blend_mode: tiny_skia::BlendMode::SourceOver, quality: FilterQuality::Bilinear };
    dst.draw_pixmap(0, 0, sprite.as_ref(), &paint, t, None);
}

/// The 2.5 px conic ring at inset -2 (only its outer ~2 px peek out from under the pill).
pub fn draw_ring(canvas: &mut Canvas, pill: Rect, angle_deg: f32, opacity: f32) {
    if opacity <= 0.002 {
        return;
    }
    let (cx, cy) = (pill.cx(), pill.cy());
    let ow = pill.w() + RING_OUTSET * 2.0;
    let oh = pill.h() + RING_OUTSET * 2.0;
    let iw = ow - RING_THICKNESS * 2.0;
    let ih = oh - RING_THICKNESS * 2.0;
    let s = canvas.scale;
    let bbox = Rect::new(cx - ow * 0.5, cy - oh * 0.5, cx + ow * 0.5, cy + oh * 0.5);
    canvas.shade_sdf(
        bbox,
        |x, y| sd_capsule(x, y, cx, cy, ow, oh),
        |x, y, c_out| {
            let d_in = sd_capsule(x, y, cx, cy, iw, ih);
            let hole = cov(d_in * s);
            let k = c_out * (1.0 - hole) * opacity;
            if k <= 0.0 {
                return [0.0; 4];
            }
            let a = angle_from_top(x - cx, y - cy);
            let frac = ((a - angle_deg) / 360.0).rem_euclid(1.0);
            let col = ring_color(frac);
            [col[0] * k, col[1] * k, col[2] * k, col[3] * k]
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gradients_hit_their_stops() {
        let c = ring_color(0.75);
        assert!((c[3] - 1.0).abs() < 1e-6 && (c[2] - SIGNAL_CORE.b).abs() < 1e-5);
        let c = ring_color(0.0);
        assert!((c[3] - 0.25).abs() < 1e-6);
        let c = ring_color(0.999);
        assert!(c[0] > 0.8, "head is near white: {c:?}");
        assert_eq!(comet_color(0.3), [0.0; 4]);
        assert!((comet_color(0.82)[3] - 0.7).abs() < 1e-6);
        assert!((halo_color(0.2)[3] - 0.9).abs() < 1e-6);
        assert!((halo_color(0.75)[3] - 0.7).abs() < 1e-6);
        assert_eq!(halo_color(1.0), [0.0; 4]);
    }

    #[test]
    fn angle_from_top_is_clockwise() {
        assert!((angle_from_top(0.0, -1.0) - 0.0).abs() < 1e-3);
        assert!((angle_from_top(1.0, 0.0) - 90.0).abs() < 1e-3);
        assert!((angle_from_top(0.0, 1.0) - 180.0).abs() < 1e-3);
        assert!((angle_from_top(-1.0, 0.0) - 270.0).abs() < 1e-3);
    }

    #[test]
    fn sprites_are_blurred_and_cached() {
        let a = Sprites::get(1.0).expect("sprites");
        let b = Sprites::get(1.0).expect("sprites");
        assert!(Arc::ptr_eq(&a, &b));
        // the halo bleeds beyond the unblurred radius
        let c = a.center as u32;
        let outside = a.halo.pixel(c + GLOW_RADIUS as u32 + 4, c).unwrap().alpha();
        assert!(outside > 0);
        // centre is dense
        assert!(a.halo.pixel(c, c).unwrap().alpha() > 150);
    }

    #[test]
    fn ring_paints_only_a_thin_annulus() {
        let mut pm = Pixmap::new(80, 80).unwrap();
        {
            let mut cv = Canvas::new(&mut pm, 1.0);
            draw_ring(&mut cv, Rect::from_xywh(20.0, 20.0, 40.0, 40.0), 0.0, 1.0);
        }
        // centre untouched, ring at radius ~21 painted
        assert_eq!(pm.pixel(40, 40).unwrap().alpha(), 0);
        assert!(pm.pixel(19, 40).unwrap().alpha() > 200);
        assert_eq!(pm.pixel(40, 40 - 30).unwrap().alpha(), 0);
    }
}
