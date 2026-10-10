//! Pure renderer: `render(state, t_ms)` -> premultiplied RGBA pixmap of the whole pill window.
//! No Win32 is touched except the (cached) GDI text masks used by the tooltip and the error card, so
//! the output is deterministic for goldens.
//!
//! A frame is drawn in four layer groups, always in this order: the signal glow ([`L_GLOW`]), the
//! static chrome ([`L_UNDER`]: app bubble, shadow, skin, body, logo, divider), the waveform bars
//! ([`L_BARS`]) and the overlays ([`L_OVER`]: unavailable ring, cancel glyph, tooltip).
//! [`FrameCache`] exploits that split so a steady frame redraws only what moves (see its docs).

use super::anim::*;
use super::card;
use super::glow::{self, GLOW_RADIUS, GlowFrame, Sprites};
use super::model::*;
use super::PillMode;
use super::raster::{Canvas, DevRect, Rect, any_alpha, clear_rect, copy_rect, cov, over_in_place, over_rect, sd_capsule, sd_round_box, sd_segment};
use super::text::{self, Font};
use super::theme::Rgba;
use std::hash::Hasher;
use std::sync::Arc;
use tiny_skia::{FilterQuality, Pixmap, PixmapPaint, Transform};

/// Layer groups (bit mask for [`render_layers`]).
pub const L_GLOW: u8 = 1;
pub const L_UNDER: u8 = 2;
pub const L_BARS: u8 = 4;
pub const L_OVER: u8 = 8;
pub const L_ALL: u8 = L_GLOW | L_UNDER | L_BARS | L_OVER;

/// Device-pixel size of the window for the current content.
pub fn pixel_size(st: &RenderState) -> (u32, u32) {
    let (w, h) = window_size(st);
    ((w * st.scale).ceil().max(1.0) as u32, (h * st.scale).ceil().max(1.0) as u32)
}

/// Render one frame into a fresh pixmap; `None` when the window size is not representable (a
/// degenerate scale), so a bad DPI/monitor query can never panic the daemon.
pub fn render_frame(st: &RenderState, t: f64) -> Option<Pixmap> {
    let (w, h) = pixel_size(st);
    let mut pm = Pixmap::new(w, h)?;
    render_into(st, t, &mut pm);
    Some(pm)
}

/// [`render_frame`] for tests and goldens, where a size failure is a bug.
#[cfg(test)]
pub fn render(st: &RenderState, t: f64) -> Pixmap {
    render_frame(st, t).expect("pill window size")
}

/// Render into an existing pixmap of exactly `pixel_size(st)`; the pixmap is cleared first.
pub fn render_into(st: &RenderState, t: f64, pm: &mut Pixmap) {
    render_layers(st, t, pm, L_ALL);
}

/// Like [`render_into`] but draws only the layer groups in `mask` (the error card ignores it).
pub fn render_layers(st: &RenderState, t: f64, pm: &mut Pixmap, mask: u8) {
    pm.data_mut().fill(0);
    if let Some(e) = st.error() {
        render_error(st, e, t, pm);
        return;
    }
    let g = st.geometry(t);
    let mut cv = Canvas::new(pm, st.scale);
    if mask & L_GLOW != 0 {
        draw_glow(&mut cv, st, &g, t);
    }
    if mask & L_UNDER != 0 {
        draw_under(&mut cv, st, &g, t);
    }
    if mask & L_BARS != 0 {
        draw_bars(&mut cv, st, &g, t, &st.waveform().heights(t));
    }
    if mask & L_OVER != 0 {
        draw_over(&mut cv, st, &g, t);
    }
}

/// 1. Signal glow sits behind everything.
fn draw_glow(cv: &mut Canvas, st: &RenderState, g: &Geometry, t: f64) {
    let glow_op = st.glow.value(t);
    if glow_op > 0.002 {
        let Some(sprites) = Sprites::get(st.scale) else { return };
        glow::draw(cv, &st.pal, &GlowFrame { pill: g.pill, opacity: glow_op, elapsed: st.glow_since.map(|s| t - s) }, &sprites);
    }
}

/// 2-5. App bubble, pill shadow, fused cancel skin, pill body and hover gradient; then the logo and
/// the divider.
fn draw_under(cv: &mut Canvas, st: &RenderState, g: &Geometry, t: f64) {
    let scale = st.scale;
    let pal = st.pal;
    let app = st.appearance_at(t);
    let fused = st.fused.value(t).clamp(0.0, 1.0);
    let chrome_alpha = 1.0 - fused;

    // 2. app bubble
    if g.bubble_t > 0.003 {
        if let Some(a) = st.app() {
            draw_bubble(cv, st, g, &app, a);
        }
    }

    // 3. pill shadow (fades with the chrome once the liquid skin owns the surface)
    let p = g.pill;
    if app.shadow.alpha > 0.001 && chrome_alpha > 0.002 {
        cv.shadow_sdf(p, app.shadow.dy, app.shadow.blur, app.shadow.spread, Rgba::new(0.0, 0.0, 0.0, 1.0), app.shadow.alpha * chrome_alpha, |x, y| {
            sd_capsule(x, y, p.cx(), p.cy(), p.w(), p.h())
        });
    }

    // 4. fused cancel skin
    if g.cancel_t > 0.0 || fused > 0.002 {
        draw_skin(cv, st, g, &app);
    }

    // 5. pill body
    if chrome_alpha > 0.002 {
        draw_chrome(cv, p, &app, chrome_alpha, scale);
    }
    let grad = st.grad.value(t);
    if grad > 0.002 {
        draw_hover_gradient(cv, p, pal.fg, grad);
    }

    // 6. contents that sit under the bars
    draw_logo_and_divider(cv, st, g, t);
}

/// Everything drawn after the waveform: unavailable ring, cancel glyph, tooltip.
fn draw_over(cv: &mut Canvas, st: &RenderState, g: &Geometry, t: f64) {
    let pal = st.pal;
    let p = g.pill;

    draw_unavailable_ring(cv, st, g, t);

    // 7. cancel glyph
    if g.cancel_t > 0.35 {
        let a = ((g.cancel_t - 0.35) / 0.65).clamp(0.0, 1.0);
        let hot = st.cancel_hot.value(t);
        let col = pal.muted_fg.lerp(pal.fg, hot);
        draw_x_glyph(cv, g.cancel.0, g.cancel.1, col, a);
    }

    // 8. tooltip
    if st.pointer().pill || st.pointer().bubble {
        if let Some(txt) = tooltip_for(st) {
            let anchor = if st.pointer().bubble && g.bubble_t > 0.3 {
                let (bx, by, br) = g.bubble;
                (Rect::new(bx - br, by - br, bx + br, by + br), Align::Center)
            } else {
                (p, align_for(st.position))
            };
            draw_tooltip(cv, st, &txt, anchor.0, anchor.1);
        }
    }
}

fn tooltip_for(st: &RenderState) -> Option<String> {
    if st.pointer().bubble && !st.pointer().pill {
        return st.app().map(|a| a.name.clone());
    }
    st.tooltip_text()
}

/// Capsule fill + 1 px inside border.
fn draw_chrome(cv: &mut Canvas, r: Rect, app: &Appearance, alpha: f32, scale: f32) {
    let (cx, cy, w, h) = (r.cx(), r.cy(), r.w(), r.h());
    let (bg, bd) = (app.bg, app.border);
    cv.shade_sdf(
        r.inflate(1.0),
        |x, y| sd_capsule(x, y, cx, cy, w, h),
        |x, y, c| {
            let ci = cov(sd_capsule(x, y, cx, cy, w - 2.0, h - 2.0) * scale);
            let ba = (c - ci).max(0.0) * bd.a;
            let pa = c * bg.a;
            let inv = 1.0 - ba;
            [
                (bd.r * ba + bg.r * pa * inv) * alpha,
                (bd.g * ba + bg.g * pa * inv) * alpha,
                (bd.b * ba + bg.b * pa * inv) * alpha,
                (ba + pa * inv) * alpha,
            ]
        },
    );
}

/// `bg-gradient-to-br from-foreground/10 to-transparent` clipped to the padding box.
fn draw_hover_gradient(cv: &mut Canvas, r: Rect, fg: Rgba, opacity: f32) {
    let (cx, cy, w, h) = (r.cx(), r.cy(), r.w() - 2.0, r.h() - 2.0);
    cv.shade_sdf(
        r,
        |x, y| sd_capsule(x, y, cx, cy, w, h),
        |x, y, c| {
            let t = (0.5 + ((x - cx) * h + (y - cy) * w) / (2.0 * w * h)).clamp(0.0, 1.0);
            let a = 0.1 * (1.0 - t) * opacity * c;
            [fg.r * a, fg.g * a, fg.b * a, a]
        },
    );
}

struct Contents {
    logo_x: f32,
    cy: f32,
    lane_x: f32,
    lane_w: f32,
    div_x: f32,
    div_w: f32,
}

fn contents_layout(g: &Geometry, divider: f32) -> Contents {
    let p = g.pill;
    let c = g.compact;
    let pr = 6.0 * c;
    let dm = 3.0 * c + divider;
    let dw = divider;
    let lane_w = LANE_W * c;
    let total = LOGO_SLOT + 2.0 * dm + dw + lane_w;
    let avail = p.w() - 2.0 - pr;
    let x0 = p.l + 1.0 + (avail - total) * 0.5;
    Contents {
        logo_x: x0,
        cy: p.cy(),
        lane_x: x0 + LOGO_SLOT + 2.0 * dm + dw,
        lane_w,
        div_x: x0 + LOGO_SLOT + dm,
        div_w: dw,
    }
}

fn draw_logo_and_divider(cv: &mut Canvas, st: &RenderState, g: &Geometry, t: f64) {
    let pal = st.pal;
    let divider = st.divider.value(t).clamp(0.0, 1.0);
    let ct = contents_layout(g, divider);

    // logo
    let logo_col = logo_color(st.look(), &pal);
    let op = st.pulse_opacity_at(t);
    draw_logo(cv, ct.logo_x, ct.cy, logo_col, op);

    // divider (bg-border/60)
    if ct.div_w > 0.02 && divider > 0.01 {
        let r = Rect::from_xywh(ct.div_x, ct.cy - 8.0, ct.div_w, 16.0);
        cv.fill_sdf(r.inflate(1.0), pal.border, 0.6 * divider, |x, y| sd_round_box(x, y, r.cx(), r.cy(), r.w() * 0.5, r.h() * 0.5, 0.0));
    }
}

/// The waveform lane: `hs` are the live bar heights (see [`Waveform::heights`]).
fn draw_bars(cv: &mut Canvas, st: &RenderState, g: &Geometry, t: f64, hs: &[f32; WAVE_BARS]) {
    let pal = st.pal;
    let ct = contents_layout(g, st.divider.value(t).clamp(0.0, 1.0));
    if ct.lane_w > 0.5 {
        let lane = Rect::from_xywh(ct.lane_x, ct.cy - LANE_H * 0.5, ct.lane_w, LANE_H);
        let bars_left = lane.cx() - LANE_W * 0.5;
        let clip = ct.lane_w < LANE_W - 0.01;
        let live = st.wave_live.value(t);
        let rest = st.wave_rest.value(t);
        if live > 0.003 {
            for (i, &h) in hs.iter().enumerate() {
                draw_bar(cv, bars_left + i as f32 * 5.0 + 1.0, ct.cy, h, pal.fg, live, clip.then_some(lane));
            }
        }
        if rest > 0.003 {
            for (i, &h) in RESTING_WAVE.iter().enumerate() {
                draw_bar(cv, bars_left + i as f32 * 5.0 + 1.0, ct.cy, h, pal.fg, rest, clip.then_some(lane));
            }
        }
    }
}

/// Unavailable: pulsing 2 px ring just inside the border.
fn draw_unavailable_ring(cv: &mut Canvas, st: &RenderState, g: &Geometry, t: f64) {
    if st.look() != Look::Unavailable {
        return;
    }
    let pal = st.pal;
    let op = st.pulse_opacity_at(t);
    let p = g.pill;
    let (cx, cy) = (p.cx(), p.cy());
    let (ow, oh) = (p.w() - 2.0, p.h() - 2.0);
    let s = cv.scale;
    cv.shade_sdf(
        p,
        |x, y| sd_capsule(x, y, cx, cy, ow, oh),
        |x, y, c| {
            let hole = cov(sd_capsule(x, y, cx, cy, ow - 4.0, oh - 4.0) * s);
            let a = (c - hole).max(0.0) * 0.3 * op;
            [pal.fg.r * a, pal.fg.g * a, pal.fg.b * a, a]
        },
    );
}

fn draw_bar(cv: &mut Canvas, cx: f32, cy: f32, h: f32, color: Rgba, alpha: f32, clip: Option<Rect>) {
    let bb = Rect::new(cx - 2.0, cy - h * 0.5 - 1.0, cx + 2.0, cy + h * 0.5 + 1.0);
    match clip {
        None => cv.fill_sdf(bb, color, alpha, |x, y| sd_capsule(x, y, cx, cy, 2.0, h)),
        Some(l) => cv.fill_sdf(bb, color, alpha, |x, y| {
            sd_capsule(x, y, cx, cy, 2.0, h).max(sd_round_box(x, y, l.cx(), l.cy(), l.w() * 0.5, l.h() * 0.5, 0.0))
        }),
    }
}

/// The voice logo: ring r=10 and three bars at 22 px (24-unit viewBox, stroke 2, round caps).
pub fn draw_logo(cv: &mut Canvas, x: f32, cy: f32, color: Rgba, opacity: f32) {
    let k = LOGO_SLOT / 24.0;
    let cx = x + 12.0 * k;
    let sw = 2.0 * k;
    let r = 10.0 * k;
    let bb = Rect::new(x - 1.0, cy - 12.0 * k - 1.0, x + LOGO_SLOT + 1.0, cy + 12.0 * k + 1.0);
    cv.fill_sdf(bb, color, opacity, |px, py| ((px - cx).hypot(py - cy) - r).abs() - sw * 0.5);
    for (bx, y0, y1) in [(8.75, 10.0, 14.0), (12.0, 8.0, 16.0), (15.25, 10.0, 14.0)] {
        let lx = x + bx * k;
        let ya = cy + (y0 - 12.0) * k;
        let yb = cy + (y1 - 12.0) * k;
        cv.fill_sdf(bb, color, opacity, |px, py| sd_segment(px, py, lx, ya, lx, yb) - sw * 0.5);
    }
}

/// Cancel glyph: X at 13 px (Nucleo `Xmark` 24-box, stroke 2.5).
fn draw_x_glyph(cv: &mut Canvas, cx: f32, cy: f32, color: Rgba, alpha: f32) {
    let k = 13.0 / 24.0;
    let e = 7.0 * k;
    let sw = 2.5 * k;
    let bb = Rect::new(cx - 7.0, cy - 7.0, cx + 7.0, cy + 7.0);
    cv.fill_sdf(bb, color, alpha, |x, y| {
        sd_segment(x, y, cx - e, cy - e, cx + e, cy + e).min(sd_segment(x, y, cx - e, cy + e, cx + e, cy - e)) - sw * 0.5
    });
}

fn draw_bubble(cv: &mut Canvas, st: &RenderState, g: &Geometry, app: &Appearance, a: &AppBubble) {
    let (bx, by, br) = g.bubble;
    let alpha = g.bubble_t;
    let d = br * 2.0;
    let r = Rect::new(bx - br, by - br, bx + br, by + br);
    if app.shadow.alpha > 0.001 {
        cv.shadow_sdf(r, app.shadow.dy, app.shadow.blur, app.shadow.spread, Rgba::new(0.0, 0.0, 0.0, 1.0), app.shadow.alpha * alpha, |x, y| {
            sd_capsule(x, y, bx, by, d, d)
        });
    }
    draw_chrome(cv, r, app, alpha, st.scale);
    // icon, snapped to whole device pixels
    let s = st.scale;
    let ic = &a.icon;
    let (iw, ih) = (ic.width() as i32, ic.height() as i32);
    let ox = (bx * s - iw as f32 * 0.5).round() as i32;
    let oy = (by * s - ih as f32 * 0.5).round() as i32;
    for y in 0..ih {
        for x in 0..iw {
            let i = ((y * iw + x) * 4) as usize;
            let d = ic.data();
            let pa = d[i + 3] as f32 / 255.0;
            if pa <= 0.0 {
                continue;
            }
            cv.blend(ox + x, oy + y, [d[i] as f32 / 255.0 * alpha, d[i + 1] as f32 / 255.0 * alpha, d[i + 2] as f32 / 255.0 * alpha, pa * alpha]);
        }
    }
}

// --- liquid fusion skin -----------------------------------------------------------------

fn smin(a: f32, b: f32, k: f32) -> f32 {
    if k <= 0.0 {
        return a.min(b);
    }
    let h = (0.5 + 0.5 * (b - a) / k).clamp(0.0, 1.0);
    b * (1.0 - h) + a * h - k * h * (1.0 - h)
}

/// `emergenceBlend`: smoothstep from a hard union to the full fillet (k = 24).
pub fn emergence_blend(t: f32) -> f32 {
    let c = t.clamp(0.0, 1.0);
    24.0 * c * c * (3.0 - 2.0 * c)
}

/// Smooth-union SDF of pill capsule and cancel circle (window space).
pub fn fusion_field(g: &Geometry, k: f32, x: f32, y: f32) -> f32 {
    let p = g.pill;
    let d_pill = sd_capsule(x, y, p.cx(), p.cy(), p.w(), p.h());
    let (cx, cy, r) = g.cancel;
    let d_circle = (x - cx).hypot(y - cy) - r;
    smin(d_pill, d_circle, k)
}

fn draw_skin(cv: &mut Canvas, st: &RenderState, g: &Geometry, app: &Appearance) {
    let k = emergence_blend(g.cancel_t);
    let p = g.pill;
    let (ccx, _, cr) = g.cancel;
    let bb = Rect::new(p.l.min(ccx - cr) - k - 2.0, p.t.min(g.cancel.1 - cr) - k - 2.0, p.r.max(ccx + cr) + k + 2.0, p.b.max(g.cancel.1 + cr) + k + 2.0);
    let s = cv.scale;
    let f = |x: f32, y: f32| fusion_field(g, k, x, y);
    if st.pal.dark {
        cv.shadow_sdf(bb, 1.0, 3.0, 0.0, Rgba::new(0.0, 0.0, 0.0, 1.0), 0.2, f);
    }
    let (bg, bd) = (app.bg, app.border);
    let e = 0.25 / s;
    cv.shade_sdf(bb, f, |x, y, _c| {
        // distance normalised by the field gradient (smin is not a true distance)
        let d = f(x, y);
        if d.abs() > 2.0 {
            let c = if d < 0.0 { 1.0 } else { 0.0 };
            return [bg.r * bg.a * c, bg.g * bg.a * c, bg.b * bg.a * c, bg.a * c];
        }
        let gx = (f(x + e, y) - f(x - e, y)) / (2.0 * e);
        let gy = (f(x, y + e) - f(x, y - e)) / (2.0 * e);
        let gl = gx.hypot(gy).max(0.25);
        let dn = d / gl;
        let fill = cov(dn * s);
        let stroke = cov((dn.abs() - 0.5) * s) ;
        let ba = stroke * bd.a;
        let pa = fill * bg.a;
        let inv = 1.0 - ba;
        [bd.r * ba + bg.r * pa * inv, bd.g * ba + bg.g * pa * inv, bd.b * ba + bg.b * pa * inv, ba + pa * inv]
    });
}

// --- tooltip -----------------------------------------------------------------------------

fn draw_tooltip(cv: &mut Canvas, st: &RenderState, txt: &str, anchor: Rect, align: Align) {
    let pal = st.pal;
    let s = st.scale;
    let font = Font::new(10.0, s, 400);
    let tw = text::measure(txt, font) / s;
    let w = tw + 12.0 + 2.0; // px-1.5 + border
    let h = 15.0 + 8.0 + 2.0; // text-[10px] * 1.5 line-height + py-1 + border
    let bottom = anchor.t - TOOLTIP_GAP;
    let mut l = match align {
        Align::Right => anchor.r - w,
        Align::Left => anchor.l,
        Align::Center => anchor.cx() - w * 0.5,
    };
    let (ww, _) = window_size(st);
    l = l.clamp(2.0, (ww - w - 2.0).max(2.0));
    let r = Rect::from_xywh(l, bottom - h, w, h);
    // shadow-lg
    cv.shadow_rect(r, h * 0.5, (0.0, 10.0), 15.0, -3.0, Rgba::new(0.0, 0.0, 0.0, 0.1), 1.0);
    cv.shadow_rect(r, h * 0.5, (0.0, 4.0), 6.0, -4.0, Rgba::new(0.0, 0.0, 0.0, 0.1), 1.0);
    // body + border
    let outer = |x: f32, y: f32| sd_capsule(x, y, r.cx(), r.cy(), r.w(), r.h());
    cv.fill_sdf(r.inflate(1.0), pal.popover, 1.0, outer);
    let bd = pal.border;
    cv.shade_sdf(r.inflate(1.0), outer, |x, y, c| {
        let inner = cov(sd_capsule(x, y, r.cx(), r.cy(), r.w() - 2.0, r.h() - 2.0) * s);
        let k = (c - inner).max(0.0) * bd.a;
        [bd.r * k, bd.g * k, bd.b * k, k]
    });
    // arrow (4x2 triangle on the bottom edge)
    let ax = match align {
        Align::Right => r.r - 1.0 - 12.0 - 4.0,
        Align::Left => r.l + 1.0 + 12.0,
        Align::Center => r.cx() - 2.0,
    };
    let ay = r.b - 1.0;
    cv.shade_sdf(
        Rect::new(ax - 1.0, ay, ax + 5.0, ay + 3.0),
        |x, y| arrow_sd(x, y, ax, ay),
        |_x, _y, c| {
            let p = pal.popover;
            [p.r * c, p.g * c, p.b * c, c]
        },
    );
    if let Some(m) = text::mask(txt, font) {
        let cell = text::content_height(font) / s;
        text::draw(cv, &m, r.l + 1.0 + 6.0, r.t + 1.0 + 4.0 + (15.0 - cell) * 0.5, pal.fg, 1.0);
    }
}

/// Signed distance to the tooltip arrow: top edge (ax, ay)-(ax+4, ay), apex (ax+2, ay+2).
fn arrow_sd(px: f32, py: f32, ax: f32, ay: f32) -> f32 {
    let dy = py - ay;
    let r = std::f32::consts::FRAC_1_SQRT_2;
    (ay - py).max(-(px - ax - dy) * r).max(-((ax + 4.0) - px - dy) * r)
}

// --- error card --------------------------------------------------------------------------

fn render_error(st: &RenderState, e: &card::ErrorCard, t: f64, pm: &mut Pixmap) {
    let scale = st.scale;
    let layout = e.layout(scale);
    let Some(base) = card::base_layer(e, scale, &st.pal) else { return };
    let (op, ty, sc) = e.presence(t);
    if op <= 0.001 {
        return;
    }
    let mut layer = (*base).clone();
    card::draw_countdown(&mut layer, &layout, scale, st.pal.fg, e.remaining_fraction(t));
    if op >= 0.999 && ty.abs() < 0.01 && (sc - 1.0).abs() < 1e-4 && layer.width() == pm.width() && layer.height() == pm.height() {
        pm.data_mut().copy_from_slice(layer.data());
        return;
    }
    let cx = layout.card.cx() * scale;
    let cy = layout.card.cy() * scale;
    let tf = Transform::from_translate(-cx, -cy).post_scale(sc, sc).post_translate(cx, cy + ty * scale);
    pm.draw_pixmap(0, 0, layer.as_ref(), &PixmapPaint { opacity: op.clamp(0.0, 1.0), blend_mode: tiny_skia::BlendMode::SourceOver, quality: FilterQuality::Bilinear }, tf, None);
}

// --- steady-state frame cache ---------------------------------------------------------------

/// What [`FrameCache::render`] did to the pixmap.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameOutcome {
    /// The pixmap already shows this frame; nothing needs uploading.
    Skip,
    /// Only this rect of the pixmap changed (the whole pixmap for a full repaint).
    Dirty(DevRect),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    /// Recording with every non-waveform animation settled: only the 52x24 lane moves.
    Lane,
    /// Thinking with the entrance settled: only the glow (a disc around the pill) moves.
    Glow,
}

/// Bars are redrawn only when one moved by more than this many pixels (1/255 of a pixel is below
/// what an 8-bit coverage value can show).
pub const BAR_EPS: f32 = 1.0 / 255.0;

/// 64-bit FNV-1a, for the cheap per-frame "did anything static change" signature.
struct Fnv(u64);

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

impl Fnv {
    fn new() -> Fnv {
        Fnv(0xcbf2_9ce4_8422_2325)
    }
    fn f(&mut self, v: f32) {
        self.write_u32(v.to_bits());
    }
    fn c(&mut self, c: Rgba) {
        self.f(c.r);
        self.f(c.g);
        self.f(c.b);
        self.f(c.a);
    }
}

/// Is the frame at `t` one of the two steady shapes? Everything except the lane (resp. the glow) is a
/// pure function of the state, not of `t`: no track is moving, nothing pulses, no error card.
fn classify(st: &RenderState, t: f64) -> Option<Kind> {
    if st.error().is_some() || st.pulse_since.is_some() {
        return None;
    }
    let tracks = [&st.compact, &st.grad, &st.wave_live, &st.wave_rest, &st.divider, &st.glow, &st.cancel, &st.fused, &st.bubble, &st.cancel_hot];
    if tracks.iter().any(|tr| tr.is_active(t)) || st.appearance.is_active(t) {
        return None;
    }
    let (live, rest, glow) = (st.wave_live.value(t), st.wave_rest.value(t), st.glow.value(t));
    match (st.mode(), st.look()) {
        (PillMode::Recording, Look::Recording) => {
            // a clipped lane is never steady; with `compact` settled this is only a guard
            let full_lane = contents_layout(&st.geometry(t), st.divider.value(t).clamp(0.0, 1.0)).lane_w >= LANE_W - 0.01;
            (glow <= 0.002 && live > 0.003 && rest <= 0.003 && full_lane).then_some(Kind::Lane)
        }
        (PillMode::Thinking, Look::Thinking) => (glow > 0.002 && live <= 0.003 && rest <= 0.003).then_some(Kind::Glow),
        _ => None,
    }
}

/// Is there anything in the overlay group (ring, cancel glyph, tooltip) at this instant?
fn has_overlay(st: &RenderState, g: &Geometry) -> bool {
    st.look() == Look::Unavailable || g.cancel_t > 0.35 || ((st.pointer().pill || st.pointer().bubble) && tooltip_for(st).is_some())
}

/// Hash of everything that decides the static pixels of a steady frame.
fn signature(st: &RenderState, t: f64, g: &Geometry, size: (u32, u32), kind: Kind) -> u64 {
    let mut h = Fnv::new();
    h.write_u8(kind as u8);
    h.write_u32(size.0);
    h.write_u32(size.1);
    h.f(st.scale);
    let p = &st.pal;
    h.write_u8(p.dark as u8);
    for c in [p.fg, p.muted_fg, p.border, p.border_hover, p.surface0, p.surface1, p.surface3, p.popover] {
        h.c(c);
    }
    h.f(p.halo_opacity);
    h.f(p.comet_opacity);
    h.write_u8(st.position as u8);
    h.write_u8(st.show_app_bubble as u8);
    h.write(st.hotkey_label.as_bytes());
    h.write_u8(st.mode() as u8);
    h.write_u8(st.look() as u8);
    let ptr = st.pointer();
    h.write_u8(ptr.group as u8 | (ptr.pill as u8) << 1 | (ptr.bubble as u8) << 2 | (ptr.cancel as u8) << 3);
    let geom = [g.compact, g.pill.l, g.pill.t, g.pill.r, g.pill.b, g.cancel_t, g.bubble_t, g.cancel.0, g.cancel.1, g.cancel.2, g.bubble.0, g.bubble.1, g.bubble.2];
    for v in geom {
        h.f(v);
    }
    let a = st.appearance_at(t);
    h.c(a.bg);
    h.c(a.border);
    for v in [a.shadow.dy, a.shadow.blur, a.shadow.spread, a.shadow.alpha] {
        h.f(v);
    }
    for v in [st.fused.value(t), st.grad.value(t), st.cancel_hot.value(t), st.divider.value(t), st.wave_live.value(t), st.wave_rest.value(t)] {
        h.f(v);
    }
    // the icon's address identifies its pixels; the cache keeps the Arc alive so it cannot be reused
    h.write_usize(st.app().map_or(0, |a| Arc::as_ptr(&a.icon) as usize));
    // the bubble's tooltip shows the app name, which may change while the icon Arc is reused
    if let Some(a) = st.app() {
        h.write(a.name.as_bytes());
    }
    h.finish()
}

/// Device rect covering every pixel the waveform bars can touch (the 52x24 lane plus AA slack).
fn lane_rect(st: &RenderState, g: &Geometry, t: f64, w: i32, h: i32) -> DevRect {
    let ct = contents_layout(g, st.divider.value(t).clamp(0.0, 1.0));
    let lane = Rect::from_xywh(ct.lane_x, ct.cy - LANE_H * 0.5, ct.lane_w, LANE_H);
    DevRect::of(lane.inflate(2.0), st.scale, w, h)
}

/// Device rect covering the halo, ring and comet. The sprites reach `GLOW_RADIUS + 16` logical px
/// from the pill's centre (scaled by its glow box, which breathes up to 1.05x); a test sweeps the
/// orbit to guard the margin.
fn glow_rect(g: &Geometry, scale: f32, w: i32, h: i32) -> DevRect {
    let box_s = (g.pill.w() + 8.0).max(g.pill.h() + 8.0) / (GLOW_RADIUS * 2.0);
    let half = (GLOW_RADIUS + 16.0) * box_s * 1.05 + 3.0;
    DevRect::of(Rect::new(g.pill.cx() - half, g.pill.cy() - half, g.pill.cx() + half, g.pill.cy() + half), scale, w, h)
}

fn ensure(slot: &mut Option<Pixmap>, w: u32, h: u32) -> Option<&mut Pixmap> {
    if slot.as_ref().is_none_or(|p| p.width() != w || p.height() != h) {
        *slot = Pixmap::new(w, h);
    }
    slot.as_mut()
}

/// Incremental renderer for the window: [`FrameCache::render`] has the same result as
/// [`render_into`] but, once an animation has settled into one of two steady shapes, it redraws only
/// what moves and says which rect changed.
///
/// - Recording: everything but the bars (shadow, body, logo, bubble) is cached in `base`. Each frame
///   restores the 52x24 lane from `base` and redraws the bars over it; an overlay that reaches into
///   the lane (the tooltip's soft shadow) is laid back over them. A frame whose bars all moved by
///   <= 1/255 px is skipped outright.
/// - Thinking: the glow is the only moving layer. The rest of the frame is cached in `base`; each
///   frame draws the glow alone into `scratch`, then composites `base` over it inside the glow's
///   bounding rect (the same as drawing in order, up to 8-bit rounding on partly covered edges).
///
/// Any change to the static inputs changes [`signature`] and any animation still running fails
/// [`classify`]; both end in a full repaint. After the target pixmap is replaced, or its content
/// stops matching the last frame (window hidden), call [`FrameCache::reset`].
pub struct FrameCache {
    kind: Option<Kind>,
    sig: u64,
    base: Option<Pixmap>,
    /// Glow layer (Glow) or the overlay layer (Lane, used only when it overlaps the lane).
    scratch: Option<Pixmap>,
    over_in_lane: bool,
    lane: DevRect,
    glow: DevRect,
    last_h: [f32; WAVE_BARS],
    /// Keeps the app icon alive so its address stays a valid signature input.
    icon: Option<Arc<Pixmap>>,
}

impl Default for FrameCache {
    fn default() -> Self {
        Self::new()
    }
}

impl FrameCache {
    pub fn new() -> FrameCache {
        let zero = DevRect::full(0, 0);
        FrameCache { kind: None, sig: 0, base: None, scratch: None, over_in_lane: false, lane: zero, glow: zero, last_h: [0.0; WAVE_BARS], icon: None }
    }

    /// Forget everything: the next frame is a full repaint.
    pub fn reset(&mut self) {
        self.kind = None;
        self.icon = None;
    }

    /// Draw the frame at `t` into `pm` (exactly `pixel_size(st)`); returns what changed in it.
    pub fn render(&mut self, st: &RenderState, t: f64, pm: &mut Pixmap) -> FrameOutcome {
        let (w, h) = (pm.width() as i32, pm.height() as i32);
        let full = FrameOutcome::Dirty(DevRect::full(w, h));
        let Some(kind) = classify(st, t) else {
            self.kind = None;
            render_into(st, t, pm);
            return full;
        };
        let g = st.geometry(t);
        let sig = signature(st, t, &g, (pm.width(), pm.height()), kind);
        if self.kind == Some(kind) && self.sig == sig {
            return match kind {
                Kind::Lane => self.update_lane(st, &g, t, pm),
                Kind::Glow => self.update_glow(st, &g, t, pm),
            };
        }
        self.rebuild(st, &g, t, pm, kind, sig);
        full
    }

    fn rebuild(&mut self, st: &RenderState, g: &Geometry, t: f64, pm: &mut Pixmap, kind: Kind, sig: u64) {
        let (w, h) = (pm.width(), pm.height());
        self.kind = None;
        self.icon = st.app().map(|a| a.icon.clone());
        let (Some(base), Some(scratch)) = (ensure(&mut self.base, w, h), ensure(&mut self.scratch, w, h)) else {
            render_into(st, t, pm);
            return;
        };
        base.data_mut().fill(0);
        scratch.data_mut().fill(0);
        let scale = st.scale;
        match kind {
            Kind::Lane => {
                draw_under(&mut Canvas::new(base, scale), st, g, t);
                // the same operations, in the same order, on the same pixels as `render_into`
                pm.data_mut().copy_from_slice(base.data());
                let hs = st.waveform().heights(t);
                {
                    let mut cv = Canvas::new(pm, scale);
                    draw_bars(&mut cv, st, g, t, &hs);
                    draw_over(&mut cv, st, g, t);
                }
                self.lane = lane_rect(st, g, t, w as i32, h as i32);
                self.last_h = hs;
                self.over_in_lane = false;
                if has_overlay(st, g) {
                    draw_over(&mut Canvas::new(scratch, scale), st, g, t);
                    self.over_in_lane = any_alpha(scratch.data(), w as i32, self.lane);
                }
            }
            Kind::Glow => {
                {
                    let mut cv = Canvas::new(base, scale);
                    draw_under(&mut cv, st, g, t);
                    draw_bars(&mut cv, st, g, t, &st.waveform().heights(t));
                    draw_over(&mut cv, st, g, t);
                }
                draw_glow(&mut Canvas::new(scratch, scale), st, g, t);
                over_rect(pm.data_mut(), base.data(), scratch.data(), w as i32, DevRect::full(w as i32, h as i32));
                self.glow = glow_rect(g, scale, w as i32, h as i32);
            }
        }
        self.sig = sig;
        self.kind = Some(kind);
    }

    fn update_lane(&mut self, st: &RenderState, g: &Geometry, t: f64, pm: &mut Pixmap) -> FrameOutcome {
        let hs = st.waveform().heights(t);
        if hs.iter().zip(&self.last_h).all(|(a, b)| (a - b).abs() <= BAR_EPS) {
            return FrameOutcome::Skip;
        }
        let (Some(base), Some(scratch)) = (self.base.as_ref(), self.scratch.as_ref()) else { return self.fallback(st, t, pm) };
        let (w, r) = (pm.width() as i32, self.lane);
        copy_rect(pm.data_mut(), base.data(), w, r);
        draw_bars(&mut Canvas::new(pm, st.scale), st, g, t, &hs);
        if self.over_in_lane {
            over_in_place(pm.data_mut(), scratch.data(), w, r);
        }
        self.last_h = hs;
        FrameOutcome::Dirty(r)
    }

    fn update_glow(&mut self, st: &RenderState, g: &Geometry, t: f64, pm: &mut Pixmap) -> FrameOutcome {
        let (Some(base), Some(scratch)) = (self.base.as_ref(), self.scratch.as_mut()) else { return self.fallback(st, t, pm) };
        let (w, r) = (pm.width() as i32, self.glow);
        clear_rect(scratch.data_mut(), w, r);
        draw_glow(&mut Canvas::new(scratch, st.scale), st, g, t);
        over_rect(pm.data_mut(), base.data(), scratch.data(), w, r);
        FrameOutcome::Dirty(r)
    }

    fn fallback(&mut self, st: &RenderState, t: f64, pm: &mut Pixmap) -> FrameOutcome {
        self.kind = None;
        render_into(st, t, pm);
        FrameOutcome::Dirty(DevRect::full(pm.width() as i32, pm.height() as i32))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::config::PanelPosition;
    use crate::daemon::pill::PillMode;
    use crate::daemon::pill::theme::Palette;

    fn rec_state(dark: bool) -> RenderState {
        let mut s = RenderState::new(Palette::get(dark), 1.0, PanelPosition::BottomRight);
        s.set_mode(PillMode::Recording, 0.0);
        s
    }

    fn alpha_at(pm: &Pixmap, x: u32, y: u32) -> u8 {
        pm.pixel(x, y).unwrap().alpha()
    }

    /// A failed DPI/monitor query (scale 0, NaN, inf, absurd) must not panic the daemon's render path.
    #[test]
    fn degenerate_scales_render_without_panicking() {
        for scale in [0.0f32, -2.0, f32::MIN_POSITIVE, f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            for dark in [false, true] {
                for mode in [PillMode::Idle, PillMode::Preparing, PillMode::Recording, PillMode::Thinking, PillMode::Unavailable] {
                    let mut s = RenderState::new(Palette::get(dark), scale, PanelPosition::BottomRight);
                    s.set_app(AppBubble::new("App", &[200u8; 8 * 8 * 4], 8, scale), 0.0);
                    s.set_mode(mode, 0.0);
                    let pm = render_frame(&s, 1500.0).unwrap_or_else(|| panic!("scale {scale}: no frame"));
                    assert_eq!((pm.width(), pm.height()), pixel_size(&s), "scale {scale}");
                    // the incremental path too
                    let mut cache = FrameCache::new();
                    let mut target = Pixmap::new(pm.width(), pm.height()).unwrap();
                    cache.render(&s, 1500.0, &mut target);
                    cache.render(&s, 1520.0, &mut target);
                }
                // the error card, with and without the retry button
                let mut s = RenderState::new(Palette::get(dark), scale, PanelPosition::BottomCenter);
                s.set_scale(scale);
                s.error = Some(card::ErrorCard::new("Transcription failed", "The speech provider rejected your key.", true, 0.0));
                assert!(render_frame(&s, 400.0).is_some(), "scale {scale}: error card");
            }
        }
        // an absurd scale is clamped, never allocated (not rendered: a 16x frame is slow in debug builds)
        let huge = RenderState::new(Palette::LIGHT, 1e9, PanelPosition::BottomRight);
        assert_eq!(pixel_size(&huge), pixel_size(&RenderState::new(Palette::LIGHT, 16.0, PanelPosition::BottomRight)));
        // zero sizes straight into the helpers
        let mut s = RenderState::new(Palette::LIGHT, 0.0, PanelPosition::BottomRight);
        assert_eq!(pixel_size(&s), pixel_size(&RenderState::new(Palette::LIGHT, 0.25, PanelPosition::BottomRight)), "clamped to the smallest usable scale");
        s.set_scale(f32::NAN);
        assert_eq!(s.scale, 1.0);
        let src = Pixmap::new(4, 4).unwrap();
        assert_eq!(resample(&src, 0).map(|p| p.width()), Some(1));
        assert!(Sprites::get(0.0).is_some());
        assert!(AppBubble::new("x", &[], 0, 1.0).is_none());
    }

    #[test]
    fn idle_circle_is_40x40_at_bottom_right() {
        let s = RenderState::new(Palette::LIGHT, 1.0, PanelPosition::BottomRight);
        let pm = render(&s, 0.0);
        assert_eq!((pm.width(), pm.height()), (208, 120));
        // circle spans x 156..196, y 68..108
        assert_eq!(alpha_at(&pm, 176, 88), 255);
        assert_eq!(alpha_at(&pm, 150, 88), 0);
        assert_eq!(alpha_at(&pm, 176, 60), 0);
        assert!(alpha_at(&pm, 157, 88) > 128);
        // idle background is surface-1 #fafafa away from the logo
        let px = pm.pixel(165, 80).unwrap();
        assert_eq!((px.red(), px.green(), px.blue()), (0xfa, 0xfa, 0xfa));
    }

    #[test]
    fn recording_capsule_has_98x36_footprint() {
        let s = rec_state(false);
        let pm = render(&s, 1500.0);
        // capsule x 98..196, y 72..108
        assert!(alpha_at(&pm, 99, 90) > 128);
        assert_eq!(alpha_at(&pm, 96, 90), 0);
        assert_eq!(alpha_at(&pm, 150, 70), 0);
        assert_eq!(alpha_at(&pm, 150, 90), 255);
    }
    fn ink(pm: &Pixmap, x: u32, y: u32) -> bool {
        pm.pixel(x, y).unwrap().red() < 128
    }

    #[test]
    fn waveform_bars_follow_levels() {
        let mut s = rec_state(false);
        // constant loud level for 1500 ms at 60 fps
        let mut t = 0.0;
        while t <= 1500.0 {
            s.tick(t, 0.2);
            t += 1000.0 / 60.0;
        }
        let pm = render(&s, 1500.0);
        // lane x = 132..184 (pill 98..196, logo 104..126, 6 px margins); bar i is centred at 133 + 5i
        for i in 0..11u32 {
            let x = 133 + 5 * i;
            assert!(ink(&pm, x, 80) && ink(&pm, x, 100), "bar {i} should reach max height");
            assert!(!ink(&pm, x + 2, 90), "3 px gap after bar {i}");
        }
        // silence: bars collapse to 4 px (rows 88..92)
        let mut s2 = rec_state(false);
        let mut t = 0.0;
        while t <= 1500.0 {
            s2.tick(t, 0.0);
            t += 1000.0 / 60.0;
        }
        let pm2 = render(&s2, 1500.0);
        for i in 0..11u32 {
            let x = 133 + 5 * i;
            assert!(ink(&pm2, x, 90), "bar {i} min height");
            assert!(!ink(&pm2, x, 85) && !ink(&pm2, x, 95), "bar {i} must not exceed 4 px");
        }
    }

    #[test]
    fn render_recording_is_fast() {
        let mut s = rec_state(true);
        let mut t = 0.0;
        while t <= 1000.0 {
            s.tick(t, 0.05);
            t += 1000.0 / 60.0;
        }
        let mut pm = Pixmap::new(208, 120).unwrap();
        render_into(&s, 1000.0, &mut pm); // warm
        let n = 50;
        let t0 = std::time::Instant::now();
        for _ in 0..n {
            render_into(&s, 1000.0, &mut pm);
        }
        let per = t0.elapsed().as_secs_f64() * 1000.0 / n as f64;
        // loose bound for debug builds; the release gate (<0.5 ms) is checked by `cli_render --bench`
        assert!(per < if cfg!(debug_assertions) { 20.0 } else { 0.5 }, "render {per:.3} ms");
    }

    #[test]
    fn emergence_blend_is_smoothstep() {
        assert_eq!(emergence_blend(0.0), 0.0);
        assert_eq!(emergence_blend(1.0), 24.0);
        assert!((emergence_blend(0.5) - 12.0).abs() < 1e-4);
    }

    #[test]
    fn fusion_field_matches_hard_union_at_zero_blend() {
        let mut s = rec_state(false);
        s.settle();
        s.set_pointer(Pointer { group: true, pill: true, bubble: false, cancel: false }, 1000.0);
        let g = s.geometry(2000.0);
        let a = fusion_field(&g, 0.0, g.pill.cx(), g.pill.cy());
        assert!(a < -17.0);
        // inside the neck between pill and bud the smooth union is more inside than the hard union
        let x = g.pill.r + 4.0;
        let hard = fusion_field(&g, 0.0, x, g.pill.cy());
        let soft = fusion_field(&g, 24.0, x, g.pill.cy());
        assert!(soft < hard);
    }

    #[test]
    fn dark_theme_uses_dark_tokens() {
        let s = RenderState::new(Palette::DARK, 1.0, PanelPosition::BottomRight);
        let pm = render(&s, 0.0);
        let px = pm.pixel(165, 80).unwrap();
        assert_eq!((px.red(), px.green(), px.blue()), (0x20, 0x22, 0x25));
    }

    #[test]
    fn scale_doubles_the_window() {
        let s = RenderState::new(Palette::LIGHT, 2.0, PanelPosition::BottomRight);
        let pm = render(&s, 0.0);
        assert_eq!((pm.width(), pm.height()), (416, 240));
        assert_eq!(alpha_at(&pm, 352, 176), 255);
    }

    // --- FrameCache ----------------------------------------------------------------------------

    const FRAME: f64 = 1000.0 / 60.0;

    /// Tick a state at 60 fps from `*t` up to (and including) `until`; `level` is the mic RMS.
    fn run(s: &mut RenderState, t: &mut f64, until: f64, level: &dyn Fn(f64) -> f32) {
        while *t <= until {
            s.tick(*t, level(*t));
            *t += FRAME;
        }
    }

    fn speech(t: f64) -> f32 {
        0.02 + 0.2 * (t / 137.0).sin().abs() as f32
    }

    fn new_pm(s: &RenderState) -> Pixmap {
        let (w, h) = pixel_size(s);
        Pixmap::new(w, h).unwrap()
    }

    fn max_diff(a: &Pixmap, b: &Pixmap) -> u8 {
        a.data().iter().zip(b.data()).map(|(x, y)| x.abs_diff(*y)).max().unwrap_or(0)
    }

    /// Bytes that differ between two same-size pixmaps outside `r`.
    fn changed_outside(a: &[u8], b: &Pixmap, r: DevRect) -> usize {
        let w = b.width() as usize;
        (0..a.len() / 4).filter(|i| !r.contains_pixel((i % w) as i32, (i / w) as i32) && a[i * 4..i * 4 + 4] != b.data()[i * 4..i * 4 + 4]).count()
    }

    fn full_rect(pm: &Pixmap) -> DevRect {
        DevRect::full(pm.width() as i32, pm.height() as i32)
    }

    /// Logical lane box of the settled recording capsule at bottom-right: x 132..184, y 78..102.
    fn lane_dev(scale: f32, pm: &Pixmap) -> DevRect {
        DevRect::of(Rect::new(132.0 - 3.0, 78.0 - 3.0, 184.0 + 3.0, 102.0 + 3.0), scale, pm.width() as i32, pm.height() as i32)
    }

    #[test]
    fn steady_recording_frame_touches_only_the_lane_rect() {
        for (scale, dark) in [(1.0f32, false), (1.0, true), (1.25, false), (2.0, true)] {
            let mut s = RenderState::new(Palette::get(dark), scale, PanelPosition::BottomRight);
            s.set_mode(PillMode::Recording, 0.0);
            let mut t = 0.0;
            run(&mut s, &mut t, 1500.0, &speech);
            let mut pm = new_pm(&s);
            let mut cache = FrameCache::new();
            // the first steady frame paints everything and is the golden path byte for byte
            assert_eq!(cache.render(&s, t, &mut pm), FrameOutcome::Dirty(full_rect(&pm)));
            assert_eq!(pm.data(), render(&s, t).data(), "scale {scale} dark {dark}: first frame");
            let lane = lane_dev(scale, &pm);
            let (mut dirty, mut skipped) = (0, 0);
            for _ in 0..180 {
                s.tick(t, speech(t));
                let before = pm.data().to_vec();
                match cache.render(&s, t, &mut pm) {
                    FrameOutcome::Skip => {
                        skipped += 1;
                        assert_eq!(before, pm.data());
                        // a skipped frame is within rounding of the true frame
                        assert!(max_diff(&pm, &render(&s, t)) <= 2, "skipped frame drifted at {t}");
                    }
                    FrameOutcome::Dirty(r) => {
                        dirty += 1;
                        assert!(!r.is_empty() && r.x0 >= lane.x0 && r.y0 >= lane.y0 && r.x1 <= lane.x1 && r.y1 <= lane.y1, "{r:?} outside the lane {lane:?}");
                        assert!(r.area() * 10 < full_rect(&pm).area(), "dirty rect {r:?} is not small");
                        assert_eq!(changed_outside(&before, &pm, r), 0, "pixels outside {r:?} changed");
                        assert_eq!(pm.data(), render(&s, t).data(), "lane update differs from a full render at {t}");
                    }
                }
                t += FRAME;
            }
            assert!(dirty > 100, "bars moved on most frames ({dirty} dirty, {skipped} skipped)");
        }
    }

    #[test]
    fn lane_updates_match_the_full_render_at_every_dock_position() {
        for pos in [PanelPosition::BottomLeft, PanelPosition::BottomCenter, PanelPosition::BottomRight] {
            let mut s = RenderState::new(Palette::get(true), 1.5, pos);
            s.set_mode(PillMode::Recording, 0.0);
            let mut t = 0.0;
            run(&mut s, &mut t, 1500.0, &speech);
            let mut pm = new_pm(&s);
            let mut cache = FrameCache::new();
            cache.render(&s, t, &mut pm);
            for _ in 0..60 {
                s.tick(t, speech(t));
                if let FrameOutcome::Dirty(r) = cache.render(&s, t, &mut pm) {
                    assert!(r.area() * 10 < full_rect(&pm).area(), "{pos:?}: {r:?} is not small");
                }
                assert!(max_diff(&pm, &render(&s, t)) <= 2, "{pos:?}: drifted at {t}");
                t += FRAME;
            }
        }
    }

    #[test]
    fn silent_recording_frames_are_skipped() {
        let mut s = rec_state(true);
        let mut t = 0.0;
        run(&mut s, &mut t, 1500.0, &|_| 0.0);
        let mut pm = new_pm(&s);
        let mut cache = FrameCache::new();
        assert!(matches!(cache.render(&s, t, &mut pm), FrameOutcome::Dirty(_)));
        for _ in 0..120 {
            t += FRAME;
            s.tick(t, 0.0);
            assert_eq!(cache.render(&s, t, &mut pm), FrameOutcome::Skip, "silent input needs no repaint at {t}");
        }
        assert_eq!(pm.data(), render(&s, t).data());
        // speech wakes the lane again
        let mut woke = false;
        for _ in 0..30 {
            t += FRAME;
            s.tick(t, 0.2);
            woke |= matches!(cache.render(&s, t, &mut pm), FrameOutcome::Dirty(_));
        }
        assert!(woke);
        assert_eq!(pm.data(), render(&s, t).data());
    }

    #[test]
    fn frames_that_are_not_steady_repaint_everything() {
        let mut s = rec_state(false);
        let mut pm = new_pm(&s);
        let mut cache = FrameCache::new();
        // entrance: the capsule is still growing
        for ms in [0.0, 100.0, 300.0, 450.0] {
            s.tick(ms, 0.1);
            assert_eq!(cache.render(&s, ms, &mut pm), FrameOutcome::Dirty(full_rect(&pm)));
            assert_eq!(pm.data(), render(&s, ms).data());
        }
        // an error card is never cached
        let mut e = rec_state(false);
        e.error = Some(card::ErrorCard::new("Title", "Description", true, 0.0));
        let mut pm = new_pm(&e);
        for t in [0.0, 400.0, 800.0] {
            assert_eq!(cache.render(&e, t, &mut pm), FrameOutcome::Dirty(full_rect(&pm)));
        }
    }

    #[test]
    fn static_input_changes_force_a_full_repaint() {
        // ticks frames until one is incremental (a skip or a sub-rect), proving the cache is primed
        fn settle_incrementally(cache: &mut FrameCache, s: &mut RenderState, t: &mut f64, pm: &mut Pixmap) {
            for _ in 0..30 {
                *t += FRAME;
                s.tick(*t, speech(*t));
                match cache.render(s, *t, pm) {
                    FrameOutcome::Dirty(r) if r == full_rect(pm) => {}
                    _ => return,
                }
            }
            panic!("never became incremental");
        }
        let mut s = rec_state(false);
        let mut t = 0.0;
        run(&mut s, &mut t, 1500.0, &speech);
        let mut pm = new_pm(&s);
        let mut cache = FrameCache::new();
        cache.render(&s, t, &mut pm);
        settle_incrementally(&mut cache, &mut s, &mut t, &mut pm);
        // palette switch snaps the chrome
        s.set_palette(Palette::DARK, t);
        assert_eq!(cache.render(&s, t, &mut pm), FrameOutcome::Dirty(full_rect(&pm)));
        assert_eq!(pm.data(), render(&s, t).data());
        // a new tooltip label, an app bubble appearing and a pointer move are static-input changes too
        settle_incrementally(&mut cache, &mut s, &mut t, &mut pm);
        s.hotkey_label = "Ctrl + Alt".into();
        assert_eq!(cache.render(&s, t, &mut pm), FrameOutcome::Dirty(full_rect(&pm)));
        settle_incrementally(&mut cache, &mut s, &mut t, &mut pm);
        s.set_pointer(Pointer { group: true, pill: true, bubble: false, cancel: false }, t);
        assert_eq!(cache.render(&s, t, &mut pm), FrameOutcome::Dirty(full_rect(&pm)));
        assert_eq!(pm.data(), render(&s, t).data());
        // an explicit reset repaints too
        let mut quiet = rec_state(false);
        let mut tq = 0.0;
        run(&mut quiet, &mut tq, 1500.0, &speech);
        let mut pmq = new_pm(&quiet);
        cache.render(&quiet, tq, &mut pmq);
        settle_incrementally(&mut cache, &mut quiet, &mut tq, &mut pmq);
        cache.reset();
        assert_eq!(cache.render(&quiet, tq, &mut pmq), FrameOutcome::Dirty(full_rect(&pmq)));
        assert_eq!(pmq.data(), render(&quiet, tq).data());
        // new pixmap size (scale change) rebuilds the layers too
        quiet.set_scale(2.0);
        let mut pm2 = new_pm(&quiet);
        assert_eq!(cache.render(&quiet, tq, &mut pm2), FrameOutcome::Dirty(full_rect(&pm2)));
        assert_eq!(pm2.data(), render(&quiet, tq).data());
    }

    #[test]
    fn hovered_recording_with_cancel_bud_and_tooltip_matches_the_full_render() {
        for with_bubble in [false, true] {
            let mut s = rec_state(false);
            if with_bubble {
                let icon = AppBubble::new("Explorer", &[200u8; 16 * 16 * 4], 16, 1.0).unwrap();
                s.set_app(Some(icon), 0.0);
            }
            let mut t = 0.0;
            run(&mut s, &mut t, 1500.0, &speech);
            let over_pill = Pointer { group: true, pill: true, bubble: false, cancel: false };
            s.set_pointer(over_pill, t);
            let until = t + 800.0;
            run(&mut s, &mut t, until, &speech);
            let mut pm = new_pm(&s);
            let mut cache = FrameCache::new();
            assert!(matches!(cache.render(&s, t, &mut pm), FrameOutcome::Dirty(_)));
            let mut partial = 0;
            for _ in 0..90 {
                s.tick(t, speech(t));
                if let FrameOutcome::Dirty(r) = cache.render(&s, t, &mut pm) {
                    partial += (r != full_rect(&pm)) as u32;
                }
                // the cancel glyph and the tooltip are laid over the bars; rounding aside this is the full frame
                assert!(max_diff(&pm, &render(&s, t)) <= 3, "bubble {with_bubble}: frame at {t}");
                t += FRAME;
            }
            assert!(partial > 50, "hover state is still incremental ({partial})");
        }
    }

    #[test]
    fn thinking_glow_frames_match_the_full_render_and_stay_inside_the_glow_rect() {
        for (scale, with_bubble) in [(1.0f32, false), (1.0, true), (1.5, false)] {
            let mut s = RenderState::new(Palette::DARK, scale, PanelPosition::BottomRight);
            if with_bubble {
                s.set_app(Some(AppBubble::new("Explorer", &[180u8; 16 * 16 * 4], 16, scale).unwrap()), 0.0);
            }
            s.set_mode(PillMode::Recording, 0.0);
            let mut t = 0.0;
            run(&mut s, &mut t, 1200.0, &speech);
            s.set_mode(PillMode::Thinking, t);
            let until = t + 1000.0;
            run(&mut s, &mut t, until, &|_| 0.0);
            let mut pm = new_pm(&s);
            let mut cache = FrameCache::new();
            assert_eq!(cache.render(&s, t, &mut pm), FrameOutcome::Dirty(full_rect(&pm)));
            let mut worst = 0;
            for _ in 0..200 {
                t += FRAME;
                s.tick(t, 0.0);
                let out = cache.render(&s, t, &mut pm);
                let FrameOutcome::Dirty(r) = out else { panic!("the glow never rests") };
                assert!(r.area() < full_rect(&pm).area(), "glow update is a sub-rect: {r:?}");
                worst = worst.max(max_diff(&pm, &render(&s, t)));
            }
            assert!(worst <= 3, "scale {scale} bubble {with_bubble}: cached glow differs from the full render by {worst}");
        }
    }

    #[test]
    fn glow_rect_contains_every_glow_pixel_over_the_orbit() {
        for (scale, pos) in [(1.0f32, PanelPosition::BottomRight), (1.25, PanelPosition::BottomCenter), (2.0, PanelPosition::BottomLeft)] {
            let mut s = RenderState::new(Palette::LIGHT, scale, pos);
            s.set_mode(PillMode::Thinking, 0.0);
            s.settle();
            let mut pm = new_pm(&s);
            let (w, h) = (pm.width() as i32, pm.height() as i32);
            let mut covered = 0usize;
            for i in 0..320 {
                let t = 100.0 + i as f64 * 97.0; // sweeps both the 1.6 s orbit and the 1.9 s breathe
                render_layers(&s, t, &mut pm, L_GLOW);
                let rect = glow_rect(&s.geometry(t), scale, w, h);
                for (n, px) in pm.data().chunks_exact(4).enumerate() {
                    if px[3] != 0 {
                        covered += 1;
                        let (x, y) = ((n as i32) % w, (n as i32) / w);
                        assert!(rect.contains_pixel(x, y), "glow pixel ({x},{y}) outside {rect:?} at t={t} scale {scale}");
                    }
                }
            }
            assert!(covered > 0);
        }
    }

    #[test]
    fn bar_rect_contains_every_bar_pixel() {
        for scale in [1.0f32, 1.25, 1.5, 2.0] {
            let mut s = RenderState::new(Palette::LIGHT, scale, PanelPosition::BottomRight);
            s.set_mode(PillMode::Recording, 0.0);
            let mut t = 0.0;
            run(&mut s, &mut t, 1500.0, &|t| if (t / 80.0) as i64 % 2 == 0 { 1.0 } else { 0.3 });
            let mut pm = new_pm(&s);
            render_layers(&s, t, &mut pm, L_BARS);
            let (w, h) = (pm.width() as i32, pm.height() as i32);
            let rect = lane_rect(&s, &s.geometry(t), t, w, h);
            let painted = pm.data().chunks_exact(4).enumerate().filter(|(_, p)| p[3] != 0).collect::<Vec<_>>();
            assert!(painted.len() > 100);
            for (n, _) in painted {
                assert!(rect.contains_pixel((n as i32) % w, (n as i32) / w), "bar pixel outside {rect:?} at scale {scale}");
            }
        }
    }

    #[test]
    fn layer_groups_compose_to_the_full_frame() {
        // the split is only an ordering: drawing the groups one after another equals `render_into`
        let mut s = rec_state(true);
        s.set_app(Some(AppBubble::new("App", &[120u8; 8 * 8 * 4], 8, 1.0).unwrap()), 0.0);
        let mut t = 0.0;
        run(&mut s, &mut t, 1500.0, &speech);
        s.set_pointer(Pointer { group: true, pill: true, bubble: false, cancel: false }, t);
        let until = t + 500.0;
        run(&mut s, &mut t, until, &speech);
        let want = render(&s, t);
        let mut pm = new_pm(&s);
        render_layers(&s, t, &mut pm, L_ALL);
        assert_eq!(pm.data(), want.data());
        // each group alone draws a subset: groups that are absent leave their pixels empty
        let mut under = new_pm(&s);
        render_layers(&s, t, &mut under, L_UNDER);
        let mut bars = new_pm(&s);
        render_layers(&s, t, &mut bars, L_BARS);
        assert!(under.data().iter().any(|b| *b != 0) && bars.data().iter().any(|b| *b != 0));
        let mut none = new_pm(&s);
        render_layers(&s, t, &mut none, 0);
        assert!(none.data().iter().all(|b| *b == 0));
    }

    #[test]
    fn steady_frame_is_much_cheaper_than_a_full_frame() {
        let mut s = rec_state(true);
        let mut t = 0.0;
        run(&mut s, &mut t, 1500.0, &speech);
        let mut pm = new_pm(&s);
        let mut cache = FrameCache::new();
        cache.render(&s, t, &mut pm);
        let n = 200;
        let mut cached = std::time::Duration::ZERO;
        let mut full = std::time::Duration::ZERO;
        for _ in 0..n {
            t += FRAME;
            s.tick(t, speech(t));
            let t0 = std::time::Instant::now();
            cache.render(&s, t, &mut pm);
            cached += t0.elapsed();
            let t1 = std::time::Instant::now();
            render_into(&s, t, &mut pm);
            full += t1.elapsed();
            // re-prime so the next cached frame starts from the same pixels
            cache.reset();
            cache.render(&s, t, &mut pm);
        }
        println!("steady lane frame {:?}/frame vs full frame {:?}/frame", cached / n, full / n);
        // not a timing gate (CI noise): the steady path just must not be the slower one
        assert!(cached < full, "cached {cached:?} vs full {full:?}");
    }
}
