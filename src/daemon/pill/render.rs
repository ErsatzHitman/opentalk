//! Pure renderer: `render(state, t_ms)` -> premultiplied RGBA pixmap of the whole pill window.
//! No Win32 is touched except the (cached) GDI text masks used by the tooltip and the error card, so
//! the output is deterministic for goldens.

use super::anim::*;
use super::card;
use super::glow::{self, GlowFrame, Sprites};
use super::model::*;
use super::raster::{Canvas, Rect, cov, sd_capsule, sd_round_box, sd_segment};
use super::text::{self, Font};
use super::theme::Rgba;
use tiny_skia::{FilterQuality, Pixmap, PixmapPaint, Transform};

/// Device-pixel size of the window for the current content.
pub fn pixel_size(st: &RenderState) -> (u32, u32) {
    let (w, h) = window_size(st);
    ((w * st.scale).ceil().max(1.0) as u32, (h * st.scale).ceil().max(1.0) as u32)
}

/// Render one frame into a fresh pixmap.
pub fn render(st: &RenderState, t: f64) -> Pixmap {
    let (w, h) = pixel_size(st);
    let mut pm = Pixmap::new(w, h).expect("pill window size");
    render_into(st, t, &mut pm);
    pm
}

/// Render into an existing pixmap of exactly `pixel_size(st)`; the pixmap is cleared first.
pub fn render_into(st: &RenderState, t: f64, pm: &mut Pixmap) {
    pm.data_mut().fill(0);
    if let Some(e) = st.error() {
        render_error(st, e, t, pm);
        return;
    }
    let scale = st.scale;
    let g = st.geometry(t);
    let pal = st.pal;
    let app = st.appearance_at(t);
    let fused = st.fused.value(t).clamp(0.0, 1.0);
    let chrome_alpha = 1.0 - fused;
    let mut cv = Canvas::new(pm, scale);

    // 1. Signal glow sits behind everything
    let glow_op = st.glow.value(t);
    if glow_op > 0.002 {
        let sprites = Sprites::get(scale);
        glow::draw(&mut cv, &pal, &GlowFrame { pill: g.pill, opacity: glow_op, elapsed: st.glow_since.map(|s| t - s) }, &sprites);
    }

    // 2. app bubble
    if g.bubble_t > 0.003 {
        if let Some(a) = st.app() {
            draw_bubble(&mut cv, st, &g, &app, a);
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
        draw_skin(&mut cv, st, &g, &app);
    }

    // 5. pill body
    if chrome_alpha > 0.002 {
        draw_chrome(&mut cv, p, &app, chrome_alpha, scale);
    }
    let grad = st.grad.value(t);
    if grad > 0.002 {
        draw_hover_gradient(&mut cv, p, pal.fg, grad);
    }

    // 6. contents
    draw_contents(&mut cv, st, &g, t);

    // 7. cancel glyph
    if g.cancel_t > 0.35 {
        let a = ((g.cancel_t - 0.35) / 0.65).clamp(0.0, 1.0);
        let hot = st.cancel_hot.value(t);
        let col = pal.muted_fg.lerp(pal.fg, hot);
        draw_x_glyph(&mut cv, g.cancel.0, g.cancel.1, col, a);
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
            draw_tooltip(&mut cv, st, &txt, anchor.0, anchor.1);
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
    div_h: f32,
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
        div_h: 20.0 - 4.0 * c,
    }
}

fn draw_contents(cv: &mut Canvas, st: &RenderState, g: &Geometry, t: f64) {
    let pal = st.pal;
    let divider = st.divider.value(t).clamp(0.0, 1.0);
    let ct = contents_layout(g, divider);
    let look = st.look();

    // logo
    let logo_col = logo_color(look, &pal);
    let op = st.pulse_opacity_at(t);
    draw_logo(cv, ct.logo_x, ct.cy, logo_col, op);

    // divider (bg-border/60)
    if ct.div_w > 0.02 && divider > 0.01 {
        let r = Rect::from_xywh(ct.div_x, ct.cy - 8.0, ct.div_w, 16.0);
        cv.fill_sdf(r.inflate(1.0), pal.border, 0.6 * divider, |x, y| sd_round_box(x, y, r.cx(), r.cy(), r.w() * 0.5, r.h() * 0.5, 0.0));
    }

    // waveform lane
    if ct.lane_w > 0.5 {
        let lane = Rect::from_xywh(ct.lane_x, ct.cy - LANE_H * 0.5, ct.lane_w, LANE_H);
        let bars_left = lane.cx() - LANE_W * 0.5;
        let clip = ct.lane_w < LANE_W - 0.01;
        let live = st.wave_live.value(t);
        let rest = st.wave_rest.value(t);
        if live > 0.003 {
            let hs = st.waveform().heights(t);
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

    // unavailable: pulsing 2 px ring just inside the border
    if look == Look::Unavailable {
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
    let base = card::base_layer(e, scale, &st.pal);
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
}
