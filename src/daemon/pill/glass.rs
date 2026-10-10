//! Liquid glass for the pill surfaces (the `liquid-glass` skill, ported to the software renderer).
//!
//! The web recipe is two layers: CSS paints a near-colourless tint and a polished rim, and a baked
//! displacement map bends the backdrop through the panel's edge. A layered window has no
//! `backdrop-filter`, so this module does both itself:
//!
//! 1. **Backdrop.** The screen rect under the pill (plus the refraction margin) is captured with the
//!    pill excluded (`WDA_EXCLUDEFROMCAPTURE`, see `capture.rs`), box-averaged into cells of about 3
//!    logical px, hashed (an unchanged hash skips everything below), frosted (3-pass box blur ~ a
//!    gaussian of [`FROST_SIGMA`], saturate 125 %) and kept as a [`Frosted`] buffer.
//! 2. **Refraction.** [`displacement`] is the skill's field (`references/refraction.md`, `GLASS_PRESET`,
//!    signed rounded-rect `distPx`, warp off, page = the monitor), transcribed 1:1. Each glass pixel
//!    samples the frosted buffer at its displaced position. Fields of exact rounded boxes are baked
//!    once per (size, radius, page, scale, sub-pixel phase) and reused ([`DispMap`]).
//! 3. **Material.** A frost layer (the skill's modal tier: light in light mode, dark in dark mode,
//!    tuned so the waveform and the logo pass a legibility check over any backdrop), the 6 % tint
//!    gradient, the three inset rim highlights, the 1 px border and the shadow from `liquid-glass.css`.
//!
//! Everything here is pure (no Win32), so the golden renders stay deterministic: with no capture the
//! pill is frosted over a flat neutral backdrop ([`Backdrop::neutral`]).
//!
//! The field formulas are transcribed from `container.js` of dashersw/liquid-glass-js (MIT,
//! (c) 2025 Armagan Amcalar) through the skill's `liquid-glass.directive.ts`.

use super::model::Shadow;
use super::raster::{Canvas, DevRect, Rect, cov, sd_round_box};
use super::theme::Rgba;
use std::cell::RefCell;
use std::hash::Hasher;
use std::rc::Rc;
use std::sync::Arc;

// --- the skill's constants ------------------------------------------------------------------------

/// `GLASS_PRESET` of the skill (distances are exponential falloff rates per pixel from the edge,
/// intensities are fractions of the page texture).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Preset {
    pub edge_intensity: f32,
    pub rim_intensity: f32,
    pub base_intensity: f32,
    pub edge_distance: f32,
    pub rim_distance: f32,
    pub base_distance: f32,
    pub corner_boost: f32,
    pub ripple_effect: f32,
    /// Post-displacement blur of the web filter (std 0.7 px); the frost layer already smooths the
    /// sampled buffer far more, so it is not applied separately.
    pub blur_radius: f32,
    /// Centre distortion; off, as in the skill (it makes content under the middle unreadable).
    pub warp: bool,
}

pub const GLASS_PRESET: Preset = Preset {
    edge_intensity: 0.015,
    rim_intensity: 0.028,
    base_intensity: 0.05,
    edge_distance: 0.5,
    rim_distance: 1.7,
    base_distance: 0.2,
    corner_boost: 0.06,
    ripple_effect: 0.26,
    blur_radius: 2.0,
    warp: false,
};

/// Depth (logical px) beyond which the field is below 0.1 px: the rim falls off as `exp(-1.7 d)` and
/// the edge term as `exp(-0.5 d)`.
const FIELD_REACH: f32 = 14.0;
/// `cornerNorm` (logical px) beyond which the corner boost is below 0.1 px.
const CORNER_REACH: f32 = 24.0;

/// The skill's field: displacement in logical px of the backdrop sample for a pixel `dist_px` inside
/// the shape edge, at normalised box position `(cx, cy)` of a box whose smaller side is `min_dim`,
/// on a page (monitor) of `page` logical px. Positive `x` samples from further right/outside.
pub fn displacement(p: &Preset, dist_px: f32, cx: f32, cy: f32, min_dim: f32, page: (f32, f32)) -> (f32, f32) {
    let edge_fall = (-dist_px * p.edge_distance).exp();
    let rim_fall = (-dist_px * p.rim_distance).exp();
    let base_fall = 1.0 - (-dist_px * p.base_distance).exp();
    let base = if p.warp { base_fall * p.base_intensity } else { 0.0 };
    let total = base + edge_fall * p.edge_intensity + rim_fall * p.rim_intensity;
    // the normal is taken in texcoord space, as in the shader
    let (mut nx, mut ny) = (cx - 0.5, cy - 0.5);
    let len = nx.hypot(ny);
    if len > 0.0 {
        nx /= len;
        ny /= len;
    }
    let corner_norm = cx.min(1.0 - cx).max(cy.min(1.0 - cy)) * min_dim;
    let corner = (-corner_norm * 0.3).exp() * p.corner_boost;
    let ripple = (dist_px / min_dim * 25.0).sin() * p.ripple_effect * rim_fall;
    ((nx * (total + corner) - ny * ripple) * page.0, (ny * (total + corner) + nx * ripple) * page.1)
}

/// Does the field matter at this depth / corner distance (otherwise it is zero within 0.1 px)?
#[inline]
fn field_matters(dist_px: f32, cx: f32, cy: f32, min_dim: f32) -> bool {
    dist_px < FIELD_REACH || cx.min(1.0 - cx).max(cy.min(1.0 - cy)) * min_dim < CORNER_REACH
}

// --- tokens (liquid-glass.css) ---------------------------------------------------------------------

/// The CSS custom properties, per theme, plus the frost layer tuned for the pill.
#[derive(Clone, Copy, Debug)]
pub struct Tokens {
    /// `--glass-tint-*`: white -> 70 % grey at 6 % alpha (11 % lifted).
    pub tint_top: [f32; 3],
    pub tint_bottom: [f32; 3],
    pub tint_alpha: f32,
    pub tint_alpha_hover: f32,
    /// `--glass-rim-*` (white highlights).
    pub rim_top: f32,
    pub rim_hairline: f32,
    pub rim_bottom: f32,
    /// `--glass-border`.
    pub border: Rgba,
    /// The frost layer (modal tier): `rgb` at `alpha` over the refracted backdrop.
    pub frost: [f32; 3],
    pub frost_alpha: f32,
    /// Frost alpha of the error card: 14-16 px text, muted description included, must hold 4.5:1.
    pub card_frost_alpha: f32,
    /// Menu tier wash (`--glass-menu-surface` over the frosted backdrop) used by the tooltip.
    pub menu: [f32; 3],
    pub menu_alpha: f32,
    pub menu_border: Rgba,
    /// Shadow of the pill surfaces at rest / lifted, and of the error card. The skill's
    /// `0 10px 30px` / `0 18px 40px` are scaled to the 12 px the window leaves around the pill, so
    /// the layered window never clips them.
    pub shadow: Shadow,
    pub shadow_lift: Shadow,
    pub card_shadow: Shadow,
}

const WHITE: [f32; 3] = [1.0, 1.0, 1.0];
const GREY70: [f32; 3] = [179.0 / 255.0, 179.0 / 255.0, 179.0 / 255.0];

/// Frost alphas. The skill's modal tier is 0.44 (light) / 0.30 (dark); the pill is 36 px tall and its
/// waveform has to stay legible over *any* backdrop, so the frost is stronger. The numbers keep
/// foreground-vs-glass contrast >= 4.5:1 over pure white, pure black and busy content
/// (`legibility_over_white_black_and_busy_backdrops`).
pub const FROST_ALPHA_LIGHT: f32 = 0.56;
pub const FROST_ALPHA_DARK: f32 = 0.69;
/// The error card frosts harder than the pill: its muted description (#525252 / #8f8f8f) needs 4.5:1.
pub const CARD_FROST_ALPHA_LIGHT: f32 = 0.82;
pub const CARD_FROST_ALPHA_DARK: f32 = 0.95;
/// Gaussian sigma (logical px) of the frost blur.
pub const FROST_SIGMA: f32 = 10.0;
/// `saturate(125%)`.
pub const FROST_SATURATE: f32 = 1.25;

pub fn tokens(dark: bool) -> &'static Tokens {
    static LIGHT: Tokens = Tokens {
        tint_top: WHITE,
        tint_bottom: GREY70,
        tint_alpha: 0.06,
        tint_alpha_hover: 0.11,
        rim_top: 0.55,
        rim_hairline: 0.12,
        rim_bottom: 0.06,
        border: Rgba::new(30.0 / 255.0, 33.0 / 255.0, 45.0 / 255.0, 0.12),
        frost: WHITE,
        frost_alpha: FROST_ALPHA_LIGHT,
        card_frost_alpha: CARD_FROST_ALPHA_LIGHT,
        menu: [246.0 / 255.0, 247.0 / 255.0, 249.0 / 255.0],
        menu_alpha: 0.74,
        menu_border: Rgba::new(30.0 / 255.0, 33.0 / 255.0, 45.0 / 255.0, 0.12),
        shadow: Shadow { dy: 3.0, blur: 9.0, spread: 0.0, alpha: 0.13 },
        shadow_lift: Shadow { dy: 5.0, blur: 12.0, spread: -1.0, alpha: 0.16 },
        card_shadow: Shadow { dy: 6.0, blur: 12.0, spread: -4.0, alpha: 0.16 },
    };
    static DARK: Tokens = Tokens {
        tint_top: WHITE,
        tint_bottom: GREY70,
        tint_alpha: 0.06,
        tint_alpha_hover: 0.11,
        rim_top: 0.50,
        rim_hairline: 0.10,
        rim_bottom: 0.06,
        border: Rgba::new(1.0, 1.0, 1.0, 0.08),
        frost: [14.0 / 255.0, 15.0 / 255.0, 19.0 / 255.0],
        frost_alpha: FROST_ALPHA_DARK,
        card_frost_alpha: CARD_FROST_ALPHA_DARK,
        menu: [22.0 / 255.0, 22.0 / 255.0, 27.0 / 255.0],
        menu_alpha: 0.74,
        menu_border: Rgba::new(1.0, 1.0, 1.0, 0.08),
        shadow: Shadow { dy: 4.0, blur: 10.0, spread: 0.0, alpha: 0.32 },
        shadow_lift: Shadow { dy: 6.0, blur: 13.0, spread: -1.0, alpha: 0.36 },
        card_shadow: Shadow { dy: 6.0, blur: 12.0, spread: -4.0, alpha: 0.40 },
    };
    if dark { &DARK } else { &LIGHT }
}

// --- backdrop --------------------------------------------------------------------------------------

/// 64-bit FNV-1a (cheap change detection, never security).
struct Fnv(u64);

impl Fnv {
    fn new() -> Fnv {
        Fnv(0xcbf2_9ce4_8422_2325)
    }
}

impl Hasher for Fnv {
    fn finish(&self) -> u64 {
        self.0
    }
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 = (self.0 ^ b as u64).wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
}

/// Logical px per cell of the frosted buffer (the frost is far smoother than this).
pub const CELL_LOGICAL: f32 = 3.0;

/// Device px per cell for a device scale.
pub fn cell_for(scale: f32) -> u32 {
    (CELL_LOGICAL * scale).round().clamp(1.0, 64.0) as u32
}

/// The captured rect, box-averaged into cells, before any blur. Cheap to build and to hash.
pub struct Cells {
    pub w: usize,
    pub h: usize,
    pub cell: u32,
    rgb: Vec<u8>,
    /// Hash of the cell colours and the grid: equal ids mean the same picture.
    pub id: u64,
}

impl Cells {
    /// Box-average a top-down `w` x `h` BGRA capture (`stride` = `w * 4`) into `cell` px cells.
    pub fn from_bgra(raw: &[u8], w: usize, h: usize, cell: u32) -> Option<Cells> {
        let cell = cell.max(1) as usize;
        if w == 0 || h == 0 || raw.len() < w * h * 4 {
            return None;
        }
        let (cw, ch) = (w.div_ceil(cell), h.div_ceil(cell));
        let mut rgb = vec![0u8; cw * ch * 3];
        let mut acc = vec![0u32; cw * 3];
        for cy in 0..ch {
            acc.fill(0);
            let (y0, y1) = (cy * cell, ((cy + 1) * cell).min(h));
            for y in y0..y1 {
                let row = &raw[y * w * 4..(y + 1) * w * 4];
                for (cx, a) in acc.chunks_exact_mut(3).enumerate() {
                    let (x0, x1) = (cx * cell, ((cx + 1) * cell).min(w));
                    for p in row[x0 * 4..x1 * 4].chunks_exact(4) {
                        a[0] += p[2] as u32;
                        a[1] += p[1] as u32;
                        a[2] += p[0] as u32;
                    }
                }
            }
            for (cx, a) in acc.chunks_exact(3).enumerate() {
                let n = ((y1 - y0) * (((cx + 1) * cell).min(w) - cx * cell)) as u32;
                let o = (cy * cw + cx) * 3;
                for c in 0..3 {
                    rgb[o + c] = ((a[c] + n / 2) / n.max(1)) as u8;
                }
            }
        }
        let mut hs = Fnv::new();
        hs.write_usize(cw);
        hs.write_usize(ch);
        hs.write_usize(cell);
        hs.write(&rgb);
        Some(Cells { w: cw, h: ch, cell: cell as u32, rgb, id: hs.finish() })
    }

    /// Declare the cells to be `cell` device px wide (cells built from a logical-resolution picture).
    pub fn with_device_cell(mut self, cell: u32) -> Cells {
        self.cell = cell.max(1);
        self
    }

    /// From straight RGBA (fixtures): same averaging as a BGRA capture.
    pub fn from_rgba(rgba: &[u8], w: usize, h: usize, cell: u32) -> Option<Cells> {
        if rgba.len() < w * h * 4 {
            return None;
        }
        let mut bgra = rgba[..w * h * 4].to_vec();
        for p in bgra.chunks_exact_mut(4) {
            p.swap(0, 2);
        }
        Cells::from_bgra(&bgra, w, h, cell)
    }

    /// Frost the cells: gaussian (3 box passes) of `sigma` logical px at device `scale`, then
    /// `saturate(FROST_SATURATE)`.
    pub fn frost(&self, sigma: f32, scale: f32) -> Frosted {
        let n = self.w * self.h;
        let sigma_cells = (sigma * scale / self.cell as f32).max(0.0);
        let radii = box_radii(sigma_cells);
        let mut planes: [Vec<f32>; 3] = std::array::from_fn(|c| self.rgb.chunks_exact(3).map(|p| p[c] as f32).collect());
        let mut tmp = vec![0f32; n];
        for plane in &mut planes {
            for &r in &radii {
                if r > 0 {
                    box_h(plane, &mut tmp, self.w, self.h, r);
                    box_v(&tmp, plane, self.w, self.h, r);
                }
            }
        }
        let mut out = vec![0u8; n * 3];
        for i in 0..n {
            let (r, g, b) = (planes[0][i], planes[1][i], planes[2][i]);
            let gray = 0.213 * r + 0.715 * g + 0.072 * b;
            let sat = |c: f32| (gray + FROST_SATURATE * (c - gray)).clamp(0.0, 255.0);
            out[i * 3] = (sat(r) + 0.5) as u8;
            out[i * 3 + 1] = (sat(g) + 0.5) as u8;
            out[i * 3 + 2] = (sat(b) + 0.5) as u8;
        }
        Frosted { id: self.id, cell: self.cell, w: self.w, h: self.h, rgb: out }
    }
}

/// Box radii whose 3-pass cascade approximates a gaussian of `sigma` (Kutskir's "boxes for gauss").
fn box_radii(sigma: f32) -> [usize; 3] {
    if sigma < 0.3 {
        return [0, 0, 0];
    }
    let n = 3.0f32;
    let ideal = (12.0 * sigma * sigma / n + 1.0).sqrt();
    let mut wl = ideal.floor() as i32;
    if wl % 2 == 0 {
        wl -= 1;
    }
    let wl = wl.max(1);
    let wu = wl + 2;
    let m = ((12.0 * sigma * sigma - n * (wl * wl) as f32 - 4.0 * n * wl as f32 - 3.0 * n) / (-4.0 * wl as f32 - 4.0)).round() as i32;
    std::array::from_fn(|i| (if (i as i32) < m { wl } else { wu } as usize - 1) / 2)
}

/// Horizontal box blur of radius `r`, edges replicated.
fn box_h(src: &[f32], dst: &mut [f32], w: usize, h: usize, r: usize) {
    let inv = 1.0 / (2 * r + 1) as f32;
    for y in 0..h {
        let row = &src[y * w..(y + 1) * w];
        let out = &mut dst[y * w..(y + 1) * w];
        let mut acc = row[0] * r as f32;
        for i in 0..=r {
            acc += row[i.min(w - 1)];
        }
        for x in 0..w {
            out[x] = acc * inv;
            acc += row[(x + r + 1).min(w - 1)] - row[x.saturating_sub(r)];
        }
    }
}

/// Vertical box blur of radius `r`, edges replicated (row sweep, cache friendly).
fn box_v(src: &[f32], dst: &mut [f32], w: usize, h: usize, r: usize) {
    let inv = 1.0 / (2 * r + 1) as f32;
    let mut acc = vec![0f32; w];
    for (x, a) in acc.iter_mut().enumerate() {
        *a = src[x] * r as f32;
        for i in 0..=r {
            *a += src[i.min(h - 1) * w + x];
        }
    }
    for y in 0..h {
        let out = &mut dst[y * w..(y + 1) * w];
        let (add, sub) = (&src[(y + r + 1).min(h - 1) * w..][..w], &src[y.saturating_sub(r) * w..][..w]);
        for x in 0..w {
            out[x] = acc[x] * inv;
            acc[x] += add[x] - sub[x];
        }
    }
}

/// The frosted backdrop: smooth RGB cells of `cell` device px, sampled bilinearly.
pub struct Frosted {
    pub id: u64,
    pub cell: u32,
    pub w: usize,
    pub h: usize,
    rgb: Vec<u8>,
}

impl Frosted {
    /// A flat backdrop (the neutral fallback and tests).
    pub fn flat(rgb: [u8; 3]) -> Frosted {
        let mut h = Fnv::new();
        h.write(&rgb);
        Frosted { id: h.finish(), cell: 1, w: 1, h: 1, rgb: rgb.to_vec() }
    }

    /// Bilinear sample at `(x, y)` buffer device px (pixel centres), edges clamped; 0..1 RGB.
    #[inline]
    pub fn sample(&self, x: f32, y: f32) -> [f32; 3] {
        let inv = 1.0 / self.cell as f32;
        let fx = (x * inv - 0.5).clamp(0.0, (self.w - 1) as f32);
        let fy = (y * inv - 0.5).clamp(0.0, (self.h - 1) as f32);
        let (x0, y0) = (fx as usize, fy as usize);
        let (x1, y1) = ((x0 + 1).min(self.w - 1), (y0 + 1).min(self.h - 1));
        let (tx, ty) = (fx - x0 as f32, fy - y0 as f32);
        let at = |x: usize, y: usize, c: usize| self.rgb[(y * self.w + x) * 3 + c] as f32;
        let mut out = [0.0; 3];
        for (c, o) in out.iter_mut().enumerate() {
            let top = at(x0, y0, c) * (1.0 - tx) + at(x1, y0, c) * tx;
            let bot = at(x0, y1, c) * (1.0 - tx) + at(x1, y1, c) * tx;
            *o = (top * (1.0 - ty) + bot * ty) * (1.0 / 255.0);
        }
        out
    }

    #[cfg(test)]
    pub fn rgb_at(&self, x: usize, y: usize) -> [u8; 3] {
        let i = (y * self.w + x) * 3;
        [self.rgb[i], self.rgb[i + 1], self.rgb[i + 2]]
    }
}

/// A frosted buffer placed relative to the pill window, plus the page the field is scaled by.
#[derive(Clone)]
pub struct Backdrop {
    pub frosted: Arc<Frosted>,
    /// Window device px of the buffer's top-left corner (negative: it starts above/left of the window).
    pub ox: i32,
    pub oy: i32,
    /// The monitor in logical px: the skill's `pageW` / `pageH`.
    pub page: (f32, f32),
    /// Identifies the picture and its placement (part of the frame signature).
    pub id: u64,
}

/// The skill's reference page when none is known (a 1080p monitor).
pub const DEFAULT_PAGE: (f32, f32) = (1920.0, 1080.0);

impl Backdrop {
    pub fn new(frosted: Arc<Frosted>, ox: i32, oy: i32, page: (f32, f32)) -> Backdrop {
        let mut h = Fnv::new();
        h.write_u64(frosted.id);
        h.write_i32(ox);
        h.write_i32(oy);
        h.write_u32(page.0.to_bits());
        h.write_u32(page.1.to_bits());
        Backdrop { id: h.finish(), frosted, ox, oy, page }
    }

    /// Flat neutral backdrop for `dark`: what the glass is frosted over when nothing was captured.
    pub fn neutral(dark: bool) -> Arc<Backdrop> {
        thread_local! {
            static NEUTRAL: [Arc<Backdrop>; 2] = [neutral_backdrop(false), neutral_backdrop(true)];
        }
        NEUTRAL.with(|n| n[dark as usize].clone())
    }

    /// Sample at window device px.
    #[inline]
    pub fn sample(&self, wx: f32, wy: f32) -> [f32; 3] {
        self.frosted.sample(wx - self.ox as f32, wy - self.oy as f32)
    }
}

fn neutral_backdrop(dark: bool) -> Arc<Backdrop> {
    let rgb = if dark { [0x30, 0x33, 0x3a] } else { [0xd9, 0xdc, 0xe3] };
    Arc::new(Backdrop::new(Arc::new(Frosted::flat(rgb)), 0, 0, DEFAULT_PAGE))
}

/// Screen rect `(l, t, r, b)` to capture for a window at `win` (device px, screen coordinates): the
/// window plus `margin`, clipped to its `monitor`. `None` when nothing is left.
pub fn capture_region(win: (i32, i32, i32, i32), margin: i32, monitor: (i32, i32, i32, i32)) -> Option<(i32, i32, i32, i32)> {
    let r = (win.0 - margin).max(monitor.0);
    let t = (win.1 - margin).max(monitor.1);
    let rr = (win.2 + margin).min(monitor.2);
    let b = (win.3 + margin).min(monitor.3);
    (rr > r && b > t).then_some((r, t, rr, b))
}

/// Largest refraction margin worth capturing (logical px). The field reaches `0.1 * page` on the
/// rim's first pixel or two (see [`displacement`]); samples beyond the cap clamp to the captured edge.
pub const MARGIN_CAP: f32 = 96.0;

/// Capture margin in logical px for a page.
pub fn margin_logical(page: (f32, f32)) -> f32 {
    (0.12 * page.0.max(page.1)).min(MARGIN_CAP)
}

// --- baked displacement ------------------------------------------------------------------------------

/// Sub-pixel phase steps of a bake key.
const PHASE_STEPS: f32 = 16.0;
/// Largest map (cells) worth keeping; bigger shapes compute the field inline.
const MAX_BAKE_CELLS: usize = 200_000;
const BAKE_SLOTS: usize = 8;

#[derive(Clone, Copy, PartialEq, Eq)]
struct DispKey {
    w: u32,
    h: u32,
    r: u32,
    phase_x: u8,
    phase_y: u8,
    page_w: u32,
    page_h: u32,
    scale: u32,
}

/// Per-device-pixel sampling offsets (device px) of an exact rounded box.
pub struct DispMap {
    gw: usize,
    gh: usize,
    data: Vec<[f32; 2]>,
}

impl DispMap {
    fn bake(key: &DispKey, scale: f32) -> DispMap {
        let (bw, bh, r) = (f32::from_bits(key.w), f32::from_bits(key.h), f32::from_bits(key.r));
        let page = (f32::from_bits(key.page_w), f32::from_bits(key.page_h));
        let pad = (1.5 * scale).ceil() as i32 + 1;
        let gw = (bw * scale).ceil() as usize + 2 * pad as usize + 3;
        let gh = (bh * scale).ceil() as usize + 2 * pad as usize + 3;
        let (px, py) = (key.phase_x as f32 / PHASE_STEPS, key.phase_y as f32 / PHASE_STEPS);
        let min_dim = bw.min(bh);
        let mut data = vec![[0.0f32; 2]; gw * gh];
        for j in 0..gh {
            let v = (j as f32 - pad as f32 + 0.5 - py) / scale;
            for i in 0..gw {
                let u = (i as f32 - pad as f32 + 0.5 - px) / scale;
                let sd = sd_round_box(u, v, bw * 0.5, bh * 0.5, bw * 0.5, bh * 0.5, r);
                if sd > 1.0 {
                    continue; // outside, beyond the anti-aliased edge pixels
                }
                let depth = (-sd).max(0.0);
                let (cx, cy) = (u / bw, v / bh);
                if field_matters(depth, cx, cy, min_dim) {
                    let (fx, fy) = displacement(&GLASS_PRESET, depth, cx, cy, min_dim, page);
                    data[j * gw + i] = [fx * scale, fy * scale];
                }
            }
        }
        #[cfg(test)]
        BAKES.with(|b| b.set(b.get() + 1));
        DispMap { gw, gh, data }
    }

    #[inline]
    fn at(&self, x: i32, y: i32, gx0: i32, gy0: i32) -> [f32; 2] {
        let (i, j) = (x - gx0, y - gy0);
        if i < 0 || j < 0 || i as usize >= self.gw || j as usize >= self.gh {
            return [0.0, 0.0];
        }
        self.data[j as usize * self.gw + i as usize]
    }
}

thread_local! {
    static DISP: RefCell<Vec<(DispKey, Rc<DispMap>)>> = const { RefCell::new(Vec::new()) };
}
#[cfg(test)]
thread_local! {
    static BAKES: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// Number of displacement maps baked on this thread (tests).
#[cfg(test)]
pub fn bake_count() -> u32 {
    BAKES.with(|b| b.get())
}

/// Fetch (or bake) the map for a rounded box; returns it with the device origin `(gx0, gy0)` of its
/// grid.
fn disp_map(field: Rect, radius: f32, page: (f32, f32), scale: f32) -> Option<(Rc<DispMap>, i32, i32)> {
    let (ax, ay) = (field.l * scale, field.t * scale);
    let (mut fx, mut fy) = (ax.floor(), ay.floor());
    let (mut qx, mut qy) = (((ax - fx) * PHASE_STEPS).round(), ((ay - fy) * PHASE_STEPS).round());
    if qx >= PHASE_STEPS {
        fx += 1.0;
        qx = 0.0;
    }
    if qy >= PHASE_STEPS {
        fy += 1.0;
        qy = 0.0;
    }
    let key = DispKey {
        w: field.w().to_bits(),
        h: field.h().to_bits(),
        r: radius.to_bits(),
        phase_x: qx as u8,
        phase_y: qy as u8,
        page_w: page.0.to_bits(),
        page_h: page.1.to_bits(),
        scale: scale.to_bits(),
    };
    let pad = (1.5 * scale).ceil() as i32 + 1;
    let gw = (field.w() * scale).ceil() as usize + 2 * pad as usize + 3;
    let gh = (field.h() * scale).ceil() as usize + 2 * pad as usize + 3;
    if gw * gh > MAX_BAKE_CELLS {
        return None;
    }
    let map = DISP.with(|c| {
        let mut c = c.borrow_mut();
        if let Some(i) = c.iter().position(|(k, _)| *k == key) {
            let hit = c.remove(i);
            let m = hit.1.clone();
            c.insert(0, hit);
            return m;
        }
        let m = Rc::new(DispMap::bake(&key, scale));
        c.insert(0, (key, m.clone()));
        c.truncate(BAKE_SLOTS);
        m
    });
    Some((map, fx as i32 - pad, fy as i32 - pad))
}

// --- painting --------------------------------------------------------------------------------------

/// What a glass surface is painted with.
pub struct Ctx<'a> {
    pub backdrop: &'a Backdrop,
    pub dark: bool,
}

/// The shape of a surface. `field` is the box the refraction field is normalised to (the element's
/// box in the web recipe); `radius` is its corner radius when `sdf` is exactly that rounded box (the
/// field can then be baked and reused), `None` for free shapes such as the fused pill and cancel bud.
pub struct Surface<'a> {
    pub field: Rect,
    pub radius: Option<f32>,
    pub sdf: &'a dyn Fn(f32, f32) -> f32,
}

/// Per-paint look.
#[derive(Clone, Copy, Debug)]
pub struct Params {
    /// Opacity of the whole surface (state cross-fades).
    pub alpha: f32,
    /// Drop shadow and its opacity multiplier.
    pub shadow: Shadow,
    pub shadow_alpha: f32,
    /// 0 at rest, 1 hovered: raises the tint from 6 % to 11 %.
    pub lift: f32,
    /// Multiplier of the border alpha (the `/60` processing states).
    pub edge: f32,
    /// Frost alpha override (the error card is a modal: text has to read on it); `None`: the pill's.
    pub frost: Option<f32>,
}

#[inline]
fn mix(a: [f32; 3], b: [f32; 3], t: f32) -> [f32; 3] {
    [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t, a[2] + (b[2] - a[2]) * t]
}

/// The drop shadow of a surface (black, `alpha` scales the shadow's own alpha).
pub fn paint_shadow(cv: &mut Canvas, surf: &Surface, shadow: Shadow, alpha: f32) {
    if alpha > 0.001 && shadow.alpha > 0.001 {
        cv.shadow_sdf(surf.field, shadow.dy, shadow.blur, shadow.spread, Rgba::new(0.0, 0.0, 0.0, 1.0), shadow.alpha * alpha, |x, y| (surf.sdf)(x, y));
    }
}

/// Paint a refracting glass surface: shadow, refracted + frosted backdrop, tint, rim, border.
pub fn paint(cv: &mut Canvas, ctx: &Ctx, surf: &Surface, p: &Params) {
    let tk = tokens(ctx.dark);
    paint_shadow(cv, surf, p.shadow, p.shadow_alpha);
    if p.alpha <= 0.002 {
        return;
    }
    let s = cv.scale;
    let inv = 1.0 / s;
    let field = surf.field;
    let (bw, bh) = (field.w().max(1e-3), field.h().max(1e-3));
    let min_dim = bw.min(bh);
    let page = ctx.backdrop.page;
    let range = DevRect::of(field.inflate(1.5), s, cv.w, cv.h);
    let baked = surf.radius.and_then(|r| disp_map(field, r, page, s));
    let tint_a = tk.tint_alpha + (tk.tint_alpha_hover - tk.tint_alpha) * p.lift.clamp(0.0, 1.0);
    let border_rgb = [tk.border.r, tk.border.g, tk.border.b];
    let border_a = tk.border.a * p.edge;
    let sdf = surf.sdf;
    for y in range.y0..range.y1 {
        let ly = (y as f32 + 0.5) * inv;
        let ty = ((ly - field.t) / bh).clamp(0.0, 1.0);
        for x in range.x0..range.x1 {
            let lx = (x as f32 + 0.5) * inv;
            let d = sdf(lx, ly);
            let c = cov(d * s);
            if c <= 0.0 {
                continue;
            }
            // refraction: where this pixel samples the frosted backdrop
            let (mut sx, mut sy) = (x as f32 + 0.5, y as f32 + 0.5);
            if let Some((m, gx0, gy0)) = &baked {
                let o = m.at(x, y, *gx0, *gy0);
                sx += o[0];
                sy += o[1];
            } else {
                let depth = (-d).max(0.0);
                let (cx, cy) = ((lx - field.l) / bw, (ly - field.t) / bh);
                if field_matters(depth, cx, cy, min_dim) {
                    let (fx, fy) = displacement(&GLASS_PRESET, depth, cx, cy, min_dim, page);
                    sx += fx * s;
                    sy += fy * s;
                }
            }
            let mut col = ctx.backdrop.sample(sx, sy);
            col = mix(col, tk.frost, p.frost.unwrap_or(tk.frost_alpha));
            col = mix(col, mix(tk.tint_top, tk.tint_bottom, ty), tint_a);
            // rim: three inset highlights inside the padding box (1 px in from the border)
            let mut pad = c;
            if d > -3.0 {
                pad = cov((d + 1.0) * s);
                if pad > 0.0 {
                    let top = pad * (1.0 - cov((sdf(lx, ly - 1.0) + 1.0) * s));
                    let bottom = pad * (1.0 - cov((sdf(lx, ly + 1.0) + 1.0) * s));
                    let hair = pad * (1.0 - cov((d + 2.0) * s));
                    col = mix(col, WHITE, tk.rim_top * top);
                    col = mix(col, WHITE, tk.rim_hairline * hair);
                    col = mix(col, WHITE, tk.rim_bottom * bottom);
                }
            }
            // border: the 1 px ring outside the padding box
            let ring = (c - pad).max(0.0);
            if ring > 0.0 {
                col = mix(col, border_rgb, border_a * ring / c);
            }
            let a = c * p.alpha;
            cv.blend(x, y, [col[0] * a, col[1] * a, col[2] * a, a]);
        }
    }
}

/// Menu-tier colour (tooltip): the frosted backdrop under a wash of the overlay surface, no
/// refraction. Opaque 0..1 RGB at window device px `(wx, wy)`.
pub fn menu_color(ctx: &Ctx, wx: f32, wy: f32) -> [f32; 3] {
    let tk = tokens(ctx.dark);
    mix(ctx.backdrop.sample(wx, wy), tk.menu, tk.menu_alpha)
}

/// Paint a menu-tier surface (the tooltip body): wash, 1 px border and the top highlight. The caller
/// draws any shadow.
pub fn paint_menu(cv: &mut Canvas, ctx: &Ctx, bbox: Rect, alpha: f32, sdf: &dyn Fn(f32, f32) -> f32) {
    let tk = tokens(ctx.dark);
    let s = cv.scale;
    let inv = 1.0 / s;
    let range = DevRect::of(bbox.inflate(1.0), s, cv.w, cv.h);
    let border = [tk.menu_border.r, tk.menu_border.g, tk.menu_border.b];
    for y in range.y0..range.y1 {
        let ly = (y as f32 + 0.5) * inv;
        for x in range.x0..range.x1 {
            let lx = (x as f32 + 0.5) * inv;
            let d = sdf(lx, ly);
            let c = cov(d * s);
            if c <= 0.0 {
                continue;
            }
            let mut col = menu_color(ctx, x as f32 + 0.5, y as f32 + 0.5);
            let pad = cov((d + 1.0) * s);
            if pad > 0.0 && d > -3.0 {
                let top = pad * (1.0 - cov((sdf(lx, ly - 1.0) + 1.0) * s));
                col = mix(col, WHITE, tk.rim_top * top);
            }
            let ring = (c - pad).max(0.0);
            if ring > 0.0 {
                col = mix(col, border, tk.menu_border.a * ring / c);
            }
            let a = c * alpha;
            cv.blend(x, y, [col[0] * a, col[1] * a, col[2] * a, a]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::pill::raster::sd_capsule;
    use tiny_skia::Pixmap;

    fn flat_backdrop(rgb: [u8; 3]) -> Backdrop {
        Backdrop::new(Arc::new(Frosted::flat(rgb)), 0, 0, DEFAULT_PAGE)
    }

    /// WCAG relative luminance of an sRGB 0..1 colour.
    fn luminance(c: [f32; 3]) -> f32 {
        let lin = |v: f32| if v <= 0.03928 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) };
        0.2126 * lin(c[0]) + 0.7152 * lin(c[1]) + 0.0722 * lin(c[2])
    }

    fn contrast(a: [f32; 3], b: [f32; 3]) -> f32 {
        let (la, lb) = (luminance(a), luminance(b));
        (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
    }

    fn px_rgb(pm: &Pixmap, x: u32, y: u32) -> [f32; 3] {
        let p = pm.pixel(x, y).unwrap().demultiply();
        [p.red() as f32 / 255.0, p.green() as f32 / 255.0, p.blue() as f32 / 255.0]
    }

    fn rest_params(dark: bool) -> Params {
        let tk = tokens(dark);
        Params { alpha: 1.0, shadow: tk.shadow, shadow_alpha: 1.0, lift: 0.0, edge: 1.0, frost: None }
    }

    /// Paint a 98x36 capsule over `backdrop` and return the pixmap.
    fn paint_capsule(backdrop: &Backdrop, dark: bool, scale: f32) -> Pixmap {
        let mut pm = Pixmap::new((208.0 * scale) as u32, (120.0 * scale) as u32).unwrap();
        let r = Rect::from_xywh(98.0, 72.0, 98.0, 36.0);
        let sdf = move |x: f32, y: f32| sd_capsule(x, y, r.cx(), r.cy(), r.w(), r.h());
        let surf = Surface { field: r, radius: Some(18.0), sdf: &sdf };
        let ctx = Ctx { backdrop, dark };
        paint(&mut Canvas::new(&mut pm, scale), &ctx, &surf, &rest_params(dark));
        pm
    }

    #[test]
    fn field_matches_the_skill_at_known_points() {
        let p = GLASS_PRESET;
        // right-middle of a 98x36 box, on the edge: normal (1, 0), no ripple (sin 0), tiny corner term
        let (fx, fy) = displacement(&p, 0.0, 1.0, 0.5, 36.0, (1920.0, 1080.0));
        let corner = (-0.5f32 * 36.0 * 0.3).exp() * 0.06;
        assert!((fx - (0.043 + corner) * 1920.0).abs() < 0.01, "{fx}");
        assert!(fy.abs() < 1e-3, "{fy}");
        // left-middle mirrors it
        let (lx, _) = displacement(&p, 0.0, 0.0, 0.5, 36.0, (1920.0, 1080.0));
        assert!((lx + fx).abs() < 0.01);
        // top-middle samples from further up (negative y), scaled by pageH
        let (tx, ty) = displacement(&p, 0.0, 0.5, 0.0, 36.0, (1920.0, 1080.0));
        assert!(ty < -40.0 && tx.abs() < 1e-3, "({tx}, {ty})");
        // one pixel in, the rim term has decayed: exp(-1.7) of 0.028 plus exp(-0.5) of 0.015
        let (f1, _) = displacement(&p, 1.0, 1.0, 0.5, 36.0, (1920.0, 1080.0));
        let want = (0.015 * (-0.5f32).exp() + 0.028 * (-1.7f32).exp()) * 1920.0;
        assert!((f1 - want).abs() < 2.5, "{f1} vs {want}");
        // the perpendicular ripple appears off the edge: sin(1/36*25) * 0.26 * exp(-1.7) of the page
        let (_, ry) = displacement(&p, 1.0, 1.0, 0.5, 36.0, (1920.0, 1080.0));
        assert!((ry - (25.0f32 / 36.0).sin() * 0.26 * (-1.7f32).exp() * 1080.0).abs() < 0.5, "{ry}");
    }

    #[test]
    fn centre_is_undistorted_and_warp_is_off() {
        let (fx, fy) = displacement(&GLASS_PRESET, 17.0, 0.5, 0.5, 36.0, (1920.0, 1080.0));
        assert!(fx.abs() < 1e-3 && fy.abs() < 1e-3, "({fx}, {fy})");
        assert!(!GLASS_PRESET.warp);
        // with warp on the base term would show up in the middle
        let warp = Preset { warp: true, ..GLASS_PRESET };
        let (wx, wy) = displacement(&warp, 17.0, 0.7, 0.5, 36.0, (1920.0, 1080.0));
        assert!(wx > 10.0 || wy.abs() > 0.0);
        assert!(!field_matters(30.0, 0.5, 0.5, 120.0));
        // a 36 px pill never gets that far from its corners: the corner term still moves its middle by ~0.5 px
        assert!(field_matters(30.0, 0.7, 0.5, 36.0));
        assert!(field_matters(2.0, 0.5, 0.5, 36.0));
    }

    #[test]
    fn region_is_clipped_to_the_monitor() {
        let mon = (0, 0, 1920, 1080);
        // bottom-right window: the margin hangs off the screen and is cut
        let win = (1708, 956, 1916, 1076);
        assert_eq!(capture_region(win, 96, mon), Some((1612, 860, 1920, 1080)));
        // monitor with a negative origin
        assert_eq!(capture_region((-300, 100, -100, 200), 50, (-1920, 0, 0, 1080)), Some((-350, 50, -50, 250)));
        // nothing visible
        assert_eq!(capture_region((3000, 0, 3100, 100), 10, mon), None);
        assert!(margin_logical((1920.0, 1080.0)) <= MARGIN_CAP);
        assert!(margin_logical((640.0, 360.0)) < MARGIN_CAP);
    }

    #[test]
    fn cells_average_and_hash() {
        let (w, h) = (6usize, 4usize);
        let mut raw = vec![0u8; w * h * 4];
        for p in raw.chunks_exact_mut(4) {
            p.copy_from_slice(&[10, 20, 30, 255]); // BGRA
        }
        let a = Cells::from_bgra(&raw, w, h, 2).unwrap();
        assert_eq!((a.w, a.h), (3, 2));
        assert_eq!(&a.rgb[..3], &[30, 20, 10]);
        let same = Cells::from_bgra(&raw, w, h, 2).unwrap();
        assert_eq!(a.id, same.id, "the same picture hashes the same");
        raw[0] = 200;
        let changed = Cells::from_bgra(&raw, w, h, 2).unwrap();
        assert_ne!(a.id, changed.id, "a changed pixel changes the hash");
        // the grid is part of the hash
        assert_ne!(a.id, Cells::from_bgra(&raw, w, h, 3).unwrap().id);
        // ragged edges average only the pixels that exist
        let r = Cells::from_bgra(&raw[..5 * 3 * 4], 5, 3, 2).unwrap();
        assert_eq!((r.w, r.h), (3, 2));
        assert!(Cells::from_bgra(&raw[..8], w, h, 2).is_none());
    }

    #[test]
    fn frost_keeps_flat_colour_and_saturates() {
        // a flat grey stays grey (saturate leaves luminance alone)
        let raw: Vec<u8> = (0..40 * 40).flat_map(|_| [120u8, 120, 120, 255]).collect();
        let f = Cells::from_bgra(&raw, 40, 40, 2).unwrap().frost(FROST_SIGMA, 1.0);
        for &(x, y) in &[(0, 0), (10, 10), (19, 19), (5, 17)] {
            let c = f.rgb_at(x, y);
            assert!(c.iter().all(|&v| (v as i32 - 120).abs() <= 1), "{c:?}");
        }
        // a coloured field gets more saturated: red channel above, blue below the grey point
        let raw: Vec<u8> = (0..40 * 40).flat_map(|_| [100u8, 120, 160, 255]).collect(); // BGRA: r160 g120 b100
        let f = Cells::from_bgra(&raw, 40, 40, 2).unwrap().frost(FROST_SIGMA, 1.0);
        let c = f.rgb_at(10, 10);
        assert!(c[0] > 160 && c[2] < 100, "{c:?}");
    }

    #[test]
    fn frost_blurs_edges_and_preserves_the_mean() {
        // left half black, right half white
        let (w, h) = (120usize, 60usize);
        let raw: Vec<u8> = (0..w * h).flat_map(|i| if (i % w) < w / 2 { [0u8, 0, 0, 255] } else { [255, 255, 255, 255] }).collect();
        let cells = Cells::from_bgra(&raw, w, h, 3).unwrap();
        let f = cells.frost(FROST_SIGMA, 1.0);
        let mid = f.rgb_at(f.w / 2, f.h / 2)[0] as i32;
        assert!((mid - 128).abs() < 24, "the step is smoothed through grey, got {mid}");
        // 10 logical px = 3.3 cells away from the step the colour is still not saturated at the pole
        let near = f.rgb_at(f.w / 2 - 3, f.h / 2)[0];
        let far = f.rgb_at(2, f.h / 2)[0];
        assert!(near > far + 8, "a gradient across the step: {far} .. {near}");
        // the clamped edges keep the pole colours
        assert!(far < 10 && f.rgb_at(f.w - 3, 5)[0] > 245);
    }

    #[test]
    fn box_radii_cascade_matches_the_gaussian_variance() {
        for sigma in [1.0f32, 2.5, 3.33, 6.0] {
            let r = box_radii(sigma);
            // variance of a box of radius r is ((2r+1)^2 - 1) / 12; the cascade adds them
            let var: f32 = r.iter().map(|&r| (((2 * r + 1) * (2 * r + 1)) as f32 - 1.0) / 12.0).sum();
            assert!((var.sqrt() - sigma).abs() < 0.45, "sigma {sigma}: cascade {} ({r:?})", var.sqrt());
        }
        assert_eq!(box_radii(0.0), [0, 0, 0]);
    }

    #[test]
    fn frosted_sampling_is_bilinear_and_clamped() {
        let mut raw = vec![0u8; 4 * 4 * 4];
        for (i, p) in raw.chunks_exact_mut(4).enumerate() {
            let v = if i % 4 < 2 { 0 } else { 200 };
            p.copy_from_slice(&[v, v, v, 255]);
        }
        let f = Cells::from_bgra(&raw, 4, 4, 1).unwrap().frost(0.0, 1.0);
        // halfway between a dark and a bright cell centre
        let m = f.sample(2.0, 0.5)[0] * 255.0;
        assert!((m - 100.0).abs() < 8.0, "{m}");
        // far outside clamps to the edge cell
        assert!((f.sample(-50.0, -50.0)[0] * 255.0).abs() < 1.0);
        assert!((f.sample(900.0, 2.0)[0] * 255.0 - 200.0).abs() < 8.0);
    }

    #[test]
    fn displacement_bake_is_reused_per_size_radius_page_and_scale() {
        let b = flat_backdrop([128, 128, 128]);
        let before = bake_count();
        let _ = paint_capsule(&b, false, 1.0);
        assert_eq!(bake_count(), before + 1);
        let _ = paint_capsule(&b, true, 1.0); // theme does not matter
        let _ = paint_capsule(&b, false, 1.0);
        assert_eq!(bake_count(), before + 1, "same size, radius, page and scale: baked once");
        let _ = paint_capsule(&b, false, 2.0);
        assert_eq!(bake_count(), before + 2, "a new scale bakes again");
        let other = Backdrop::new(b.frosted.clone(), 0, 0, (2560.0, 1440.0));
        let _ = paint_capsule(&other, false, 1.0);
        assert_eq!(bake_count(), before + 3, "a new monitor size bakes again");
    }

    #[test]
    fn baked_field_matches_the_direct_field() {
        // a backdrop with structure so a wrong offset shows
        let (w, h) = (300usize, 160usize);
        let raw: Vec<u8> = (0..w * h).flat_map(|i| { let (x, y) = (i % w, i / w); [(x * 255 / w) as u8, (y * 255 / h) as u8, ((x ^ y) & 255) as u8, 255] }).collect();
        let frosted = Arc::new(Cells::from_bgra(&raw, w, h, 3).unwrap().frost(FROST_SIGMA, 1.0));
        let b = Backdrop::new(frosted, 0, 0, DEFAULT_PAGE);
        let baked = paint_capsule(&b, false, 1.0);
        // the same surface declared as a free shape takes the inline path
        let mut pm = Pixmap::new(208, 120).unwrap();
        let r = Rect::from_xywh(98.0, 72.0, 98.0, 36.0);
        let sdf = move |x: f32, y: f32| sd_capsule(x, y, r.cx(), r.cy(), r.w(), r.h());
        let surf = Surface { field: r, radius: None, sdf: &sdf };
        paint(&mut Canvas::new(&mut pm, 1.0), &Ctx { backdrop: &b, dark: false }, &surf, &rest_params(false));
        let worst = baked.data().iter().zip(pm.data()).map(|(a, b)| a.abs_diff(*b)).max().unwrap();
        assert!(worst <= 3, "baked and inline fields differ by {worst}");
    }

    #[test]
    fn glass_is_opaque_inside_with_a_visible_rim() {
        let b = flat_backdrop([128, 128, 128]);
        let pm = paint_capsule(&b, false, 1.0);
        // capsule x 98..196, y 72..108
        assert_eq!(pm.pixel(150, 90).unwrap().alpha(), 255);
        assert_eq!(pm.pixel(150, 60).unwrap().alpha(), 0);
        let mid = px_rgb(&pm, 150, 90);
        // light frost over mid-grey: lighter than the backdrop, still not white
        assert!(mid[0] > 0.6 && mid[0] < 0.95, "{mid:?}");
        // the top rim highlight is brighter than the interior just below it
        let rim = px_rgb(&pm, 150, 73);
        assert!(rim[0] > px_rgb(&pm, 150, 80)[0] + 0.03, "rim {rim:?}");
        // the tint gradient darkens towards the bottom
        assert!(px_rgb(&pm, 150, 100)[0] < px_rgb(&pm, 150, 78)[0]);
        // dark mode frosts dark
        let pd = paint_capsule(&b, true, 1.0);
        assert!(px_rgb(&pd, 150, 90)[0] < 0.3);
    }

    #[test]
    fn hover_lift_and_processing_edge_change_the_surface() {
        let b = flat_backdrop([100, 100, 100]);
        let paint_with = |lift: f32, edge: f32| {
            let mut pm = Pixmap::new(208, 120).unwrap();
            let r = Rect::from_xywh(98.0, 72.0, 98.0, 36.0);
            let sdf = move |x: f32, y: f32| sd_capsule(x, y, r.cx(), r.cy(), r.w(), r.h());
            let surf = Surface { field: r, radius: Some(18.0), sdf: &sdf };
            let p = Params { lift, edge, ..rest_params(false) };
            paint(&mut Canvas::new(&mut pm, 1.0), &Ctx { backdrop: &b, dark: false }, &surf, &p);
            pm
        };
        let rest = paint_with(0.0, 1.0);
        let lifted = paint_with(1.0, 1.0);
        assert!(px_rgb(&lifted, 150, 90)[0] > px_rgb(&rest, 150, 90)[0], "hover raises the tint");
        // the 1 px border ring (x = 98) is dimmer when edge = 0.6, and gone at 0
        let full = px_rgb(&rest, 98, 90)[0];
        let none = px_rgb(&paint_with(0.0, 0.0), 98, 90)[0];
        let dim = px_rgb(&paint_with(0.0, 0.6), 98, 90)[0];
        assert!(none > dim && dim > full, "{none} {dim} {full}");
    }

    #[test]
    fn rim_refracts_the_backdrop_from_outside_the_shape() {
        // bright on the right of the window, dark inside: the right edge pixels sample outward
        let (w, h) = (208usize, 120usize);
        let raw: Vec<u8> = (0..w * h).flat_map(|i| if (i % w) >= 196 { [255u8, 255, 255, 255] } else { [0, 0, 0, 255] }).collect();
        let frosted = Arc::new(Cells::from_bgra(&raw, w, h, 1).unwrap().frost(0.0, 1.0));
        let b = Backdrop::new(frosted, 0, 0, DEFAULT_PAGE);
        let pm = paint_capsule(&b, false, 1.0);
        // the capsule's right edge is at x = 196: pixels 192..195 pull white from x >= 196
        let edge = px_rgb(&pm, 194, 90)[0];
        let inner = px_rgb(&pm, 150, 90)[0];
        assert!(edge > inner + 0.1, "edge {edge} inner {inner}");
        // far from the edge the dark half is untouched
        let left = px_rgb(&pm, 110, 90)[0];
        assert!((left - inner).abs() < 0.03, "left {left} inner {inner}");
    }

    /// White, black and a busy stripe/noise backdrop, 520x260 placed so they cover a 466x120 window
    /// with room to spare (origin at window (-40, -40)).
    fn test_backdrops() -> Vec<(&'static str, Backdrop)> {
        let (w, h) = (520usize, 260usize);
        let busy: Vec<u8> = (0..w * h)
            .flat_map(|i| {
                let (x, y) = (i % w, i / w);
                let v = if (x / 2 + y / 3) % 2 == 0 { 0u8 } else { 255 };
                let n = ((x * 31 + y * 17) % 64) as u8;
                [v ^ n, v.wrapping_add(n), v ^ (n << 2), 255]
            })
            .collect();
        let raws: Vec<(&'static str, Vec<u8>)> = vec![
            ("white", vec![255u8; w * h * 4]),
            ("black", (0..w * h).flat_map(|_| [0u8, 0, 0, 255]).collect()),
            ("busy", busy),
        ];
        raws.into_iter()
            .map(|(name, raw)| {
                let frosted = Arc::new(Cells::from_bgra(&raw, w, h, 3).unwrap().frost(FROST_SIGMA, 1.0));
                (name, Backdrop::new(frosted, -40, -40, DEFAULT_PAGE))
            })
            .collect()
    }

    #[test]
    fn legibility_over_white_black_and_busy_backdrops() {
        let fg_light = Rgba::hex(0x171717);
        let fg_dark = Rgba::hex(0xe8e8e8);
        for (name, b) in test_backdrops() {
            for (dark, fg) in [(false, fg_light), (true, fg_dark)] {
                let pm = paint_capsule(&b, dark, 1.0);
                let theme = if dark { "dark" } else { "light" };
                // the waveform lane: x 132..184, y 78..102
                let mut worst = f32::MAX;
                for y in 78..102u32 {
                    for x in 132..184u32 {
                        worst = worst.min(contrast([fg.r, fg.g, fg.b], px_rgb(&pm, x, y)));
                    }
                }
                assert!(worst >= 4.5, "{name} {theme}: waveform contrast {worst:.2}");
                // the logo slot
                let mut logo = f32::MAX;
                for y in 80..100u32 {
                    for x in 104..126u32 {
                        logo = logo.min(contrast([fg.r, fg.g, fg.b], px_rgb(&pm, x, y)));
                    }
                }
                assert!(logo >= 4.5, "{name} {theme}: logo contrast {logo:.2}");
            }
        }
    }

    #[test]
    fn error_card_text_is_legible_over_white_black_and_busy_backdrops() {
        // title (fg) and the muted description (muted-fg) on the card surface, both themes
        for (name, b) in test_backdrops() {
            for dark in [false, true] {
                let tk = tokens(dark);
                let mut pm = Pixmap::new(466, 120).unwrap();
                let r = Rect::from_xywh(12.0, 12.0, 442.0, 76.0);
                let sdf = move |x: f32, y: f32| sd_round_box(x, y, r.cx(), r.cy(), r.w() * 0.5, r.h() * 0.5, 16.0);
                let surf = Surface { field: r, radius: Some(16.0), sdf: &sdf };
                let p = Params { alpha: 1.0, shadow: tk.card_shadow, shadow_alpha: 1.0, lift: 0.0, edge: 1.0, frost: Some(tk.card_frost_alpha) };
                paint(&mut Canvas::new(&mut pm, 1.0), &Ctx { backdrop: &b, dark }, &surf, &p);
                let pal = if dark { crate::daemon::pill::theme::Palette::DARK } else { crate::daemon::pill::theme::Palette::LIGHT };
                for (what, fg) in [("title", pal.fg), ("description", pal.muted_fg)] {
                    let mut worst = f32::MAX;
                    for y in 28..72u32 {
                        for x in 28..420u32 {
                            worst = worst.min(contrast([fg.r, fg.g, fg.b], px_rgb(&pm, x, y)));
                        }
                    }
                    assert!(worst >= 4.5, "{name} {}: card {what} contrast {worst:.2}", if dark { "dark" } else { "light" });
                }
            }
        }
    }

    #[test]
    fn menu_tier_washes_and_reads() {
        let b = flat_backdrop([0, 0, 0]);
        let ctx = Ctx { backdrop: &b, dark: false };
        // over black the light wash is still light enough for the dark foreground (tooltip text)
        let c = menu_color(&ctx, 10.0, 10.0);
        assert!(contrast(c, [0.09, 0.09, 0.09]) >= 4.5, "{c:?}");
        let w = flat_backdrop([255, 255, 255]);
        let dctx = Ctx { backdrop: &w, dark: true };
        let c = menu_color(&dctx, 10.0, 10.0);
        assert!(contrast(c, [0.91, 0.91, 0.91]) >= 4.5, "{c:?}");
        let mut pm = Pixmap::new(60, 30).unwrap();
        let r = Rect::from_xywh(5.0, 5.0, 50.0, 20.0);
        let sdf = move |x: f32, y: f32| sd_capsule(x, y, r.cx(), r.cy(), r.w(), r.h());
        paint_menu(&mut Canvas::new(&mut pm, 1.0), &ctx, r, 1.0, &sdf);
        assert_eq!(pm.pixel(30, 15).unwrap().alpha(), 255);
        assert_eq!(pm.pixel(1, 1).unwrap().alpha(), 0);
    }

    #[test]
    fn neutral_backdrop_is_shared_and_flat() {
        let a = Backdrop::neutral(false);
        let b = Backdrop::neutral(false);
        assert!(Arc::ptr_eq(&a, &b));
        assert_ne!(a.id, Backdrop::neutral(true).id);
        let s0 = a.sample(0.0, 0.0);
        let s1 = a.sample(500.0, 300.0);
        assert_eq!(s0, s1);
    }
}
