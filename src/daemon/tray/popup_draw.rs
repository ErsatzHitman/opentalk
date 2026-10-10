//! Tray popup, rendering: a flat popover (solid surface, 1 px border, soft shadow) drawn with the
//! pill's software rasteriser (SDF shapes on a premultiplied pixmap) and GDI text. No glass, no
//! backdrop capture: it is the one popup that is deliberately plain. Icons are small vector shapes
//! (no icon font, so Windows 10 without Segoe Fluent Icons looks the same).
//!
//! Pure apart from the GDI text: the same picture is produced by `--render-tray-popup` and by the
//! live window.

use super::popup::{self, Content, Kind, Layout, MARGIN, Nav, PAD, Phase, R, RADIUS, ROW_RADIUS, Row, Target};
use crate::daemon::pill::kit::{Canvas, Font, Palette, Rect, Rgba, content_height, draw_icon_mask, draw_logo, draw_retry_icon, draw_text, icon_mask, sd_round_box, sd_segment, text_mask, text_measure};
use tiny_skia::Pixmap;

/// Flat popover tokens (docs/SPEC.md "Pill" and "Settings UI" tables), light | dark.
#[derive(Clone, Copy, Debug)]
pub struct Theme {
    pub popover: Rgba,
    pub border: Rgba,
    pub border_hover: Rgba,
    pub fg: Rgba,
    pub muted: Rgba,
    pub primary: Rgba,
    pub destructive: Rgba,
    pub success: Rgba,
    /// Amber for warnings (banners, chips).
    pub warn: Rgba,
    /// Colour drawn over a row for hover; pressed doubles its strength.
    pub overlay: Rgba,
    pub hover_alpha: f32,
    pub shadow_alpha: f32,
}

pub fn theme(dark: bool) -> Theme {
    let pal = Palette::get(dark);
    Theme {
        popover: pal.popover,
        border: pal.border,
        border_hover: pal.border_hover,
        fg: pal.fg,
        muted: pal.muted_fg,
        primary: if dark { Rgba::hex(0x3e89ff) } else { Rgba::hex(0x4577e9) },
        destructive: if dark { Rgba::hex(0xee343b) } else { Rgba::hex(0xdc2626) },
        success: if dark { Rgba::hex(0x45ba50) } else { Rgba::hex(0x16a34a) },
        warn: if dark { Rgba::hex(0xf0a23a) } else { Rgba::hex(0xc2750a) },
        overlay: if dark { Rgba::hex(0xffffff) } else { Rgba::hex(0x000000) },
        hover_alpha: if dark { 0.08 } else { 0.055 },
        shadow_alpha: if dark { 0.55 } else { 0.16 },
    }
}

/// Everything the picture depends on.
pub struct View<'a> {
    pub content: &'a Content,
    pub layout: &'a Layout,
    pub nav: Nav,
    pub dark: bool,
    pub scale: f32,
    /// Draw the GDI text (off for the deterministic structural goldens).
    pub text: bool,
}

/// Window size in device px for a layout at `scale`.
pub fn pixel_size(layout: &Layout, scale: f32) -> (u32, u32) {
    let (w, h) = layout.window_size();
    ((w * scale).ceil() as u32, (h * scale).ceil() as u32)
}

pub(crate) const WHITE: Rgba = Rgba::new(1.0, 1.0, 1.0, 1.0);

pub(crate) fn rr(cv: &mut Canvas, r: Rect, radius: f32, color: Rgba, alpha: f32) {
    let (cx, cy, hw, hh) = (r.cx(), r.cy(), r.w() * 0.5, r.h() * 0.5);
    cv.fill_sdf(r.inflate(1.0), color, alpha, |x, y| sd_round_box(x, y, cx, cy, hw, hh, radius));
}

/// An inside stroke of width `sw` along a rounded rect.
pub(crate) fn ring(cv: &mut Canvas, r: Rect, radius: f32, sw: f32, color: Rgba, alpha: f32) {
    let c = r.inflate(-sw * 0.5);
    let (cx, cy, hw, hh) = (c.cx(), c.cy(), c.w() * 0.5, c.h() * 0.5);
    let rad = (radius - sw * 0.5).max(0.0);
    cv.fill_sdf(r.inflate(1.0), color, alpha, |x, y| sd_round_box(x, y, cx, cy, hw, hh, rad).abs() - sw * 0.5);
}

fn to_rect(r: R, dx: f32, dy: f32) -> Rect {
    Rect::new(r.l + dx, r.t + dy, r.r + dx, r.b + dy)
}

/// Text with its glyph cell centred on `cy`.
fn text_at(cv: &mut Canvas, s: &str, font: Font, x: f32, cy: f32, color: Rgba, alpha: f32) {
    if let Some(m) = text_mask(s, font) {
        let cell = content_height(font) / cv.scale;
        draw_text(cv, &m, x, cy - cell * 0.5, color, alpha);
    }
}

fn width_of(s: &str, font: Font, scale: f32) -> f32 {
    text_measure(s, font) / scale
}

/// `s` cut to fit `max_w` logical px, ending in an ellipsis when cut.
pub fn fit(s: &str, font: Font, scale: f32, max_w: f32) -> String {
    if width_of(s, font, scale) <= max_w {
        return s.to_string();
    }
    let chars: Vec<char> = s.chars().collect();
    let (mut lo, mut hi) = (0usize, chars.len());
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        let cand: String = chars[..mid].iter().collect::<String>().trim_end().to_string() + "\u{2026}";
        if width_of(&cand, font, scale) <= max_w {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    chars[..lo].iter().collect::<String>().trim_end().to_string() + "\u{2026}"
}

// ---------------------------------------------------------------------------------------------
// Icons (16 px box, stroke ~1.4)
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Icon {
    Mic,
    Stop,
    History,
    Book,
    Gear,
    Pill,
    Retry,
    Power,
    Copy,
    Search,
    Pencil,
    Trash,
    Play,
    Sliders,
    Keyboard,
    Sparkle,
    Shield,
    Info,
    Check,
    Close,
    Link,
    Download,
    Plus,
}

/// Every icon (tests).
#[cfg(test)]
pub const ALL_ICONS: [Icon; 23] = [
    Icon::Mic,
    Icon::Stop,
    Icon::History,
    Icon::Book,
    Icon::Gear,
    Icon::Pill,
    Icon::Retry,
    Icon::Power,
    Icon::Copy,
    Icon::Search,
    Icon::Pencil,
    Icon::Trash,
    Icon::Play,
    Icon::Sliders,
    Icon::Keyboard,
    Icon::Sparkle,
    Icon::Shield,
    Icon::Info,
    Icon::Check,
    Icon::Close,
    Icon::Link,
    Icon::Download,
    Icon::Plus,
];

pub fn icon_for(t: Target, phase: Phase) -> Icon {
    match t {
        Target::Dictation if phase == Phase::Recording => Icon::Stop,
        Target::Dictation => Icon::Mic,
        Target::History => Icon::History,
        Target::Dictionary => Icon::Book,
        Target::Settings => Icon::Gear,
        Target::Recent(_) => Icon::Copy,
        Target::TogglePill => Icon::Pill,
        Target::Retry => Icon::Retry,
        Target::Quit => Icon::Power,
    }
}

const SW: f32 = 1.4;

fn stroke(d: f32) -> f32 {
    d.abs() - SW * 0.5
}

fn ring_sd(x: f32, y: f32, cx: f32, cy: f32, r: f32) -> f32 {
    stroke((x - cx).hypot(y - cy) - r)
}

/// Signed distance of `icon` at (x, y) in its 16 x 16 box.
fn icon_sd(icon: Icon, x: f32, y: f32) -> f32 {
    match icon {
        Icon::Mic => {
            let body = stroke(sd_round_box(x, y, 8.0, 5.4, 2.5, 4.0, 2.5));
            let arc = ring_sd(x, y, 8.0, 6.4, 5.2).max(6.4 - y);
            let stem = sd_segment(x, y, 8.0, 11.6, 8.0, 14.0) - SW * 0.5;
            let base = sd_segment(x, y, 5.6, 14.0, 10.4, 14.0) - SW * 0.5;
            body.min(arc).min(stem).min(base)
        }
        Icon::Stop => sd_round_box(x, y, 8.0, 8.0, 4.0, 4.0, 1.4),
        Icon::History => {
            let face = ring_sd(x, y, 8.0, 8.0, 6.2);
            let h1 = sd_segment(x, y, 8.0, 4.6, 8.0, 8.0) - SW * 0.5;
            let h2 = sd_segment(x, y, 8.0, 8.0, 10.6, 9.6) - SW * 0.5;
            face.min(h1).min(h2)
        }
        Icon::Book => {
            let cover = stroke(sd_round_box(x, y, 8.0, 8.0, 4.8, 6.0, 1.3));
            let spine = sd_segment(x, y, 6.2, 2.4, 6.2, 13.6) - SW * 0.5;
            let line = sd_segment(x, y, 8.4, 5.8, 10.4, 5.8) - SW * 0.5;
            cover.min(spine).min(line)
        }
        Icon::Gear => {
            let body = ring_sd(x, y, 8.0, 8.0, 4.4);
            let hub = ring_sd(x, y, 8.0, 8.0, 1.5);
            let mut d = body.min(hub);
            for k in 0..8 {
                let a = k as f32 * std::f32::consts::FRAC_PI_4;
                let (s, c) = a.sin_cos();
                d = d.min(sd_segment(x, y, 8.0 + c * 4.6, 8.0 + s * 4.6, 8.0 + c * 6.6, 8.0 + s * 6.6) - 0.9);
            }
            d
        }
        Icon::Pill => {
            let shell = stroke(sd_round_box(x, y, 8.0, 8.0, 6.4, 3.6, 3.6));
            let dot = (x - 5.2).hypot(y - 8.0) - 1.2;
            shell.min(dot)
        }
        Icon::Retry => f32::MAX, // drawn by the card's arrow
        Icon::Power => {
            let g = (x - 8.0).abs() - 2.5;
            let gap = g.max(y - 8.6);
            let arc = ring_sd(x, y, 8.0, 8.6, 5.4).max(-gap);
            let stem = sd_segment(x, y, 8.0, 2.4, 8.0, 8.0) - SW * 0.5;
            arc.min(stem)
        }
        Icon::Copy => {
            let front = sd_round_box(x, y, 9.0, 9.0, 4.0, 4.0, 1.3);
            let back = stroke(sd_round_box(x, y, 6.8, 6.8, 4.0, 4.0, 1.3)).max(-(front - 1.1));
            stroke(front).min(back)
        }
        Icon::Search => ring_sd(x, y, 7.0, 7.0, 4.4).min(line(x, y, (10.3, 10.3), (13.6, 13.6))),
        Icon::Pencil => {
            let body = stroke(sd_segment(x, y, 5.0, 11.0, 11.6, 4.4) - 1.7);
            let tip = line(x, y, (2.8, 13.2), (4.2, 11.8));
            body.min(tip)
        }
        Icon::Trash => {
            let lid = line(x, y, (2.8, 4.4), (13.2, 4.4));
            let grip = line(x, y, (6.4, 2.4), (9.6, 2.4));
            let can = stroke(sd_round_box(x, y, 8.0, 9.4, 3.9, 4.6, 1.2)).max(4.6 - y);
            let r1 = line(x, y, (6.6, 7.4), (6.6, 11.4));
            let r2 = line(x, y, (9.4, 7.4), (9.4, 11.4));
            lid.min(grip).min(can).min(r1).min(r2)
        }
        Icon::Play => {
            // a filled, slightly rounded triangle
            let (a, b, c) = ((5.0, 3.2), (13.0, 8.0), (5.0, 12.8));
            let edge = line(x, y, a, b).min(line(x, y, b, c)).min(line(x, y, c, a));
            let inside = x >= 5.0 && (y - 8.0).abs() <= (13.0 - x) * 0.6;
            if inside { -1.0 } else { edge }
        }
        Icon::Sliders => {
            let mut d = f32::MAX;
            for (yy, knob) in [(4.0, 10.4), (8.0, 5.6), (12.0, 9.2)] {
                d = d.min(line(x, y, (2.4, yy), (13.6, yy)).max(-((x - knob).hypot(y - yy) - 2.6)));
                d = d.min((x - knob).hypot(y - yy) - 1.9);
            }
            d
        }
        Icon::Keyboard => {
            let frame = stroke(sd_round_box(x, y, 8.0, 8.0, 6.6, 4.6, 1.4));
            let mut d = frame;
            for kx in [4.6, 7.0, 9.4, 11.8] {
                d = d.min((x - kx).hypot(y - 6.3) - 0.85);
            }
            d.min(line(x, y, (5.4, 10.0), (10.6, 10.0)))
        }
        Icon::Sparkle => star(x, y, 7.0, 9.0, 5.6).min(star(x, y, 12.6, 3.6, 2.4)),
        Icon::Shield => {
            // flat top, straight sides, then a point at the bottom
            let dx = (x - 8.0).abs();
            let half = if y <= 7.5 { 5.4 } else { 5.4 * ((14.2 - y) / 6.7).max(0.0) };
            let f = (dx - half).max(2.0 - y).max(y - 14.2);
            stroke(f * 0.8)
        }
        Icon::Info => {
            let face = ring_sd(x, y, 8.0, 8.0, 6.2);
            let dot = (x - 8.0).hypot(y - 5.0) - 0.95;
            face.min(dot).min(line(x, y, (8.0, 7.4), (8.0, 11.2)))
        }
        Icon::Check => line(x, y, (3.4, 8.4), (6.6, 11.6)).min(line(x, y, (6.6, 11.6), (12.8, 4.6))),
        Icon::Close => line(x, y, (4.2, 4.2), (11.8, 11.8)).min(line(x, y, (11.8, 4.2), (4.2, 11.8))),
        Icon::Link => {
            let frame = stroke(sd_round_box(x, y, 7.0, 9.0, 4.6, 4.6, 1.3)).max(-((x - 12.0).max(4.0 - y)));
            let arrow = line(x, y, (7.6, 8.4), (13.2, 2.8));
            let head = line(x, y, (9.4, 2.8), (13.2, 2.8)).min(line(x, y, (13.2, 2.8), (13.2, 6.6)));
            frame.min(arrow).min(head)
        }
        Icon::Download => {
            let stem = line(x, y, (8.0, 2.4), (8.0, 10.0));
            let head = line(x, y, (4.8, 7.0), (8.0, 10.2)).min(line(x, y, (8.0, 10.2), (11.2, 7.0)));
            stem.min(head).min(line(x, y, (3.0, 13.4), (13.0, 13.4)))
        }
        Icon::Plus => line(x, y, (8.0, 3.0), (8.0, 13.0)).min(line(x, y, (3.0, 8.0), (13.0, 8.0))),
    }
}

/// A stroked segment.
fn line(x: f32, y: f32, a: (f32, f32), b: (f32, f32)) -> f32 {
    sd_segment(x, y, a.0, a.1, b.0, b.1) - SW * 0.5
}

/// A filled four-pointed star (astroid) of radius `r` at (cx, cy); an approximate distance.
fn star(x: f32, y: f32, cx: f32, cy: f32, r: f32) -> f32 {
    let (dx, dy) = ((x - cx).abs(), (y - cy).abs());
    let k = (dx.powf(2.0 / 3.0) + dy.powf(2.0 / 3.0)).powf(1.5);
    (k - r) * 0.55
}

/// The icon's glyph in the Windows icon font (Segoe Fluent Icons / Segoe MDL2 Assets).
fn system_glyph(icon: Icon) -> char {
    match icon {
        Icon::Mic => '\u{E720}',
        Icon::Stop => '\u{E71A}',
        Icon::History => '\u{E81C}',
        Icon::Book => '\u{E82D}',
        Icon::Gear => '\u{E713}',
        Icon::Pill => '\u{E890}',
        Icon::Retry => '\u{E72C}',
        Icon::Power => '\u{E7E8}',
        Icon::Copy => '\u{E8C8}',
        Icon::Search => '\u{E721}',
        Icon::Pencil => '\u{E70F}',
        Icon::Trash => '\u{E74D}',
        Icon::Play => '\u{E768}',
        Icon::Sliders => '\u{E9E9}',
        Icon::Keyboard => '\u{E765}',
        Icon::Sparkle => '\u{E99A}',
        Icon::Shield => '\u{EA18}',
        Icon::Info => '\u{E946}',
        Icon::Check => '\u{E73E}',
        Icon::Close => '\u{E711}',
        Icon::Link => '\u{E8A7}',
        Icon::Download => '\u{E896}',
        Icon::Plus => '\u{E710}',
    }
}

thread_local! {
    /// Draw icons from the Windows icon font (on unless a text-free render turned it off).
    static SYSTEM_ICONS: std::cell::Cell<bool> = const { std::cell::Cell::new(true) };
}

/// Text-free renders (the structural goldens) keep the vector icons, so they match everywhere.
pub(crate) fn use_system_icons(on: bool) {
    SYSTEM_ICONS.with(|c| c.set(on));
}

pub(crate) fn draw_icon(cv: &mut Canvas, icon: Icon, x: f32, cy: f32, color: Rgba, alpha: f32) {
    let top = cy - 8.0;
    if SYSTEM_ICONS.with(|c| c.get()) {
        let px = (16.0 * cv.scale).round() as i32;
        if let Some(m) = icon_mask(system_glyph(icon), px) {
            draw_icon_mask(cv, &m, x, top, color, alpha);
            return;
        }
    }
    if icon == Icon::Retry {
        let scale = cv.scale;
        draw_retry_icon(cv, Rect::from_xywh(x + 1.0, top + 1.0, 14.0, 14.0), scale, color.with_alpha(alpha));
        return;
    }
    let bb = Rect::new(x - 1.0, top - 1.0, x + 17.0, top + 17.0);
    cv.fill_sdf(bb, color, alpha, |px, py| icon_sd(icon, px - x, py - top));
}

// ---------------------------------------------------------------------------------------------
// The picture
// ---------------------------------------------------------------------------------------------

/// Render the popup window (card plus shadow margin) at `v.scale`.
pub fn render(v: &View) -> Option<Pixmap> {
    use_system_icons(v.text);
    let (w, h) = pixel_size(v.layout, v.scale);
    let mut pm = Pixmap::new(w.max(1), h.max(1))?;
    let th = theme(v.dark);
    let mut cv = Canvas::new(&mut pm, v.scale);
    let card = Rect::from_xywh(MARGIN, MARGIN, v.layout.card_w, v.layout.card_h);

    // Soft shadow, then the 1 px border and the solid surface.
    cv.shadow_rect(card, RADIUS, (0.0, 6.0), 20.0, 0.0, Rgba::new(0.0, 0.0, 0.0, 1.0), th.shadow_alpha);
    cv.shadow_rect(card, RADIUS, (0.0, 1.0), 3.0, 0.0, Rgba::new(0.0, 0.0, 0.0, 1.0), th.shadow_alpha * 0.55);
    rr(&mut cv, card, RADIUS, th.border, 1.0);
    rr(&mut cv, card.inflate(-1.0), RADIUS - 1.0, th.popover, 1.0);

    for (i, row) in v.layout.rows.iter().enumerate() {
        let rect = to_rect(row.rect, MARGIN, MARGIN);
        let active = v.nav.active == Some(i);
        match row.kind {
            Kind::Header => draw_header(&mut cv, v, &th, rect),
            Kind::Primary => draw_primary(&mut cv, v, &th, row, rect, active),
            Kind::Item => draw_item(&mut cv, v, &th, row, rect, active),
            Kind::Recent => draw_recent(&mut cv, v, &th, row, rect, active),
            Kind::Section => {
                if v.text {
                    text_at(&mut cv, &row.label, Font::new(11.5, v.scale, 600), rect.l + 10.0, rect.cy() + 2.0, th.muted, 1.0);
                }
            }
            Kind::Separator => {
                let y = rect.cy();
                let line = Rect::new(rect.l + 1.0 + PAD, y - 0.5, rect.r - 1.0 - PAD, y + 0.5);
                rr(&mut cv, line, 0.0, th.border, 1.0);
            }
        }
    }
    Some(pm)
}

fn draw_header(cv: &mut Canvas, v: &View, th: &Theme, rect: Rect) {
    draw_logo(cv, rect.l + 15.0, rect.cy(), th.fg, 1.0);
    let line = Rect::new(rect.l + 1.0, rect.b - 0.5, rect.r - 1.0, rect.b + 0.5);
    rr(cv, line, 0.0, th.border, 1.0);
    let (label, dot) = match v.content.phase {
        Phase::Ready => (Phase::Ready.label(), th.success),
        Phase::Recording => (Phase::Recording.label(), th.destructive),
        Phase::Transcribing => (Phase::Transcribing.label(), th.primary),
    };
    let status_font = Font::new(12.0, v.scale, 400);
    let sw = if v.text { width_of(label, status_font, v.scale) } else { 44.0 };
    let right = rect.r - 16.0;
    cv.fill_sdf(Rect::new(right - sw - 18.0, rect.cy() - 6.0, right - sw - 4.0, rect.cy() + 6.0), dot, 1.0, |x, y| (x - (right - sw - 10.0)).hypot(y - rect.cy()) - 3.5);
    if v.text {
        text_at(cv, "OpenTalk", Font::new(14.0, v.scale, 600), rect.l + 45.0, rect.cy(), th.fg, 1.0);
        text_at(cv, label, status_font, right - sw, rect.cy(), th.muted, 1.0);
    }
}

fn row_fill(cv: &mut Canvas, th: &Theme, rect: Rect, active: bool, pressed: bool) {
    if active {
        rr(cv, rect, ROW_RADIUS, th.overlay, th.hover_alpha * if pressed { 2.0 } else { 1.0 });
    }
}

fn focus_ring(cv: &mut Canvas, v: &View, rect: Rect, active: bool, color: Rgba) {
    if active && v.nav.keyboard {
        ring(cv, rect, ROW_RADIUS, 1.75, color, 1.0);
    }
}

fn draw_item(cv: &mut Canvas, v: &View, th: &Theme, row: &Row, rect: Rect, active: bool) {
    row_fill(cv, th, rect, active, active && v.nav.pressed);
    focus_ring(cv, v, rect, active, th.primary);
    let a = if row.enabled { 1.0 } else { 0.4 };
    let icon = row.target.map_or(Icon::Mic, |t| icon_for(t, v.content.phase));
    draw_icon(cv, icon, rect.l + 11.0, rect.cy(), th.fg, 0.85 * a);
    if v.text {
        text_at(cv, &row.label, Font::new(13.0, v.scale, 400), rect.l + 38.0, rect.cy(), th.fg, a);
    }
}

fn draw_recent(cv: &mut Canvas, v: &View, th: &Theme, row: &Row, rect: Rect, active: bool) {
    row_fill(cv, th, rect, active, active && v.nav.pressed);
    focus_ring(cv, v, rect, active, th.primary);
    let icon_x = rect.r - 11.0 - 16.0;
    if active {
        draw_icon(cv, Icon::Copy, icon_x, rect.cy(), th.muted, 1.0);
    }
    if v.text {
        let font = Font::new(12.5, v.scale, 400);
        let max_w = (if active { icon_x - 8.0 } else { rect.r - 10.0 }) - (rect.l + 10.0);
        text_at(cv, &fit(&row.label, font, v.scale, max_w), font, rect.l + 10.0, rect.cy(), th.fg, 0.8);
    }
}

fn draw_primary(cv: &mut Canvas, v: &View, th: &Theme, row: &Row, rect: Rect, active: bool) {
    let recording = v.content.phase == Phase::Recording;
    let base = if recording { th.destructive } else { th.primary };
    let (fill, fg, fg_a) = if row.enabled {
        let shade = if active { if v.nav.pressed { 0.2 } else { 0.1 } } else { 0.0 };
        (Rgba::new(base.r * (1.0 - shade), base.g * (1.0 - shade), base.b * (1.0 - shade), 1.0), WHITE, 1.0)
    } else {
        (th.overlay.with_alpha(th.hover_alpha), th.fg, 0.5)
    };
    rr(cv, rect, ROW_RADIUS + 2.0, fill, 1.0);
    // Keyboard focus: a white ring inside the filled row, so it reads on either colour.
    if active && v.nav.keyboard && row.enabled {
        ring(cv, rect.inflate(-2.0), ROW_RADIUS, 1.5, WHITE, 0.95);
    }
    let icon = row.target.map_or(Icon::Mic, |t| icon_for(t, v.content.phase));
    draw_icon(cv, icon, rect.l + 11.0, rect.cy(), fg, fg_a);
    if v.text {
        text_at(cv, &row.label, Font::new(13.0, v.scale, 600), rect.l + 38.0, rect.cy(), fg, fg_a);
    }
    // Hotkey hint as key caps on the right.
    if row.enabled && !v.content.hotkey.is_empty() {
        let font = Font::new(11.0, v.scale, 600);
        let parts: Vec<&str> = v.content.hotkey.split(" + ").collect();
        let caps: Vec<f32> = parts.iter().map(|p| if v.text { width_of(p, font, v.scale) + 12.0 } else { 36.0 }).collect();
        let total = caps.iter().sum::<f32>() + 4.0 * (caps.len().saturating_sub(1)) as f32;
        let mut x = rect.r - 10.0 - total;
        for (p, w) in parts.iter().zip(&caps) {
            let cap = Rect::from_xywh(x, rect.cy() - 10.0, *w, 20.0);
            rr(cv, cap, 5.0, WHITE, 0.2);
            if v.text {
                text_at(cv, p, font, x + 6.0, cap.cy(), WHITE, 1.0);
            }
            x += w + 4.0;
        }
    }
}

// ---------------------------------------------------------------------------------------------
// `--render-tray-popup`
// ---------------------------------------------------------------------------------------------

const SAMPLE_RECENT: [&str; 2] = ["Send the revised budget to Priya before the Thursday review.", "Remind me to renew the domain and check the DNS records."];

/// `--render-tray-popup <out.png> --page <route>`: a page of the popup in a chosen state.
fn cli_render_page(out: &str, page: &str, args: &[String]) -> i32 {
    let flag = |f: &str| args.iter().any(|a| a == f);
    let value = |f: &str| args.iter().position(|a| a == f).and_then(|i| args.get(i + 1)).cloned();
    let opts = super::page::draw::PageCli {
        dark: flag("--dark"),
        scale: value("--scale").and_then(|s| s.parse().ok()).unwrap_or(1.0f32).clamp(0.5, 8.0),
        text: !flag("--no-text"),
        card_h: value("--height").and_then(|s| s.parse().ok()).unwrap_or(super::page::PAGE_MAX_H).clamp(super::popup::PAGE_MIN_H, 2000.0),
        scroll: value("--scroll").and_then(|s| s.parse().ok()).unwrap_or(0.0),
        hover: value("--hover"),
        focus: value("--focus").and_then(|s| s.parse().ok()),
        edit: value("--edit").and_then(|s| s.parse().ok()),
        select_all: flag("--select-all"),
        type_text: value("--type"),
        toast: value("--toast"),
        confirm: flag("--confirm"),
    };
    let mut pm = match super::page::draw::render_page_cli(page, &opts) {
        Ok(pm) => pm,
        Err(e) => {
            eprintln!("{e}");
            return 2;
        }
    };
    if !flag("--transparent") {
        let desk = if opts.dark { [0x10u8, 0x12, 0x15] } else { [0xe9, 0xeb, 0xf0] };
        for p in pm.data_mut().chunks_exact_mut(4) {
            let k = 255 - p[3] as u32;
            for c in 0..3 {
                p[c] = (p[c] as u32 + (desk[c] as u32 * k + 127) / 255).min(255) as u8;
            }
            p[3] = 255;
        }
    }
    match pm.save_png(out) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("could not write {out}: {e}");
            1
        }
    }
}

/// `--render-tray-popup <out.png> [--dark] [--recording|--transcribing] [--recent] [--retry]
/// [--pill-hidden] [--hover <row label>] [--focus] [--scale s] [--transparent] [--no-text]`.
/// Without `--transparent` the popup is composited over a flat desktop colour so the shadow shows.
pub fn cli_render(args: &[String]) -> i32 {
    let Some(out) = args.iter().find(|a| !a.starts_with("--")).cloned() else {
        eprintln!("usage: --render-tray-popup <out.png> [--dark] [--recording|--transcribing] [--recent] [--retry] [--pill-hidden] [--hover <label>] [--focus] [--scale s] [--transparent] [--no-text]");
        eprintln!("       --render-tray-popup <out.png> --page <route|gallery|gallery-list> [--height h] [--scroll px] [--hover id|row:N] [--focus id] [--edit id] [--type text] [--select-all] [--toast text] [--confirm] [--dark] [--scale s] [--transparent] [--no-text]");
        return 2;
    };
    let flag = |f: &str| args.iter().any(|a| a == f);
    let value = |f: &str| args.iter().position(|a| a == f).and_then(|i| args.get(i + 1)).cloned();
    if let Some(page) = value("--page") {
        return cli_render_page(&out, &page, args);
    }
    let content = Content {
        phase: if flag("--recording") {
            Phase::Recording
        } else if flag("--transcribing") {
            Phase::Transcribing
        } else {
            Phase::Ready
        },
        floating_pill_visible: !flag("--pill-hidden"),
        can_retry: flag("--retry"),
        hotkey: "Ctrl + Win".into(),
        recent: if flag("--recent") { SAMPLE_RECENT.iter().map(|s| s.to_string()).collect() } else { Vec::new() },
    };
    let layout = popup::layout(&content);
    let mut nav = if flag("--focus") { Nav::for_keyboard(&layout) } else { Nav::default() };
    if let Some(label) = value("--hover") {
        match layout.rows.iter().position(|r| r.selectable() && r.label.to_lowercase().contains(&label.to_lowercase())) {
            Some(i) => nav.active = Some(i),
            None => {
                eprintln!("no row matches '{label}'");
                return 2;
            }
        }
    }
    let dark = flag("--dark");
    let scale: f32 = value("--scale").and_then(|s| s.parse().ok()).unwrap_or(1.0f32).clamp(0.5, 8.0);
    let view = View { content: &content, layout: &layout, nav, dark, scale, text: !flag("--no-text") };
    let Some(mut pm) = render(&view) else {
        eprintln!("could not render");
        return 1;
    };
    if !flag("--transparent") {
        let desk = if dark { [0x10u8, 0x12, 0x15] } else { [0xe9, 0xeb, 0xf0] };
        for p in pm.data_mut().chunks_exact_mut(4) {
            let k = 255 - p[3] as u32;
            for c in 0..3 {
                p[c] = (p[c] as u32 + (desk[c] as u32 * k + 127) / 255).min(255) as u8;
            }
            p[3] = 255;
        }
    }
    match pm.save_png(&out) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("could not write {out}: {e}");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::tray::popup::CARD_W;

    fn content() -> Content {
        Content { phase: Phase::Ready, floating_pill_visible: true, can_retry: true, hotkey: "Ctrl + Win".into(), recent: SAMPLE_RECENT.iter().map(|s| s.to_string()).collect() }
    }

    fn render_with(c: &Content, nav: Nav, dark: bool, scale: f32) -> Pixmap {
        let layout = popup::layout(c);
        render(&View { content: c, layout: &layout, nav, dark, scale, text: false }).expect("pixmap")
    }

    fn px(pm: &Pixmap, x: u32, y: u32) -> [u8; 4] {
        let p = pm.pixel(x, y).expect("in range");
        [p.red(), p.green(), p.blue(), p.alpha()]
    }

    #[test]
    fn theme_tokens_match_the_spec_in_both_themes() {
        let (l, d) = (theme(false), theme(true));
        assert_eq!(l.popover, Rgba::hex(0xffffff));
        assert_eq!(d.popover, Rgba::hex(0x292b30));
        assert_eq!((l.border, d.border), (Rgba::hex(0xe5e5e5), Rgba::hex(0x383b3e)));
        assert_eq!((l.fg, d.fg), (Rgba::hex(0x171717), Rgba::hex(0xe8e8e8)));
        assert_eq!((l.muted, d.muted), (Rgba::hex(0x525252), Rgba::hex(0x8f8f8f)));
        assert_eq!((l.primary, d.primary), (Rgba::hex(0x4577e9), Rgba::hex(0x3e89ff)));
        assert_eq!((l.destructive, d.destructive), (Rgba::hex(0xdc2626), Rgba::hex(0xee343b)));
        assert_eq!((l.success, d.success), (Rgba::hex(0x16a34a), Rgba::hex(0x45ba50)));
        // Hover is an overlay of the foreground polarity, visible but quiet.
        assert!(l.hover_alpha > 0.03 && l.hover_alpha < 0.12 && d.hover_alpha > 0.03 && d.hover_alpha < 0.15);
    }

    #[test]
    fn surface_is_solid_and_the_corners_are_transparent() {
        for dark in [false, true] {
            let th = theme(dark);
            let c = content();
            let pm = render_with(&c, Nav::default(), dark, 1.0);
            let want = th.popover;
            let at = |x: u32, y: u32| px(&pm, x, y);
            // The window corner is outside everything: (almost) no shadow there.
            assert!(at(0, 0)[3] < 12, "dark={dark}: corner alpha {}", at(0, 0)[3]);
            // A point inside the card (the right padding column, away from rows) is the opaque popover.
            let p = at(MARGIN as u32 + CARD_W as u32 - 3, MARGIN as u32 + 60);
            assert_eq!(p[3], 255, "dark={dark}: the surface is opaque");
            let tol = |a: u8, b: f32| (a as f32 - b * 255.0).abs() <= 1.5;
            assert!(tol(p[0], want.r) && tol(p[1], want.g) && tol(p[2], want.b), "dark={dark}: {p:?}");
            // The 1 px border is the border colour.
            let b = at(MARGIN as u32 + 100, MARGIN as u32);
            assert!(tol(b[0], th.border.r) && tol(b[1], th.border.g) && tol(b[2], th.border.b), "dark={dark}: border {b:?}");
            // The shadow is darker than nothing, below the card, and fades out toward the window edge.
            let (_, h) = pixel_size(&popup::layout(&c), 1.0);
            let below = at(MARGIN as u32 + 100, MARGIN as u32 + popup::layout(&c).card_h as u32 + 3);
            assert!(below[3] > 3, "dark={dark}: shadow under the card");
            assert!(at(MARGIN as u32 + 100, h - 1)[3] < below[3]);
        }
    }

    #[test]
    fn card_height_follows_the_layout_and_dpi() {
        let c = content();
        let layout = popup::layout(&c);
        let (w1, h1) = pixel_size(&layout, 1.0);
        let (w2, h2) = pixel_size(&layout, 2.0);
        assert_eq!((w1, h1), ((CARD_W + 32.0) as u32, (layout.card_h + 32.0).ceil() as u32));
        assert_eq!((w2, h2), ((CARD_W + 32.0) as u32 * 2, ((layout.card_h + 32.0) * 2.0).ceil() as u32));
        let pm = render_with(&c, Nav::default(), false, 2.0);
        assert_eq!((pm.width(), pm.height()), (w2, h2));
    }

    #[test]
    fn hover_pressed_and_focus_states_change_the_row() {
        let c = content();
        let layout = popup::layout(&c);
        let hist = layout.rows.iter().position(|r| r.label == "History").unwrap();
        let r = layout.rows[hist].rect;
        let probe = ((MARGIN + r.l + 100.0) as u32, (MARGIN + (r.t + r.b) / 2.0) as u32);
        for dark in [false, true] {
            let idle = px(&render_with(&c, Nav::default(), dark, 1.0), probe.0, probe.1);
            let hover = px(&render_with(&c, Nav { active: Some(hist), ..Nav::default() }, dark, 1.0), probe.0, probe.1);
            let pressed = px(&render_with(&c, Nav { active: Some(hist), pressed: true, ..Nav::default() }, dark, 1.0), probe.0, probe.1);
            let lum = |p: [u8; 4]| p[0] as i32 + p[1] as i32 + p[2] as i32;
            assert_ne!(idle, hover, "dark={dark}: hover is visible");
            assert!((lum(pressed) - lum(idle)).abs() > (lum(hover) - lum(idle)).abs(), "dark={dark}: pressed is stronger than hover");
            // The keyboard ring is drawn on the row's edge, not on a hovered one.
            let edge = ((MARGIN + r.l + 1.0) as u32, probe.1);
            let plain = px(&render_with(&c, Nav { active: Some(hist), ..Nav::default() }, dark, 1.0), edge.0, edge.1);
            let ringed = px(&render_with(&c, Nav { active: Some(hist), keyboard: true, ..Nav::default() }, dark, 1.0), edge.0, edge.1);
            assert_ne!(plain, ringed, "dark={dark}: focus ring");
        }
    }

    #[test]
    fn primary_row_is_blue_idle_red_recording_and_dim_while_transcribing() {
        let probe = |c: &Content, dark: bool| {
            let layout = popup::layout(c);
            let r = layout.rows[1].rect;
            px(&render_with(c, Nav::default(), dark, 1.0), (MARGIN + r.l + 140.0) as u32, (MARGIN + r.t + 3.0) as u32)
        };
        let ready = Content { phase: Phase::Ready, ..Content::default() };
        let rec = Content { phase: Phase::Recording, ..Content::default() };
        let busy = Content { phase: Phase::Transcribing, ..Content::default() };
        let near = |p: [u8; 4], c: Rgba| (p[0] as f32 - c.r * 255.0).abs() < 2.0 && (p[1] as f32 - c.g * 255.0).abs() < 2.0 && (p[2] as f32 - c.b * 255.0).abs() < 2.0;
        for dark in [false, true] {
            let th = theme(dark);
            assert!(near(probe(&ready, dark), th.primary), "dark={dark} ready");
            assert!(near(probe(&rec, dark), th.destructive), "dark={dark} recording");
            let b = probe(&busy, dark);
            assert!(!near(b, th.primary) && !near(b, th.destructive), "dark={dark}: transcribing is dimmed {b:?}");
        }
    }

    #[test]
    fn light_and_dark_differ_and_text_free_renders_are_deterministic() {
        let c = content();
        let a = render_with(&c, Nav::default(), false, 1.0);
        let b = render_with(&c, Nav::default(), true, 1.0);
        assert_ne!(a.data(), b.data());
        assert_eq!(a.data(), render_with(&c, Nav::default(), false, 1.0).data());
    }

    #[test]
    fn every_icon_draws_something_inside_its_box() {
        for icon in ALL_ICONS {
            let mut pm = Pixmap::new(24, 24).unwrap();
            let mut cv = Canvas::new(&mut pm, 1.0);
            draw_icon(&mut cv, icon, 4.0, 12.0, WHITE, 1.0);
            let lit = pm.pixels().iter().filter(|p| p.alpha() > 100).count();
            assert!(lit > 12, "{icon:?} draws ({lit} px)");
            // Nothing leaks outside the 16 px box (plus the antialiasing pixel).
            for y in 0..24u32 {
                for x in 0..24u32 {
                    if !(3..21).contains(&x) || !(3..21).contains(&y) {
                        assert_eq!(pm.pixel(x, y).unwrap().alpha(), 0, "{icon:?} leaks at {x},{y}");
                    }
                }
            }
        }
    }

    #[test]
    fn icons_map_to_rows() {
        assert_eq!(icon_for(Target::Dictation, Phase::Ready), Icon::Mic);
        assert_eq!(icon_for(Target::Dictation, Phase::Recording), Icon::Stop);
        assert_eq!(icon_for(Target::Settings, Phase::Ready), Icon::Gear);
        assert_eq!(icon_for(Target::Quit, Phase::Ready), Icon::Power);
    }

    #[test]
    fn long_text_is_cut_with_an_ellipsis_and_short_text_is_kept() {
        let f = Font::new(12.5, 1.0, 400);
        assert_eq!(fit("Hi", f, 1.0, 200.0), "Hi");
        let cut = fit("a long long long long long long long long dictation line", f, 1.0, 60.0);
        assert!(cut.ends_with('\u{2026}') && cut.chars().count() < 30, "{cut}");
        assert!(width_of(&cut, f, 1.0) <= 60.5 || text_measure(&cut, f) == 0.0);
        // Without any font metrics (measure 0) nothing is cut.
        assert_eq!(fit("", f, 1.0, 0.0), "");
    }

    // ---- structural goldens (no text, so they match on every machine) --------------------------

    fn golden_dir() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests").join("golden")
    }

    /// The popup without text in its main states, against `tests/golden/tray_popup_*.png`. Regenerate
    /// with `UPDATE_GOLDENS=1`. The shapes, icons, shadow and colours are pure math, so a few stray
    /// pixels of antialiasing is all the tolerance there is.
    #[test]
    fn tray_popup_goldens_match() {
        let update = std::env::var("UPDATE_GOLDENS").is_ok();
        let ready = Content { phase: Phase::Ready, floating_pill_visible: true, can_retry: false, hotkey: "Ctrl + Win".into(), recent: Vec::new() };
        let rec = Content { phase: Phase::Recording, ..content() };
        let cases: Vec<(&str, Content, bool, Option<&str>)> = vec![
            ("tray_popup_light", ready.clone(), false, None),
            ("tray_popup_dark", ready.clone(), true, None),
            ("tray_popup_recording_dark", rec, true, None),
            ("tray_popup_recent_light", content(), false, None),
            ("tray_popup_hover_light", ready.clone(), false, Some("History")),
            ("tray_popup_focus_dark", content(), true, Some("Dictionary")),
        ];
        let mut failures = Vec::new();
        for (name, c, dark, hover) in cases {
            let layout = popup::layout(&c);
            let mut nav = Nav::default();
            if let Some(h) = hover {
                nav.active = layout.rows.iter().position(|r| r.label == h);
                nav.keyboard = name.contains("focus");
            }
            let pm = render(&View { content: &c, layout: &layout, nav, dark, scale: 1.0, text: false }).expect("render");
            let path = golden_dir().join(format!("{name}.png"));
            if update || !path.exists() {
                pm.save_png(&path).unwrap();
                continue;
            }
            let want = Pixmap::load_png(&path).expect("golden png");
            if (want.width(), want.height()) != (pm.width(), pm.height()) {
                failures.push(format!("{name}: size {}x{} != {}x{}", pm.width(), pm.height(), want.width(), want.height()));
                continue;
            }
            let differ = pm.data().chunks_exact(4).zip(want.data().chunks_exact(4)).filter(|(a, b)| a.iter().zip(*b).any(|(x, y)| x.abs_diff(*y) > 2)).count();
            let allowed = (pm.width() * pm.height()) as usize / 1000;
            if differ > allowed {
                failures.push(format!("{name}: {differ} px differ (allowed {allowed})"));
            }
        }
        assert!(failures.is_empty(), "tray popup golden mismatches:\n{}", failures.join("\n"));
    }
}
