//! Liquid glass for the Settings UI: a native egui port of the `liquid-glass` skill
//! (`assets/liquid-glass.css` for the tokens, `references/refraction.md` for the field).
//!
//! The web skill is two layers, and so is this port:
//!
//! 1. the **CSS layer** (tint gradient, 1 px border, three inset rim highlights, outer shadow)
//!    is painted from the same tokens, both themes;
//! 2. the **refraction layer** is the skill's displacement field evaluated analytically. Because
//!    the backdrop under every panel is our own ambient mesh ([`mesh_color`]), a panel can "bend"
//!    it exactly: a thin band inside the rim is redrawn with `mesh_color(p + displacement(p))`.
//!    Warp is off, so the interior stays undistorted.
//!
//! Tiers (see `docs/GLASS.md`): [`panel`] (tint + rim + refraction band), [`chip`] (CSS only),
//! [`menu`] (55 % wash of the menu surface), [`modal`] (panel + frost over a scrim) and
//! [`colored`] (tint override for primary / destructive buttons).
//!
//! All painters return shapes (`*_shapes`) or add them to a [`Painter`]; none allocates a
//! texture, none requests a repaint. Geometry is cached per size, so a repaint costs a few
//! hundred vertex colours per surface.
#![allow(dead_code)] // the tier painters are a toolkit; not every tier is used by every page yet
use eframe::egui::epaint::{Vertex, WHITE_UV};
use eframe::egui::{self, Color32, Mesh, Painter, Pos2, Rect, Shape, Stroke, StrokeKind, Vec2, pos2, vec2};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};

/// CSS `rem` in points; the mesh geometry is specified in rem.
pub const REM: f32 = 16.0;
/// `--glass-radius`.
pub const RADIUS: f32 = 12.0;
/// `--glass-radius-lg` (modals).
pub const RADIUS_LG: f32 = 20.0;
/// `--glass-duration` in seconds (hover transitions).
pub const DURATION: f32 = 0.18;

// ------------------------------------------------------------------------------------------ tokens

/// An `rgba()` token: 8-bit channels and a fractional alpha, exactly as written in the CSS.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rgba {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: f32,
}

impl Rgba {
    pub const fn new(r: u8, g: u8, b: u8, a: f32) -> Self {
        Self { r, g, b, a }
    }
    /// Opaque `#rrggbb`.
    pub const fn hex(h: u32) -> Self {
        Self { r: (h >> 16) as u8, g: (h >> 8) as u8, b: h as u8, a: 1.0 }
    }
    /// Premultiplied float colour with alpha scaled by `k`.
    fn px(self, k: f32) -> Px {
        let a = (self.a * k).clamp(0.0, 1.0);
        [self.r as f32 / 255.0 * a, self.g as f32 / 255.0 * a, self.b as f32 / 255.0 * a, a]
    }
    pub fn color(self) -> Color32 {
        to_color(self.px(1.0))
    }
    pub fn scaled(self, k: f32) -> Color32 {
        to_color(self.px(k))
    }
}

/// One CSS `box-shadow` (x offset is always 0 in the skill).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ShadowTok {
    pub dy: f32,
    pub blur: f32,
    pub color: Rgba,
}

/// Every token of `liquid-glass.css`, valued for one theme.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Tokens {
    pub dark: bool,
    pub tint_top: Rgba,
    pub tint_bottom: Rgba,
    pub tint_top_hover: Rgba,
    pub tint_bottom_hover: Rgba,
    pub rim_top: Rgba,
    pub rim_hairline: Rgba,
    pub rim_bottom: Rgba,
    pub border: Rgba,
    pub shadow: ShadowTok,
    pub menu_surface: Rgba,
    pub menu_shadow: [ShadowTok; 2],
    pub modal_tint: Rgba,
    pub scrim: Rgba,
    pub page_bg: Rgba,
    pub page_fg: Rgba,
    pub mesh_violet: Rgba,
    pub mesh_pink: Rgba,
    pub mesh_cyan: Rgba,
    pub mesh_corner_top: Rgba,
    pub mesh_corner_bottom: Rgba,
}

/// `html[data-theme='light']` (and `:root`).
pub const LIGHT: Tokens = Tokens {
    dark: false,
    tint_top: Rgba::new(255, 255, 255, 0.06),
    tint_bottom: Rgba::new(179, 179, 179, 0.06),
    tint_top_hover: Rgba::new(255, 255, 255, 0.11),
    tint_bottom_hover: Rgba::new(179, 179, 179, 0.11),
    rim_top: Rgba::new(255, 255, 255, 0.55),
    rim_hairline: Rgba::new(255, 255, 255, 0.12),
    rim_bottom: Rgba::new(255, 255, 255, 0.06),
    border: Rgba::new(30, 33, 45, 0.12),
    shadow: ShadowTok { dy: 10.0, blur: 30.0, color: Rgba::new(30, 33, 45, 0.1) },
    menu_surface: Rgba::hex(0xf6f7f9),
    menu_shadow: [
        ShadowTok { dy: 12.0, blur: 32.0, color: Rgba::new(20, 22, 30, 0.14) },
        ShadowTok { dy: 4.0, blur: 10.0, color: Rgba::new(20, 22, 30, 0.07) },
    ],
    modal_tint: Rgba::new(255, 255, 255, 0.44),
    scrim: Rgba::new(20, 22, 30, 0.35),
    page_bg: Rgba::hex(0xe8eaef),
    page_fg: Rgba::hex(0x2b2f3a),
    mesh_violet: Rgba::new(139, 92, 246, 0.2),
    mesh_pink: Rgba::new(236, 72, 153, 0.16),
    mesh_cyan: Rgba::new(6, 182, 212, 0.14),
    mesh_corner_top: Rgba::new(79, 70, 229, 0.05),
    mesh_corner_bottom: Rgba::new(67, 56, 202, 0.06),
};

/// `html[data-theme='dark']`.
pub const DARK: Tokens = Tokens {
    dark: true,
    tint_top: Rgba::new(255, 255, 255, 0.06),
    tint_bottom: Rgba::new(179, 179, 179, 0.06),
    tint_top_hover: Rgba::new(255, 255, 255, 0.11),
    tint_bottom_hover: Rgba::new(179, 179, 179, 0.11),
    rim_top: Rgba::new(255, 255, 255, 0.5),
    rim_hairline: Rgba::new(255, 255, 255, 0.1),
    rim_bottom: Rgba::new(255, 255, 255, 0.06),
    border: Rgba::new(255, 255, 255, 0.08),
    shadow: ShadowTok { dy: 18.0, blur: 40.0, color: Rgba::new(0, 0, 0, 0.35) },
    menu_surface: Rgba::hex(0x16161b),
    menu_shadow: [
        ShadowTok { dy: 16.0, blur: 40.0, color: Rgba::new(0, 0, 0, 0.55) },
        ShadowTok { dy: 4.0, blur: 12.0, color: Rgba::new(0, 0, 0, 0.35) },
    ],
    modal_tint: Rgba::new(0, 0, 0, 0.3),
    scrim: Rgba::new(0, 0, 0, 0.55),
    page_bg: Rgba::hex(0x0a0a0c),
    page_fg: Rgba::hex(0xededf0),
    mesh_violet: Rgba::new(139, 92, 246, 0.26),
    mesh_pink: Rgba::new(236, 72, 153, 0.2),
    mesh_cyan: Rgba::new(6, 182, 212, 0.18),
    mesh_corner_top: Rgba::new(99, 102, 241, 0.14),
    mesh_corner_bottom: Rgba::new(79, 70, 229, 0.3),
};

pub const fn tokens(dark: bool) -> &'static Tokens {
    if dark { &DARK } else { &LIGHT }
}

/// How much of `--glass-page-bg` covers the acrylic behind the window (so text stays legible over
/// any wallpaper). Opaque mode paints it at full strength.
pub const PAGE_ALPHA_LIGHT: f32 = 0.80;
pub const PAGE_ALPHA_DARK: f32 = 0.82;

/// Opaque until a system backdrop is confirmed, so the window is never see-through without one.
const OPAQUE_DEFAULT: bool = true;
static OPAQUE: AtomicBool = AtomicBool::new(OPAQUE_DEFAULT);

/// Switch the backdrop between "translucent over the Windows acrylic" and "opaque page colour".
/// Opaque is the default until the window has confirmed a system backdrop, so the window is never
/// left see-through without one.
pub fn set_opaque(opaque: bool) {
    OPAQUE.store(opaque, Ordering::Relaxed);
}

pub fn is_opaque() -> bool {
    OPAQUE.load(Ordering::Relaxed)
}

/// Everything a painter needs to know about the window it paints into.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Gl {
    pub dark: bool,
    /// Window size in points: the `pageW`/`pageH` of the refraction field and the frame of the mesh.
    pub window: Vec2,
}

impl Gl {
    pub fn new(ctx: &egui::Context, dark: bool) -> Self {
        Self { dark, window: ctx.content_rect().size() }
    }
    pub fn t(&self) -> &'static Tokens {
        tokens(self.dark)
    }
    pub fn page_alpha(&self) -> f32 {
        self.page_alpha_for(is_opaque())
    }
    /// Page colour alpha for an explicit mode (pure; `page_alpha` reads the process-wide mode).
    pub fn page_alpha_for(&self, opaque: bool) -> f32 {
        if opaque {
            1.0
        } else if self.dark {
            PAGE_ALPHA_DARK
        } else {
            PAGE_ALPHA_LIGHT
        }
    }
}

// ------------------------------------------------------------------------------- colour plumbing

/// Premultiplied float colour (r, g, b, a), each 0..=1. Gradients and `over` are done here, in
/// gamma space like CSS and egui, and quantised once per vertex.
type Px = [f32; 4];

fn over(top: Px, bottom: Px) -> Px {
    let k = 1.0 - top[3];
    [top[0] + bottom[0] * k, top[1] + bottom[1] * k, top[2] + bottom[2] * k, top[3] + bottom[3] * k]
}

fn scale(p: Px, k: f32) -> Px {
    [p[0] * k, p[1] * k, p[2] * k, p[3] * k]
}

fn lerp_px(a: Px, b: Px, t: f32) -> Px {
    [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t, a[2] + (b[2] - a[2]) * t, a[3] + (b[3] - a[3]) * t]
}

fn to_color(p: Px) -> Color32 {
    let q = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
    // Premultiplied channels may not exceed alpha after rounding.
    let a = q(p[3]);
    Color32::from_rgba_premultiplied(q(p[0]).min(a), q(p[1]).min(a), q(p[2]).min(a), a)
}

fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

// ------------------------------------------------------------------------------------ ambient mesh

/// One `radial-gradient(<rx> <ry> at <x> <y>, color, transparent <stop>)` of
/// `.liquid-glass-backdrop::before`. Sizes in rem, positions as fractions of the window.
#[derive(Clone, Copy, Debug)]
struct Glow {
    x: f32,
    y: f32,
    rx: f32,
    ry: f32,
    stop: f32,
}

/// Top layer first, as in the CSS.
const GLOWS: [Glow; 5] = [
    Glow { x: 0.42, y: 0.38, rx: 40.0, ry: 32.0, stop: 0.60 },
    Glow { x: 0.62, y: 0.52, rx: 34.0, ry: 30.0, stop: 0.62 },
    Glow { x: 0.48, y: 0.66, rx: 38.0, ry: 34.0, stop: 0.62 },
    Glow { x: 0.12, y: -0.10, rx: 60.0, ry: 40.0, stop: 0.60 },
    Glow { x: 0.95, y: 1.10, rx: 50.0, ry: 36.0, stop: 0.60 },
];

fn glow_colors(t: &Tokens) -> [Rgba; 5] {
    [t.mesh_violet, t.mesh_pink, t.mesh_cyan, t.mesh_corner_top, t.mesh_corner_bottom]
}

/// Strength of one glow at `p`: 1 at the centre, linear to 0 at `stop` of the ellipse radius.
fn glow_falloff(g: &Glow, p: Pos2, window: Vec2) -> f32 {
    let dx = (p.x - g.x * window.x) / (g.rx * REM);
    let dy = (p.y - g.y * window.y) / (g.ry * REM);
    (1.0 - (dx * dx + dy * dy).sqrt() / g.stop).max(0.0)
}

fn mesh_px(p: Pos2, t: &Tokens, window: Vec2) -> Px {
    let cols = glow_colors(t);
    let mut acc: Px = [0.0; 4];
    for i in (0..GLOWS.len()).rev() {
        let f = glow_falloff(&GLOWS[i], p, window);
        if f > 0.0 {
            acc = over(cols[i].px(f), acc);
        }
    }
    acc
}

/// The ambient mesh at `p` (window points) as a premultiplied colour: the five radial glows of
/// `.liquid-glass-backdrop::before` composited over each other, with no page colour underneath.
pub fn mesh_color(p: Pos2, dark: bool, window: Vec2) -> Color32 {
    to_color(mesh_px(p, tokens(dark), window))
}

/// What a panel sees behind it at `p`: the mesh over `--glass-page-bg` at `page_alpha`.
fn backdrop_px(p: Pos2, t: &Tokens, window: Vec2, page_alpha: f32) -> Px {
    over(mesh_px(p, t, window), t.page_bg.px(page_alpha))
}

/// Backdrop colour with the page fully opaque: the stand-in for the heavy blur behind menus and
/// modals (a blurred smooth gradient is the gradient), so text under them never shows through.
fn solid_backdrop_px(p: Pos2, t: &Tokens, window: Vec2) -> Px {
    backdrop_px(p, t, window, 1.0)
}

/// Mesh grid cells: 24 x 16 as specified.
const MESH_COLS: usize = 24;
const MESH_ROWS: usize = 16;

/// The page layer: `--glass-page-bg` (partial alpha over the acrylic, opaque in fallback mode)
/// and the five glows as a vertex-coloured grid. Paint it first, under all content.
pub fn backdrop_shapes(g: &Gl, screen: Rect) -> Vec<Shape> {
    let t = g.t();
    let mut shapes = vec![Shape::rect_filled(screen, 0.0, t.page_bg.scaled(g.page_alpha()))];
    let mut mesh = Mesh::default();
    mesh.vertices.reserve((MESH_COLS + 1) * (MESH_ROWS + 1));
    // Positions are window-relative, but the shapes live in the screen rect: offset by its min.
    for j in 0..=MESH_ROWS {
        for i in 0..=MESH_COLS {
            let local = pos2(g.window.x * i as f32 / MESH_COLS as f32, g.window.y * j as f32 / MESH_ROWS as f32);
            let color = to_color(mesh_px(local, t, g.window));
            mesh.vertices.push(Vertex { pos: screen.min + local.to_vec2(), uv: WHITE_UV, color });
        }
    }
    let stride = (MESH_COLS + 1) as u32;
    for j in 0..MESH_ROWS as u32 {
        for i in 0..MESH_COLS as u32 {
            let a = j * stride + i;
            mesh.indices.extend_from_slice(&[a, a + 1, a + stride, a + 1, a + stride + 1, a + stride]);
        }
    }
    shapes.push(Shape::mesh(mesh));
    shapes
}

// --------------------------------------------------------------------------------- refraction field

/// `GLASS_PRESET` of `liquid-glass.directive.ts` (uniforms of the upstream shader).
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
    pub blur_radius: f32,
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

/// Distance in points from `(px, py)` to the edge of a `w x h` rounded rectangle of radius `r`,
/// measured inward and clamped at the edge (`distPx` of the reference).
pub fn dist_px(px: f32, py: f32, w: f32, h: f32, r: f32) -> f32 {
    let tx = (px - w / 2.0).abs() - (w / 2.0 - r);
    let ty = (py - h / 2.0).abs() - (h / 2.0 - r);
    let outside = tx.max(0.0).hypot(ty.max(0.0));
    let inside = tx.max(ty).min(0.0);
    (-(outside + inside - r)).max(0.0)
}

/// Refraction offset in points at `(px, py)` inside a `w x h` panel of radius `r`, for a window
/// of `page_w x page_h` (displacement is viewport-proportional, as in the reference).
pub fn displacement(px: f32, py: f32, w: f32, h: f32, r: f32, page_w: f32, page_h: f32, cfg: &Preset) -> Vec2 {
    let (cx, cy) = (px / w, py / h);
    let d = dist_px(px, py, w, h, r);
    let edge_fall = (-d * cfg.edge_distance).exp();
    let rim_fall = (-d * cfg.rim_distance).exp();
    let base_fall = 1.0 - (-d * cfg.base_distance).exp();
    let base = if cfg.warp { base_fall * cfg.base_intensity } else { 0.0 };
    let total = base + edge_fall * cfg.edge_intensity + rim_fall * cfg.rim_intensity;

    let (mut nx, mut ny) = (cx - 0.5, cy - 0.5);
    let len = nx.hypot(ny);
    if len > 0.0 {
        nx /= len;
        ny /= len;
    }
    let min_dim = w.min(h);
    let corner_norm = cx.min(1.0 - cx).max(cy.min(1.0 - cy)) * min_dim;
    let corner = (-corner_norm * 0.3).exp() * cfg.corner_boost;
    let ripple = ((d / min_dim) * 25.0).sin() * cfg.ripple_effect * rim_fall;
    vec2((nx * (total + corner) - ny * ripple) * page_w, (ny * (total + corner) + nx * ripple) * page_h)
}

// ------------------------------------------------------------------------------------- outline

/// A rounded rectangle as a closed loop of samples, each with an outward unit normal. A sample
/// offset by `d` (positive = outward) sits at `c + (r + d) n`, so any number of rows at different
/// offsets share vertex indices and can be stitched into strips.
#[derive(Debug)]
pub struct Outline {
    pub w: f32,
    pub h: f32,
    pub r: f32,
    c: Vec<Vec2>,
    n: Vec<Vec2>,
}

impl Outline {
    pub fn new(w: f32, h: f32, r: f32) -> Self {
        let r = r.min(w / 2.0).min(h / 2.0).max(0.0);
        let k = if r < 0.75 { 1 } else { ((r / 2.5).ceil() as usize).clamp(2, 8) };
        let corners = [(vec2(r, r), 180.0f32), (vec2(w - r, r), 270.0), (vec2(w - r, h - r), 0.0), (vec2(r, h - r), 90.0)];
        let dir = |deg: f32| {
            let a = deg.to_radians();
            vec2(a.cos(), a.sin())
        };
        let (mut c, mut n) = (Vec::new(), Vec::new());
        for (ci, (ctr, a0)) in corners.iter().enumerate() {
            for i in 0..=k {
                c.push(*ctr);
                n.push(dir(a0 + 90.0 * i as f32 / k as f32));
            }
            // Straight edge to the next corner, subdivided so the field stays smooth on long edges.
            let (next_ctr, next_a0) = corners[(ci + 1) % 4];
            let nrm = dir(a0 + 90.0);
            let from = *ctr + nrm * r;
            let to = next_ctr + dir(next_a0) * r;
            let m = ((to - from).length() / 64.0).floor() as usize;
            for j in 1..=m {
                let t = j as f32 / (m + 1) as f32;
                let pt = from + (to - from) * t;
                c.push(pt - nrm * r);
                n.push(nrm);
            }
        }
        Self { w, h, r, c, n }
    }

    pub fn len(&self) -> usize {
        self.c.len()
    }

    /// Sample `k` offset outwards by `d` (negative = inward), relative to the rect's min corner.
    pub fn point(&self, k: usize, d: f32) -> Vec2 {
        self.c[k] + self.n[k] * (self.r + d).max(0.0)
    }

    pub fn normal(&self, k: usize) -> Vec2 {
        self.n[k]
    }

    /// Deepest inward offset that keeps the offset outline well formed.
    pub fn max_depth(&self) -> f32 {
        self.r.min(self.w / 2.0 - 0.5).min(self.h / 2.0 - 0.5).max(0.0)
    }
}

// ------------------------------------------------------------------------------------- caches

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SizeKey {
    w: u32,
    h: u32,
    r: u32,
}

/// Quantise to quarter points so sub-pixel jitter never rebuilds geometry.
fn q4(v: f32) -> u32 {
    (v * 4.0).round().max(0.0) as u32
}

impl SizeKey {
    pub fn new(w: f32, h: f32, r: f32) -> Self {
        Self { w: q4(w), h: q4(h), r: q4(r) }
    }
}

/// Key of a refraction band: (rect size, radius, window size, theme), as specified.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct BandKey {
    pub size: SizeKey,
    pub win_w: u32,
    pub win_h: u32,
    pub dark: bool,
}

impl BandKey {
    pub fn new(size: Vec2, radius: f32, window: Vec2, dark: bool) -> Self {
        Self { size: SizeKey::new(size.x, size.y, radius), win_w: q4(window.x), win_h: q4(window.y), dark }
    }
}

/// Band depth rows (points inside the rim) and the fade weight of each.
const BAND_ROWS: [f32; 6] = [0.0, 0.75, 1.75, 3.25, 5.25, 8.0];
const BAND_DEPTH: f32 = 8.0;

/// Cached geometry of one refraction band: per vertex the local position, its displacement and
/// its fade weight. Colours are looked up per draw, since they depend on where the panel sits.
struct Band {
    rows: usize,
    n: usize,
    pos: Vec<Vec2>,
    disp: Vec<Vec2>,
    weight: Vec<f32>,
    indices: Vec<u32>,
}

#[derive(Default)]
struct Cache {
    outlines: HashMap<SizeKey, Rc<Outline>>,
    shadows: HashMap<(SizeKey, u32, u32), Rc<Mesh>>,
    bands: HashMap<BandKey, Rc<Band>>,
}

thread_local! {
    static CACHE: RefCell<Cache> = RefCell::new(Cache::default());
}

const CACHE_LIMIT: usize = 96;

fn outline(w: f32, h: f32, r: f32) -> Rc<Outline> {
    CACHE.with(|c| {
        let mut c = c.borrow_mut();
        if c.outlines.len() > CACHE_LIMIT {
            c.outlines.clear();
        }
        c.outlines.entry(SizeKey::new(w, h, r)).or_insert_with(|| Rc::new(Outline::new(w, h, r))).clone()
    })
}

/// Number of cached refraction bands (tests).
pub fn band_cache_len() -> usize {
    CACHE.with(|c| c.borrow().bands.len())
}

fn band(o: &Outline, size: Vec2, window: Vec2, dark: bool) -> Rc<Band> {
    let key = BandKey::new(size, o.r, window, dark);
    CACHE.with(|c| {
        let mut c = c.borrow_mut();
        if c.bands.len() > CACHE_LIMIT {
            c.bands.clear();
        }
        c.bands.entry(key).or_insert_with(|| Rc::new(build_band(o, window))).clone()
    })
}

fn build_band(o: &Outline, window: Vec2) -> Band {
    let cap = (o.max_depth() / BAND_DEPTH).min(1.0);
    let rows = BAND_ROWS.len();
    let n = o.len();
    let (mut pos, mut disp, mut weight) = (Vec::with_capacity(rows * n), Vec::with_capacity(rows * n), Vec::with_capacity(rows * n));
    for depth in BAND_ROWS {
        let depth = depth * cap;
        let w = (1.0 - depth / (BAND_DEPTH * cap).max(1e-3)).clamp(0.0, 1.0);
        for k in 0..n {
            let p = o.point(k, -depth);
            pos.push(p);
            disp.push(displacement(p.x, p.y, o.w, o.h, o.r, window.x, window.y, &GLASS_PRESET));
            weight.push(w * w);
        }
    }
    Band { rows, n, pos, disp, weight, indices: strip_indices(rows, n) }
}

/// Triangles for `rows` rows of `n` closed-loop samples.
fn strip_indices(rows: usize, n: usize) -> Vec<u32> {
    let mut idx = Vec::with_capacity((rows - 1) * n * 6);
    for r in 0..rows - 1 {
        for k in 0..n {
            let k2 = (k + 1) % n;
            let (a, b, c, d) = ((r * n + k) as u32, (r * n + k2) as u32, ((r + 1) * n + k) as u32, ((r + 1) * n + k2) as u32);
            idx.extend_from_slice(&[a, b, c, b, d, c]);
        }
    }
    idx
}

// ----------------------------------------------------------------------------------- mesh builders

fn push_vertex(m: &mut Mesh, pos: Pos2, p: Px) {
    m.vertices.push(Vertex { pos, uv: WHITE_UV, color: to_color(p) });
}

/// Filled rounded rect as a fan from the centre, colour `f(local position)`, with a 1 px feather
/// (full colour 0.5 inside the edge, transparent 0.5 outside) like egui's own fills.
fn fill_fan(o: &Outline, origin: Pos2, f: impl Fn(Vec2) -> Px) -> Mesh {
    let n = o.len();
    let mut m = Mesh::default();
    let centre = vec2(o.w / 2.0, o.h / 2.0);
    push_vertex(&mut m, origin + centre, f(centre));
    for k in 0..n {
        let p = o.point(k, -0.5);
        push_vertex(&mut m, origin + p, f(p));
    }
    for k in 0..n {
        let p = o.point(k, 0.5);
        push_vertex(&mut m, origin + p, scale(f(o.point(k, -0.5)), 0.0));
    }
    for k in 0..n as u32 {
        let k2 = (k + 1) % n as u32;
        m.indices.extend_from_slice(&[0, 1 + k, 1 + k2]);
        // Feather ring between the two outlines.
        let (a, b, c, d) = (1 + k, 1 + k2, 1 + n as u32 + k, 1 + n as u32 + k2);
        m.indices.extend_from_slice(&[a, b, c, b, d, c]);
    }
    m
}

/// CSS `linear-gradient(<angle>deg, top, bottom)` parameter at `p` inside a `w x h` box.
fn gradient_t(angle_deg: f32, p: Vec2, w: f32, h: f32) -> f32 {
    let a = angle_deg.to_radians();
    let dir = vec2(a.sin(), -a.cos());
    let len = (w * a.sin()).abs() + (h * a.cos()).abs();
    if len <= 0.0 {
        return 0.5;
    }
    (((p.x - w / 2.0) * dir.x + (p.y - h / 2.0) * dir.y) / len + 0.5).clamp(0.0, 1.0)
}

/// Gaussian-ish coverage of a blurred edge: 1 well inside (`s` < 0), 0 well outside.
fn blur_coverage(s: f32, sigma: f32) -> f32 {
    1.0 / (1.0 + (1.702 * s / sigma.max(1e-3)).exp())
}

/// Signed distance (negative inside) from a point to a rounded rect at the origin.
fn sdf_rrect(p: Vec2, w: f32, h: f32, r: f32) -> f32 {
    let tx = (p.x - w / 2.0).abs() - (w / 2.0 - r);
    let ty = (p.y - h / 2.0).abs() - (h / 2.0 - r);
    tx.max(0.0).hypot(ty.max(0.0)) + tx.max(ty).min(0.0) - r
}

/// Outer `box-shadow 0 <dy> <blur> color`, drawn only outside the surface (CSS clips an outer
/// shadow to the outside of the border box; painting it under translucent glass would grey the
/// glass). A ring of rows going outwards, alpha from the blurred offset shape.
fn shadow_mesh(o: &Outline, sh: &ShadowTok) -> Mesh {
    let sigma = sh.blur / 2.0;
    let reach = sh.dy.abs() + 2.6 * sigma;
    const ROWS: usize = 9;
    let n = o.len();
    let mut m = Mesh::default();
    for i in 0..ROWS {
        let d = reach * (i as f32 / (ROWS - 1) as f32).powf(1.6);
        for k in 0..n {
            let p = o.point(k, d);
            // Distance to the shadow shape, which is the surface moved down by `dy`.
            let s = sdf_rrect(p - vec2(0.0, sh.dy), o.w, o.h, o.r);
            let a = blur_coverage(s, sigma);
            // Fade the last row to nothing so the tail has no visible end.
            let end = if i == ROWS - 1 { 0.0 } else { 1.0 };
            push_vertex(&mut m, Pos2::ZERO + p, sh.color.px(a * end));
        }
    }
    m.indices = strip_indices(ROWS, n);
    m
}

fn shadow_shape(o: &Outline, origin: Pos2, sh: &ShadowTok, layer: u32, tag: u32) -> Option<Shape> {
    if sh.color.a <= 0.0 {
        return None;
    }
    let key = (SizeKey::new(o.w, o.h, o.r), layer, tag);
    let mesh = CACHE.with(|c| {
        let mut c = c.borrow_mut();
        if c.shadows.len() > CACHE_LIMIT {
            c.shadows.clear();
        }
        c.shadows.entry(key).or_insert_with(|| Rc::new(shadow_mesh(o, sh))).clone()
    });
    let mut m = (*mesh).clone();
    for v in &mut m.vertices {
        v.pos += origin.to_vec2();
    }
    Some(Shape::mesh(m))
}

/// The refraction band: a strip inside the rim redrawn with the backdrop sampled through the
/// displacement field, faded into the undistorted interior.
fn band_shape(o: &Outline, origin: Pos2, size: Vec2, g: &Gl) -> Shape {
    let b = band(o, size, g.window, g.dark);
    let t = g.t();
    let alpha = g.page_alpha();
    let mut m = Mesh::default();
    m.vertices.reserve(b.rows * b.n);
    for i in 0..b.rows * b.n {
        let p = origin + b.pos[i];
        let sample = p + b.disp[i];
        let col = scale(backdrop_px(sample, t, g.window, alpha), b.weight[i]);
        push_vertex(&mut m, p, col);
    }
    m.indices = b.indices.clone();
    Shape::mesh(m)
}

/// Rim: `inset 0 1px 0 rim-top`, `inset 0 0 0 1px rim-hairline`, `inset 0 -1px 0 rim-bottom`,
/// inside the 1 px border. Each is a strip whose thickness follows the surface normal, so the top
/// highlight is 1 px on the flat top and tapers to nothing down the sides, as an inset shadow does.
fn rim_mesh(o: &Outline, origin: Pos2, t: &Tokens, top: bool, hairline: bool, bottom: bool, boost: f32) -> Mesh {
    const FEATHER: f32 = 0.4;
    let n = o.len();
    let cap = o.max_depth();
    let mut m = Mesh::default();
    let add = |m: &mut Mesh, col: Rgba, thick: &dyn Fn(Vec2) -> f32| {
        let base = m.vertices.len() as u32;
        for row in 0..3 {
            for k in 0..n {
                let th = thick(o.normal(k));
                let (depth, a) = match row {
                    0 => (1.0, 1.0),
                    1 => (1.0 + th, 1.0),
                    _ => (1.0 + th + FEATHER, 0.0),
                };
                let p = o.point(k, -depth.min(cap));
                push_vertex(m, origin + p, col.px((a * boost).min(1.0)));
            }
        }
        m.indices.extend(strip_indices(3, n).into_iter().map(|ix| base + ix));
    };
    // CSS paints the first box-shadow on top: bottom, then hairline, then top.
    if bottom {
        add(&mut m, t.rim_bottom, &|nrm| nrm.y.max(0.0));
    }
    if hairline {
        add(&mut m, t.rim_hairline, &|_| 1.0);
    }
    if top {
        add(&mut m, t.rim_top, &|nrm| (-nrm.y).max(0.0));
    }
    m
}

fn border_shape(rect: Rect, radius: f32, color: Color32) -> Shape {
    Shape::rect_stroke(rect, egui::CornerRadius::same(radius.round().clamp(0.0, 255.0) as u8), Stroke::new(1.0, color), StrokeKind::Inside)
}

// ------------------------------------------------------------------------------------------ tiers

fn tint_px(t: &Tokens, hover: f32, y: f32) -> Px {
    let top = lerp_px(t.tint_top.px(1.0), t.tint_top_hover.px(1.0), hover);
    let bot = lerp_px(t.tint_bottom.px(1.0), t.tint_bottom_hover.px(1.0), hover);
    // Gradient in unmultiplied space is indistinguishable here (alpha is equal at both ends).
    lerp_px(top, bot, y)
}

/// Panel tier: shadow, refraction band, tint, border, rim. `hover` (0..=1) raises the tint from
/// 6 % to 11 %.
pub fn panel_shapes(g: &Gl, rect: Rect, radius: f32, hover: f32) -> Vec<Shape> {
    let t = g.t();
    let o = outline(rect.width(), rect.height(), radius);
    let origin = rect.min;
    let mut shapes = Vec::with_capacity(6);
    shapes.extend(shadow_shape(&o, origin, &t.shadow, 0, 0));
    shapes.push(band_shape(&o, origin, rect.size(), g));
    let h = rect.height().max(1.0);
    shapes.push(Shape::mesh(fill_fan(&o, origin, |p| tint_px(t, hover, p.y / h))));
    shapes.push(border_shape(rect, radius, t.border.color()));
    shapes.push(Shape::mesh(rim_mesh(&o, origin, t, true, true, true, 1.0)));
    shapes
}

/// Chip tier: tint + border + top rim, no shadow, no refraction. For anything small or repeated.
pub fn chip_shapes(g: &Gl, rect: Rect, radius: f32, hover: f32) -> Vec<Shape> {
    let t = g.t();
    let o = outline(rect.width(), rect.height(), radius);
    let h = rect.height().max(1.0);
    vec![
        Shape::mesh(fill_fan(&o, rect.min, |p| tint_px(t, hover, p.y / h))),
        border_shape(rect, radius, t.border.color()),
        Shape::mesh(rim_mesh(&o, rect.min, t, true, false, false, 1.0)),
    ]
}

/// Menu tier: dropdowns, popovers, tooltips. A 55 % wash of the menu surface over a solid stand-in
/// for the heavy backdrop blur, so labels are readable over anything.
pub fn menu_shapes(g: &Gl, rect: Rect, radius: f32) -> Vec<Shape> {
    let t = g.t();
    let o = outline(rect.width(), rect.height(), radius);
    let origin = rect.min;
    let mut shapes = Vec::with_capacity(6);
    for (i, sh) in t.menu_shadow.iter().enumerate() {
        shapes.extend(shadow_shape(&o, origin, sh, 1 + i as u32, 0));
    }
    let wash = t.menu_surface.px(0.55);
    // Window position of the rect is needed for the stand-in; the fan works in local space.
    shapes.push(Shape::mesh(fill_fan(&o, origin, |p| over(wash, solid_backdrop_px(origin + p, t, g.window)))));
    shapes.push(border_shape(rect, radius, t.border.color()));
    shapes.push(Shape::mesh(rim_mesh(&o, origin, t, true, false, false, 1.0)));
    shapes
}

/// Modal tier: a panel with the frost layer over the tint, on a solid stand-in for the scrimmed,
/// blurred page. Pair it with [`scrim_color`] as the modal backdrop.
pub fn modal_shapes(g: &Gl, rect: Rect, radius: f32) -> Vec<Shape> {
    let t = g.t();
    let o = outline(rect.width(), rect.height(), radius);
    let origin = rect.min;
    let h = rect.height().max(1.0);
    let frost = t.modal_tint.px(1.0);
    let scrim = t.scrim.px(1.0);
    let mut shapes = Vec::with_capacity(7);
    shapes.extend(shadow_shape(&o, origin, &t.shadow, 0, 1));
    // The page behind the modal, dimmed by the scrim and blurred away.
    shapes.push(Shape::mesh(fill_fan(&o, origin, |p| over(scrim, solid_backdrop_px(origin + p, t, g.window)))));
    shapes.push(band_shape(&o, origin, rect.size(), g));
    shapes.push(Shape::mesh(fill_fan(&o, origin, |p| over(frost, tint_px(t, 0.0, p.y / h)))));
    shapes.push(border_shape(rect, radius, t.border.color()));
    shapes.push(Shape::mesh(rim_mesh(&o, origin, t, true, true, true, 1.0)));
    shapes
}

/// `--glass-scrim`, the colour behind a modal.
pub fn scrim_color(dark: bool) -> Color32 {
    tokens(dark).scrim.color()
}

/// Coloured glass (primary / destructive buttons): only the tint is replaced, the rim and the
/// border stay. `base` is the brand colour; `hover` (0..=1) strengthens it.
pub fn colored_shapes(g: &Gl, rect: Rect, radius: f32, base: Color32, hover: f32, shadow: bool) -> Vec<Shape> {
    let t = g.t();
    let o = outline(rect.width(), rect.height(), radius);
    let origin = rect.min;
    let (r, gr, b) = (base.r(), base.g(), base.b());
    let dark = |c: u8| (c as f32 * 0.84) as u8;
    // recipes.md: linear-gradient(155deg, base .55, deeper .42); raised towards opaque because
    // white labels must stay legible on a light page (see docs/GLASS.md).
    let (a0, a1) = (lerp(0.90, 0.97, hover), lerp(0.84, 0.92, hover));
    let top = Rgba::new(r, gr, b, a0).px(1.0);
    let bot = Rgba::new(dark(r), dark(gr), dark(b), a1).px(1.0);
    let (w, h) = (rect.width(), rect.height());
    let mut shapes = Vec::with_capacity(5);
    if shadow {
        let sh = ShadowTok { dy: (h * 0.18).clamp(3.0, 12.0), blur: (h * 0.5).clamp(8.0, 28.0), color: Rgba::new(dark(r), dark(gr), dark(b), 0.34) };
        shapes.extend(shadow_shape(&o, origin, &sh, 3 + (h as u32 / 4), 2));
    }
    shapes.push(Shape::mesh(fill_fan(&o, origin, |p| lerp_px(top, bot, gradient_t(155.0, p, w, h)))));
    shapes.push(border_shape(rect, radius, Rgba::new(r, gr, b, 0.5).color()));
    let mut rim = rim_mesh(&o, origin, t, false, false, false, 1.0);
    // The recipe re-states the highlights on coloured glass: .40 top, .12 ring.
    let tt = Tokens { rim_top: Rgba::new(255, 255, 255, 0.4), rim_hairline: Rgba::new(255, 255, 255, 0.12), ..*t };
    let more = rim_mesh(&o, origin, &tt, true, true, false, 1.0);
    let base_ix = rim.vertices.len() as u32;
    rim.vertices.extend(more.vertices);
    rim.indices.extend(more.indices.iter().map(|i| base_ix + i));
    shapes.push(Shape::mesh(rim));
    shapes
}

/// Soft wash for hover / selection on a child of glass: a tint, no border, no rim (rule 3).
/// `level` 0 = barely there, 1 = hover, 2 = selected / pressed.
pub fn wash_color(dark: bool, level: u8) -> Color32 {
    let a = [0.04, 0.07, 0.11][level.min(2) as usize];
    if dark { Rgba::new(255, 255, 255, a * 1.15).color() } else { Rgba::new(30, 33, 45, a).color() }
}

/// A hairline stronger than `--glass-border`, for the bottom line of a text input (rule 3: inputs
/// inside glass have no box). Same hue as the border token, about 2.3x its alpha.
pub fn hairline_color(dark: bool) -> Color32 {
    let t = tokens(dark);
    Rgba { a: (t.border.a * if dark { 2.7 } else { 2.3 }).min(1.0), ..t.border }.color()
}

// ----------------------------------------------------------------------------- painter helpers

pub fn panel(painter: &Painter, g: &Gl, rect: Rect, radius: f32, hover: f32) {
    painter.extend(panel_shapes(g, rect, radius, hover));
}

pub fn chip(painter: &Painter, g: &Gl, rect: Rect, radius: f32, hover: f32) {
    painter.extend(chip_shapes(g, rect, radius, hover));
}

pub fn menu(painter: &Painter, g: &Gl, rect: Rect, radius: f32) {
    painter.extend(menu_shapes(g, rect, radius));
}

pub fn modal(painter: &Painter, g: &Gl, rect: Rect, radius: f32) {
    painter.extend(modal_shapes(g, rect, radius));
}

pub fn colored(painter: &Painter, g: &Gl, rect: Rect, radius: f32, base: Color32, hover: f32, shadow: bool) {
    painter.extend(colored_shapes(g, rect, radius, base, hover, shadow));
}

pub fn backdrop(painter: &Painter, g: &Gl, screen: Rect) {
    painter.extend(backdrop_shapes(g, screen));
}

// -------------------------------------------------------------------------------- Windows backdrop

/// DWM system backdrop: real blur of the desktop behind the window (the native `backdrop-filter`).
#[cfg(windows)]
pub mod sys {
    use std::ffi::c_void;
    use windows::Win32::Foundation::HWND;
    use windows::Win32::Graphics::Dwm::{
        DWM_SYSTEMBACKDROP_TYPE, DWMWA_SYSTEMBACKDROP_TYPE, DWMWA_USE_IMMERSIVE_DARK_MODE, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND, DWMSBT_TRANSIENTWINDOW,
        DwmExtendFrameIntoClientArea, DwmSetWindowAttribute,
    };
    use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
    use windows::Win32::System::SystemInformation::OSVERSIONINFOW;
    use windows::Win32::UI::Controls::MARGINS;
    use windows::core::{s, w};

    /// First Windows 11 build with `DWMWA_SYSTEMBACKDROP_TYPE` (22H2).
    pub const MIN_BUILD: u32 = 22621;

    /// The real OS build number (`RtlGetVersion` is not subject to the manifest compatibility shim
    /// that makes `GetVersionEx` lie), or 0 when it cannot be read.
    pub fn os_build() -> u32 {
        type RtlGetVersion = unsafe extern "system" fn(*mut OSVERSIONINFOW) -> i32;
        // SAFETY: ntdll is always loaded; the looked-up symbol has this documented signature and the
        // struct is zeroed with its size field set before the call.
        unsafe {
            let Ok(ntdll) = GetModuleHandleW(w!("ntdll.dll")) else { return 0 };
            let Some(f) = GetProcAddress(ntdll, s!("RtlGetVersion")) else { return 0 };
            let f: RtlGetVersion = std::mem::transmute(f);
            let mut info = OSVERSIONINFOW { dwOSVersionInfoSize: std::mem::size_of::<OSVERSIONINFOW>() as u32, ..Default::default() };
            if f(&mut info) != 0 { 0 } else { info.dwBuildNumber }
        }
    }

    /// `Settings > Personalization > Colors > Transparency effects` (HKCU `EnableTransparency`).
    /// When off, Windows paints acrylic as a flat colour, so we paint our own opaque page instead.
    pub fn transparency_effects_enabled() -> bool {
        use windows::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RegGetValueW};
        let mut data: u32 = 1;
        let mut len = std::mem::size_of::<u32>() as u32;
        // SAFETY: `data`/`len` are valid for writes of a DWORD for the duration of the call.
        let r = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                w!("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize"),
                w!("EnableTransparency"),
                RRF_RT_REG_DWORD,
                None,
                Some(&mut data as *mut u32 as *mut _),
                Some(&mut len),
            )
        };
        r.is_err() || data != 0
    }

    fn hwnd(raw: isize) -> HWND {
        HWND(raw as *mut c_void)
    }

    /// Ask DWM for the acrylic system backdrop behind the whole client area and match its tint to
    /// the theme. Returns true only if the OS supports it and accepted the request.
    pub fn enable_acrylic(raw: isize, dark: bool) -> bool {
        if raw == 0 || os_build() < MIN_BUILD {
            return false;
        }
        let h = hwnd(raw);
        let backdrop: DWM_SYSTEMBACKDROP_TYPE = DWMSBT_TRANSIENTWINDOW;
        let round = DWMWCP_ROUND;
        // SAFETY: each pointer is to a live local of exactly the size passed.
        unsafe {
            if DwmSetWindowAttribute(h, DWMWA_SYSTEMBACKDROP_TYPE, &backdrop as *const _ as *const c_void, std::mem::size_of::<DWM_SYSTEMBACKDROP_TYPE>() as u32).is_err() {
                return false;
            }
            let margins = MARGINS { cxLeftWidth: -1, cxRightWidth: -1, cyTopHeight: -1, cyBottomHeight: -1 };
            let _ = DwmExtendFrameIntoClientArea(h, &margins);
            let _ = DwmSetWindowAttribute(h, DWMWA_WINDOW_CORNER_PREFERENCE, &round as *const _ as *const c_void, std::mem::size_of_val(&round) as u32);
        }
        set_dark(raw, dark);
        true
    }

    /// `DWMWA_USE_IMMERSIVE_DARK_MODE`: darkens the acrylic tint to match the theme.
    pub fn set_dark(raw: isize, dark: bool) {
        if raw == 0 {
            return;
        }
        let v: i32 = dark as i32;
        // SAFETY: `v` is a live i32 and the size passed is its size.
        unsafe {
            let _ = DwmSetWindowAttribute(hwnd(raw), DWMWA_USE_IMMERSIVE_DARK_MODE, &v as *const _ as *const c_void, std::mem::size_of::<i32>() as u32);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WIN: Vec2 = vec2(1200.0, 800.0);

    fn close(a: f32, b: f32, eps: f32) -> bool {
        (a - b).abs() <= eps
    }

    #[test]
    fn tokens_match_the_stylesheet_in_both_themes() {
        // Light
        assert_eq!(LIGHT.tint_top, Rgba::new(255, 255, 255, 0.06));
        assert_eq!(LIGHT.tint_bottom, Rgba::new(179, 179, 179, 0.06));
        assert_eq!(LIGHT.tint_top_hover.a, 0.11);
        assert_eq!(LIGHT.rim_top, Rgba::new(255, 255, 255, 0.55));
        assert_eq!(LIGHT.rim_hairline.a, 0.12);
        assert_eq!(LIGHT.rim_bottom.a, 0.06);
        assert_eq!(LIGHT.border, Rgba::new(30, 33, 45, 0.12));
        assert_eq!((LIGHT.shadow.dy, LIGHT.shadow.blur, LIGHT.shadow.color), (10.0, 30.0, Rgba::new(30, 33, 45, 0.1)));
        assert_eq!(LIGHT.menu_surface, Rgba::hex(0xf6f7f9));
        assert_eq!((LIGHT.menu_shadow[0].dy, LIGHT.menu_shadow[0].blur, LIGHT.menu_shadow[0].color.a), (12.0, 32.0, 0.14));
        assert_eq!((LIGHT.menu_shadow[1].dy, LIGHT.menu_shadow[1].blur, LIGHT.menu_shadow[1].color.a), (4.0, 10.0, 0.07));
        assert_eq!(LIGHT.modal_tint, Rgba::new(255, 255, 255, 0.44));
        assert_eq!(LIGHT.scrim, Rgba::new(20, 22, 30, 0.35));
        assert_eq!(LIGHT.page_bg, Rgba::hex(0xe8eaef));
        assert_eq!(LIGHT.page_fg, Rgba::hex(0x2b2f3a));
        assert_eq!(LIGHT.mesh_violet, Rgba::new(139, 92, 246, 0.2));
        assert_eq!(LIGHT.mesh_pink, Rgba::new(236, 72, 153, 0.16));
        assert_eq!(LIGHT.mesh_cyan, Rgba::new(6, 182, 212, 0.14));
        assert_eq!(LIGHT.mesh_corner_top, Rgba::new(79, 70, 229, 0.05));
        assert_eq!(LIGHT.mesh_corner_bottom, Rgba::new(67, 56, 202, 0.06));
        // Dark
        assert_eq!(DARK.rim_top, Rgba::new(255, 255, 255, 0.5));
        assert_eq!(DARK.rim_hairline.a, 0.1);
        assert_eq!(DARK.border, Rgba::new(255, 255, 255, 0.08));
        assert_eq!((DARK.shadow.dy, DARK.shadow.blur, DARK.shadow.color), (18.0, 40.0, Rgba::new(0, 0, 0, 0.35)));
        assert_eq!(DARK.menu_surface, Rgba::hex(0x16161b));
        assert_eq!((DARK.menu_shadow[0].dy, DARK.menu_shadow[0].blur, DARK.menu_shadow[0].color.a), (16.0, 40.0, 0.55));
        assert_eq!((DARK.menu_shadow[1].dy, DARK.menu_shadow[1].blur, DARK.menu_shadow[1].color.a), (4.0, 12.0, 0.35));
        assert_eq!(DARK.modal_tint, Rgba::new(0, 0, 0, 0.3));
        assert_eq!(DARK.scrim, Rgba::new(0, 0, 0, 0.55));
        assert_eq!(DARK.page_bg, Rgba::hex(0x0a0a0c));
        assert_eq!(DARK.page_fg, Rgba::hex(0xededf0));
        assert_eq!(DARK.mesh_violet, Rgba::new(139, 92, 246, 0.26));
        assert_eq!(DARK.mesh_pink, Rgba::new(236, 72, 153, 0.2));
        assert_eq!(DARK.mesh_cyan, Rgba::new(6, 182, 212, 0.18));
        assert_eq!(DARK.mesh_corner_top, Rgba::new(99, 102, 241, 0.14));
        assert_eq!(DARK.mesh_corner_bottom, Rgba::new(79, 70, 229, 0.3));
    }

    #[test]
    fn the_glass_is_colourless_and_identical_across_themes() {
        // Rule 2: the tint is the same white -> 70 % grey in both themes; only the frame changes.
        assert_eq!(LIGHT.tint_top, DARK.tint_top);
        assert_eq!(LIGHT.tint_bottom, DARK.tint_bottom);
        assert_eq!(LIGHT.tint_top_hover, DARK.tint_top_hover);
        assert_eq!(LIGHT.rim_bottom, DARK.rim_bottom);
        assert_ne!(LIGHT.border, DARK.border);
        assert_ne!(LIGHT.page_bg, DARK.page_bg);
        assert!(!LIGHT.dark && DARK.dark);
        assert_eq!(tokens(true), &DARK);
        assert_eq!(tokens(false), &LIGHT);
    }

    #[test]
    fn field_values_at_known_points() {
        // Panel 400 x 300, radius 12, window 1200 x 800.
        let (w, h, r) = (400.0, 300.0, 12.0);
        // Left edge midpoint: dist 0, normal points left (-x), corner term negligible.
        let d = displacement(0.0, 150.0, w, h, r, 1200.0, 800.0, &GLASS_PRESET);
        let want = -(0.015 + 0.028) * 1200.0 - ((-(0.0f32) * 0.3).exp() * 0.0); // ripple sin(0)=0
        // cx = 0 so the corner term is exp(-(max(0, 0.5) * 300) * 0.3) ~ 0.
        assert!(close(d.x, want, 0.05), "dx {} want {}", d.x, want);
        assert!(close(d.y, 0.0, 0.05), "dy {}", d.y);
        // Top edge midpoint: normal is -y, scaled by pageH.
        let d = displacement(200.0, 0.0, w, h, r, 1200.0, 800.0, &GLASS_PRESET);
        assert!(close(d.y, -(0.015 + 0.028) * 800.0, 0.05), "dy {}", d.y);
        assert!(close(d.x, 0.0, 0.05));
        // Top-left corner pixel: dist is 0, corner boost 0.06 adds on top and points to the corner.
        let d = displacement(0.0, 0.0, w, h, r, 1200.0, 800.0, &GLASS_PRESET);
        assert!(d.x < 0.0 && d.y < 0.0);
        assert!(d.x.abs() > 0.043 * 1200.0 * 0.7, "corner boost is on top of the rim: {}", d.x);
    }

    #[test]
    fn the_field_decays_away_from_the_rim_and_vanishes_in_the_middle() {
        let (w, h, r) = (500.0, 400.0, 12.0);
        let mag = |x: f32| displacement(x, 200.0, w, h, r, 1200.0, 800.0, &GLASS_PRESET).length();
        assert!(mag(0.0) > mag(1.0) && mag(1.0) > mag(3.0) && mag(3.0) > mag(8.0));
        // Warp is off: the interior is bias-free (exactly zero before the encoder's neutral).
        let mid = displacement(250.0, 200.0, w, h, r, 1200.0, 800.0, &GLASS_PRESET);
        assert!(mid.length() < 1e-3, "interior drifted by {}", mid.length());
        // Turning warp on is the only thing that moves the middle.
        let warp = Preset { warp: true, ..GLASS_PRESET };
        assert!(displacement(300.0, 200.0, w, h, r, 1200.0, 800.0, &warp).length() > 1.0);
    }

    #[test]
    fn rounded_rect_distance() {
        // On the edge and at the corner arc.
        assert_eq!(dist_px(0.0, 50.0, 100.0, 100.0, 10.0), 0.0);
        assert!(close(dist_px(5.0, 50.0, 100.0, 100.0, 10.0), 5.0, 1e-4));
        assert!(close(dist_px(50.0, 50.0, 100.0, 100.0, 10.0), 50.0, 1e-4));
        // The corner pixel of the bounding box lies outside the arc: clamped to 0.
        assert_eq!(dist_px(0.0, 0.0, 100.0, 100.0, 10.0), 0.0);
        // Along the diagonal the arc is closer than the straight distance.
        let arc_pt = 10.0 - 10.0 / 2f32.sqrt(); // the 45 degree point of the corner arc
        assert!(close(dist_px(arc_pt, arc_pt, 100.0, 100.0, 10.0), 0.0, 1e-3));
    }

    #[test]
    fn mesh_is_uneven_and_has_the_exact_geometry() {
        for dark in [false, true] {
            let t = tokens(dark);
            // Centre of the violet glow: 42 % / 38 % of the window.
            let c = pos2(0.42 * WIN.x, 0.38 * WIN.y);
            let at_violet = mesh_px(c, t, WIN);
            // The violet layer is on top: its own alpha is at least the token's.
            assert!(at_violet[3] >= t.mesh_violet.a - 1e-4);
            // 60 % of the 40rem x 32rem ellipse is the end of the falloff: nothing from violet.
            let g = GLOWS[0];
            assert_eq!(glow_falloff(&g, pos2(c.x + 0.6 * 40.0 * REM + 1.0, c.y), WIN), 0.0);
            assert!(close(glow_falloff(&g, c, WIN), 1.0, 1e-6));
            assert!(close(glow_falloff(&g, pos2(c.x + 0.3 * 40.0 * REM, c.y), WIN), 0.5, 1e-5));
            // The page must look uneven with no glass on it: corners differ from the glow cores.
            let far = mesh_px(pos2(5.0, 400.0), t, WIN);
            assert!((at_violet[3] - far[3]).abs() > 0.05, "dark={dark}: backdrop is flat");
        }
    }

    #[test]
    fn mesh_color_is_a_valid_premultiplied_colour() {
        for dark in [false, true] {
            for x in (0..=1200).step_by(60) {
                for y in (0..=800).step_by(80) {
                    let c = mesh_color(pos2(x as f32, y as f32), dark, WIN);
                    assert!(c.r() <= c.a() && c.g() <= c.a() && c.b() <= c.a());
                }
            }
        }
    }

    #[test]
    fn glow_positions_follow_the_window() {
        // The top-left corner glow sits above the window (-10 %), the bottom-right one below it.
        let small = vec2(600.0, 400.0);
        let a = mesh_px(pos2(0.12 * small.x, 0.0), &LIGHT, small)[3];
        let b = mesh_px(pos2(0.12 * WIN.x, 0.0), &LIGHT, WIN)[3];
        assert!(a > 0.0 && b > 0.0);
        // Same relative spot, different window: the falloff in pixels differs, so alpha does too.
        assert!((a - b).abs() > 1e-4);
    }

    #[test]
    fn outline_rows_are_aligned_and_offsets_are_exact() {
        let o = Outline::new(200.0, 100.0, 12.0);
        assert!(o.len() > 16);
        for k in 0..o.len() {
            let p0 = o.point(k, 0.0);
            // Every sample on the outline is on the rounded-rect boundary.
            assert!(sdf_rrect(p0, 200.0, 100.0, 12.0).abs() < 1e-3, "sample {k} off the outline");
            // An inward offset of d lands at signed distance -d (corner samples included).
            let p3 = o.point(k, -3.0);
            assert!(close(sdf_rrect(p3, 200.0, 100.0, 12.0), -3.0, 1e-3));
        }
        // A pill (r = h/2) keeps its samples finite.
        let pill = Outline::new(120.0, 32.0, 16.0);
        assert!(pill.max_depth() <= 15.5);
        assert!((0..pill.len()).all(|k| pill.point(k, -2.0).x.is_finite()));
    }

    #[test]
    fn band_cache_is_keyed_by_size_radius_window_and_theme() {
        let a = BandKey::new(vec2(400.0, 300.0), 12.0, WIN, false);
        assert_eq!(a, BandKey::new(vec2(400.01, 300.0), 12.0, WIN, false), "sub-quarter-point jitter shares an entry");
        assert_ne!(a, BandKey::new(vec2(401.0, 300.0), 12.0, WIN, false));
        assert_ne!(a, BandKey::new(vec2(400.0, 300.0), 20.0, WIN, false));
        assert_ne!(a, BandKey::new(vec2(400.0, 300.0), 12.0, vec2(1201.0, 800.0), false));
        assert_ne!(a, BandKey::new(vec2(400.0, 300.0), 12.0, WIN, true));

        let g = Gl { dark: false, window: WIN };
        let before = band_cache_len();
        let r1 = Rect::from_min_size(pos2(10.0, 10.0), vec2(400.0, 300.0));
        let r2 = Rect::from_min_size(pos2(500.0, 200.0), vec2(400.0, 300.0));
        let _ = panel_shapes(&g, r1, 12.0, 0.0);
        let _ = panel_shapes(&g, r2, 12.0, 0.0);
        assert_eq!(band_cache_len(), before + 1, "same size at another position reuses the band");
        let _ = panel_shapes(&Gl { dark: true, window: WIN }, r1, 12.0, 0.0);
        assert_eq!(band_cache_len(), before + 2, "the other theme is a different entry");
    }

    #[test]
    fn panel_band_has_a_few_hundred_vertices_and_valid_indices() {
        let g = Gl { dark: false, window: WIN };
        let rect = Rect::from_min_size(pos2(8.0, 64.0), vec2(184.0, 720.0));
        let shapes = panel_shapes(&g, rect, RADIUS, 0.0);
        let mut meshes = 0;
        for s in &shapes {
            if let Shape::Mesh(m) = s {
                meshes += 1;
                assert!(m.indices.iter().all(|&i| (i as usize) < m.vertices.len()));
                assert_eq!(m.indices.len() % 3, 0);
            }
        }
        assert!(meshes >= 4);
        let b = build_band(&outline(184.0, 720.0, 12.0), WIN);
        assert!(b.pos.len() >= 100 && b.pos.len() <= 1500, "{} band vertices", b.pos.len());
    }

    #[test]
    fn band_fades_into_the_interior() {
        let o = Outline::new(300.0, 200.0, 12.0);
        let b = build_band(&o, WIN);
        // First row (the rim) is fully weighted, the last (8 px in) is zero.
        assert!(b.weight[..b.n].iter().all(|&w| w == 1.0));
        assert!(b.weight[(b.rows - 1) * b.n..].iter().all(|&w| w == 0.0));
        // And the rim moves the sample point much more than the inner rows.
        let rim = b.disp[..b.n].iter().map(|d| d.length()).fold(0.0, f32::max);
        let inner = b.disp[(b.rows - 1) * b.n..].iter().map(|d| d.length()).fold(0.0, f32::max);
        assert!(rim > 40.0 && inner < rim / 4.0, "rim {rim} inner {inner}");
    }

    #[test]
    fn gradient_angles() {
        // 180deg runs top -> bottom.
        assert!(close(gradient_t(180.0, vec2(50.0, 0.0), 100.0, 40.0), 0.0, 1e-5));
        assert!(close(gradient_t(180.0, vec2(50.0, 40.0), 100.0, 40.0), 1.0, 1e-5));
        // 155deg is mostly vertical, slightly towards the right.
        let t_left = gradient_t(155.0, vec2(0.0, 20.0), 100.0, 40.0);
        let t_right = gradient_t(155.0, vec2(100.0, 20.0), 100.0, 40.0);
        assert!(t_right > t_left);
    }

    #[test]
    fn shadow_is_clipped_to_the_outside_and_offset_downwards() {
        let o = Outline::new(200.0, 100.0, 12.0);
        let m = shadow_mesh(&o, &LIGHT.shadow);
        let n = o.len();
        // Row 0 sits on the outline: alpha near the full shadow strength below, less above.
        let alpha = |k: usize| m.vertices[k].color.a();
        let (mut top_k, mut bottom_k) = (0, 0);
        for k in 0..n {
            if o.normal(k).y < -0.99 {
                top_k = k;
            }
            if o.normal(k).y > 0.99 {
                bottom_k = k;
            }
        }
        assert!(alpha(bottom_k) > alpha(top_k), "shadow is heavier below the surface");
        // Last row fades to nothing.
        assert!((0..n).all(|k| m.vertices[8 * n + k].color.a() == 0));
        // All geometry lies on or outside the outline (never under the glass).
        assert!(m.vertices.iter().all(|v| sdf_rrect(v.pos.to_vec2(), 200.0, 100.0, 12.0) >= -1e-3));
    }

    #[test]
    fn rim_top_tapers_down_the_sides() {
        let o = Outline::new(200.0, 100.0, 12.0);
        let m = rim_mesh(&o, Pos2::ZERO, &LIGHT, true, false, false, 1.0);
        let n = o.len();
        let thick = |k: usize| (m.vertices[n + k].pos - m.vertices[k].pos).length();
        let (mut top_k, mut side_k) = (0, 0);
        for k in 0..n {
            if o.normal(k).y < -0.99 {
                top_k = k;
            }
            if o.normal(k).x > 0.99 && o.normal(k).y.abs() < 1e-3 {
                side_k = k;
            }
        }
        assert!(close(thick(top_k), 1.0, 1e-3), "1 px on the flat top");
        assert!(thick(side_k) < 1e-3, "nothing on the sides");
    }

    #[test]
    fn colour_helpers_stay_premultiplied() {
        let c = Rgba::new(255, 255, 255, 0.06).color();
        assert_eq!(c.a(), 15);
        assert!(c.r() <= c.a());
        assert_eq!(over([0.0; 4], [0.2, 0.3, 0.4, 0.5]), [0.2, 0.3, 0.4, 0.5]);
        let o = over([0.1, 0.1, 0.1, 0.5], [0.2, 0.2, 0.2, 1.0]);
        assert!(close(o[3], 1.0, 1e-6) && close(o[0], 0.2, 1e-6));
        assert_eq!(wash_color(false, 0).a() < wash_color(false, 2).a(), true);
    }

    #[test]
    fn opaque_mode_paints_the_page_at_full_strength() {
        let (light, dark) = (Gl { dark: false, window: WIN }, Gl { dark: true, window: WIN });
        assert_eq!(light.page_alpha_for(true), 1.0);
        assert_eq!(dark.page_alpha_for(true), 1.0);
        assert_eq!(light.page_alpha_for(false), PAGE_ALPHA_LIGHT);
        assert_eq!(dark.page_alpha_for(false), PAGE_ALPHA_DARK);
        // The process-wide mode starts opaque (until a backdrop is confirmed): read it from a fresh
        // static, not from the shared one other tests flip.
        assert!(OPAQUE_DEFAULT);
    }

    #[cfg(windows)]
    #[test]
    fn os_build_is_readable() {
        assert!(sys::os_build() >= 10_000, "RtlGetVersion should report a Windows 10+ build");
    }
}
