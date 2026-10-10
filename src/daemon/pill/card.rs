//! The dictation error card (DictationErrorCard.tsx): title 16 px, description 14 px muted, optional
//! Retry button, countdown stroke along the top edge, auto-dismiss 3/4/5 s by message length.
//! The static part (shadow, surface, text, button) is drawn once into a cached layer; each frame only
//! adds the countdown stroke and composites with the 200 ms entrance/exit.

use super::anim::TW_EASE_OUT;
use super::model::Hit;
use super::raster::{Canvas, Rect, cov, sd_capsule, sd_round_box};
use super::text::{self, Font, LEADING_SNUG};
use super::theme::{Palette, Rgba};
use std::sync::{Arc, Mutex};
use tiny_skia::{FillRule, LineCap, LineJoin, Paint, PathBuilder, Pixmap, Stroke, Transform};

pub const CARD_W: f32 = 442.0;
/// Window = card + 12 px gutter on every side (`inset-x-3 bottom-3`, 24 px window gutter).
pub const GUTTER: f32 = 12.0;
pub const WINDOW_W: f32 = CARD_W + GUTTER * 2.0;
pub const CARD_RADIUS: f32 = 16.0;
pub const FADE_MS: f64 = 200.0;
pub const TITLE_PX: f32 = 16.0;
pub const DESC_PX: f32 = 14.0;
pub const BTN_PX: f32 = 14.0;
pub const BTN_WEIGHT: i32 = 500;
pub const BTN_H: f32 = 32.0;

/// `getDictationErrorDuration`.
pub fn auto_dismiss_ms(title: &str, desc: &str) -> f64 {
    let len = [title, desc].iter().filter(|s| !s.is_empty()).map(|s| s.to_string()).collect::<Vec<_>>().join(" ").trim().encode_utf16().count();
    if len <= 90 {
        3000.0
    } else if len <= 180 {
        4000.0
    } else {
        5000.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum CardHover {
    #[default]
    None,
    Card,
    Retry,
}

#[derive(Clone, Debug)]
pub struct LineAt {
    pub text: String,
    /// Logical top-left of the glyph cell.
    pub x: f32,
    pub y: f32,
    pub font: Font,
}

#[derive(Clone, Debug)]
pub struct CardLayout {
    pub win_w: f32,
    pub win_h: f32,
    pub card: Rect,
    pub retry: Option<Rect>,
    pub retry_label: Option<LineAt>,
    pub retry_icon: Option<Rect>,
    pub title: Vec<LineAt>,
    pub desc: Vec<LineAt>,
}

pub struct ErrorCard {
    pub title: String,
    pub desc: String,
    pub retry: bool,
    shown_at: f64,
    duration: f64,
    paused_at: Option<f64>,
    paused_ms: f64,
    exit_at: Option<f64>,
    pub hover: CardHover,
    layout_cache: Mutex<Option<(u32, Arc<CardLayout>)>>,
}

impl ErrorCard {
    pub fn new(title: &str, desc: &str, retry: bool, now: f64) -> ErrorCard {
        ErrorCard {
            title: title.to_string(),
            desc: desc.to_string(),
            retry,
            shown_at: now,
            duration: auto_dismiss_ms(title, desc),
            paused_at: None,
            paused_ms: 0.0,
            exit_at: None,
            hover: CardHover::None,
            layout_cache: Mutex::new(None),
        }
    }

    /// Milliseconds of countdown consumed (pauses excluded).
    pub fn elapsed(&self, now: f64) -> f64 {
        let paused = self.paused_ms + self.paused_at.map(|p| now - p).unwrap_or(0.0);
        (now - self.shown_at - paused).max(0.0)
    }

    /// 0..1 portion of the countdown still to run.
    pub fn remaining_fraction(&self, now: f64) -> f32 {
        (1.0 - self.elapsed(now) / self.duration).clamp(0.0, 1.0) as f32
    }

    /// Hovering the card pauses the countdown; leaving resumes it.
    pub fn set_hover(&mut self, h: CardHover, now: f64) {
        if h == self.hover {
            return;
        }
        let was = self.hover != CardHover::None;
        let is = h != CardHover::None;
        self.hover = h;
        if is && !was && self.exit_at.is_none() {
            self.paused_at = Some(now);
        } else if !is && was {
            if let Some(p) = self.paused_at.take() {
                self.paused_ms += now - p;
            }
        }
    }

    /// Start the exit fade without telling the owner (a new dictation took over).
    pub fn dismiss_now(&mut self, now: f64) {
        if self.exit_at.is_none() {
            self.exit_at = Some(now);
        }
    }

    #[cfg(test)]
    pub fn is_exiting(&self) -> bool {
        self.exit_at.is_some()
    }

    /// Advance the timer. Returns `true` exactly once when the countdown ran out.
    pub fn update(&mut self, now: f64) -> bool {
        if self.exit_at.is_none() && self.paused_at.is_none() && self.elapsed(now) >= self.duration {
            self.exit_at = Some(now);
            return true;
        }
        false
    }

    pub fn finished(&self, now: f64) -> bool {
        self.exit_at.is_some_and(|e| now >= e + FADE_MS)
    }

    pub fn is_animating(&self, now: f64) -> bool {
        if now < self.shown_at + FADE_MS || self.exit_at.is_some() {
            return true;
        }
        self.paused_at.is_none()
    }

    /// (opacity, translate_y px, scale) for the entrance/exit transitions.
    pub fn presence(&self, now: f64) -> (f32, f32, f32) {
        // enter: translate-y-1 (4 px) + opacity 0 -> rest
        let enter = ((now - self.shown_at) / FADE_MS).clamp(0.0, 1.0) as f32;
        let e = TW_EASE_OUT.ease(enter);
        let mut op = e;
        let mut ty = 4.0 * (1.0 - e);
        let mut sc = 1.0;
        if let Some(x) = self.exit_at {
            let p = TW_EASE_OUT.ease(((now - x) / FADE_MS).clamp(0.0, 1.0) as f32);
            op *= 1.0 - p;
            ty += 8.0 * p;
            sc = 1.0 - 0.02 * p;
        }
        (op, ty, sc)
    }

    pub fn layout(&self, scale: f32) -> Arc<CardLayout> {
        let key = (scale * 64.0).round() as u32;
        let mut c = self.layout_cache.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((k, l)) = c.as_ref() {
            if *k == key {
                return l.clone();
            }
        }
        let l = Arc::new(compute_layout(&self.title, &self.desc, self.retry, scale));
        *c = Some((key, l.clone()));
        l
    }

    pub fn hit_test(&self, lx: f32, ly: f32, scale: f32) -> Hit {
        let l = self.layout(scale);
        if let Some(r) = l.retry {
            if r.contains(lx, ly) {
                return Hit::ErrorRetry;
            }
        }
        if l.card.contains(lx, ly) {
            return Hit::ErrorCard;
        }
        Hit::None
    }
}

fn compute_layout(title: &str, desc: &str, retry: bool, scale: f32) -> CardLayout {
    let s = scale;
    let inv = 1.0 / s;
    let tf = Font::new(TITLE_PX, s, 400);
    let df = Font::new(DESC_PX, s, 400);
    let bf = Font::new(BTN_PX, s, BTN_WEIGHT);
    // content box inside border (1) and px-2 (8)
    let content_w = CARD_W - 2.0 - 16.0;
    let label = "Retry";
    let label_w = text::measure(label, bf) * inv;
    let (btn_w, text_avail) = if retry {
        let bw = 16.0 + 14.0 + 6.0 + label_w + 16.0;
        (bw, content_w - CARD_GAP - bw - 16.0)
    } else {
        (0.0, content_w - 16.0)
    };
    let dev = |w: f32| w * s;
    let title_lines = if title.is_empty() { vec![] } else { clamp_lines(text::wrap(&clamp_chars(title, MAX_TITLE_CHARS), tf, dev(text_avail)), MAX_TITLE_LINES) };
    let desc_lines = if desc.is_empty() { vec![] } else { clamp_lines(text::wrap(&clamp_chars(desc, MAX_DESC_CHARS), df, dev(text_avail)), MAX_DESC_LINES) };
    let t_lh = TITLE_PX * LEADING_SNUG;
    let d_lh = DESC_PX * LEADING_SNUG;
    let title_h = t_lh * title_lines.len() as f32;
    let desc_h = d_lh * desc_lines.len() as f32;
    let gap = if !title_lines.is_empty() && !desc_lines.is_empty() { 4.0 } else { 0.0 };
    let text_h = 4.0 + title_h + gap + desc_h + 4.0;
    let content_h = if retry { text_h.max(BTN_H) } else { text_h };
    let card_h = 2.0 + 24.0 + content_h;
    let win_h = card_h + GUTTER * 2.0;
    let card = Rect::from_xywh(GUTTER, GUTTER, CARD_W, card_h);
    let x0 = card.l + 1.0 + 8.0;
    let y0 = card.t + 1.0 + 12.0;
    let tx = x0 + 8.0;
    let ty = y0 + (content_h - text_h) * 0.5 + 4.0;
    let cell = |f: Font, lh: f32| (text::content_height(f) * inv, lh);
    let (t_cell, _) = cell(tf, t_lh);
    let (d_cell, _) = cell(df, d_lh);
    let mut y = ty;
    let mut title_out = Vec::new();
    for l in title_lines {
        title_out.push(LineAt { text: l, x: tx, y: y + (t_lh - t_cell) * 0.5, font: tf });
        y += t_lh;
    }
    y += gap;
    let mut desc_out = Vec::new();
    for l in desc_lines {
        desc_out.push(LineAt { text: l, x: tx, y: y + (d_lh - d_cell) * 0.5, font: df });
        y += d_lh;
    }
    let (retry_rect, retry_label, retry_icon) = if retry {
        let bx = card.r - 1.0 - 8.0 - btn_w;
        let by = y0 + (content_h - BTN_H) * 0.5;
        let r = Rect::from_xywh(bx, by, btn_w, BTN_H);
        let b_cell = text::content_height(bf) * inv;
        let icon = Rect::from_xywh(bx + 16.0, by + (BTN_H - 14.0) * 0.5, 14.0, 14.0);
        let lab = LineAt { text: label.to_string(), x: bx + 16.0 + 14.0 + 6.0, y: by + (BTN_H - b_cell) * 0.5, font: bf };
        (Some(r), Some(lab), Some(icon))
    } else {
        (None, None, None)
    };
    CardLayout { win_w: WINDOW_W, win_h, card, retry: retry_rect, retry_label, retry_icon, title: title_out, desc: desc_out }
}

const CARD_GAP: f32 = 8.0;

/// Bounds for pathological messages (a raw provider body): keeps the window on screen and the
/// per-word GDI measuring off the UI thread's critical path. Normal messages never reach them.
const MAX_TITLE_CHARS: usize = 160;
const MAX_DESC_CHARS: usize = 900;
const MAX_TITLE_LINES: usize = 3;
const MAX_DESC_LINES: usize = 10;

/// At most `max` chars, ending in an ellipsis when cut.
fn clamp_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('\u{2026}');
    out
}

/// At most `max` lines; the last kept line ends in an ellipsis when lines were dropped.
fn clamp_lines(mut lines: Vec<String>, max: usize) -> Vec<String> {
    if lines.len() > max {
        lines.truncate(max);
        if let Some(last) = lines.last_mut() {
            last.push('\u{2026}');
        }
    }
    lines
}

/// Static layer: shadow, surface, border, text and the button.
/// `None` when the layer's size is not representable (degenerate scale).
pub fn draw_base(card: &ErrorCard, scale: f32, pal: &Palette) -> Option<Pixmap> {
    let l = card.layout(scale);
    let w = (l.win_w * scale).ceil() as u32;
    let h = (l.win_h * scale).ceil() as u32;
    let mut pm = Pixmap::new(w.max(1), h.max(1))?;
    {
        let mut cv = Canvas::new(&mut pm, scale);
        let r = l.card;
        let s = scale;
        if pal.dark {
            // --shadow-modal: 0 24px 48px -12px rgba(0,0,0,.4)
            cv.shadow_rect(r, CARD_RADIUS, (0.0, 24.0), 48.0, -12.0, Rgba::new(0.0, 0.0, 0.0, 0.4), 1.0);
        }
        let outer = |x: f32, y: f32| sd_round_box(x, y, r.cx(), r.cy(), r.w() * 0.5, r.h() * 0.5, CARD_RADIUS);
        let inner = |x: f32, y: f32| sd_round_box(x, y, r.cx(), r.cy(), r.w() * 0.5 - 1.0, r.h() * 0.5 - 1.0, CARD_RADIUS - 1.0);
        cv.fill_sdf(r.inflate(1.0), pal.surface0, 1.0, outer);
        let border = pal.border.with_alpha(0.5);
        cv.shade_sdf(r.inflate(1.0), outer, |x, y, c| {
            let k = (c - cov(inner(x, y) * s)).max(0.0) * border.a;
            [border.r * k, border.g * k, border.b * k, k]
        });
        for t in &l.title {
            if let Some(m) = text::mask(&t.text, t.font) {
                text::draw(&mut cv, &m, t.x, t.y, pal.fg, 1.0);
            }
        }
        for t in &l.desc {
            if let Some(m) = text::mask(&t.text, t.font) {
                text::draw(&mut cv, &m, t.x, t.y, pal.muted_fg, 1.0);
            }
        }
        if let (Some(b), Some(lab), Some(icon)) = (l.retry, &l.retry_label, l.retry_icon) {
            let bg = if card.hover == CardHover::Retry { pal.fg.with_alpha(0.9) } else { pal.fg };
            cv.fill_sdf(b.inflate(1.0), bg, 1.0, |x, y| sd_capsule(x, y, b.cx(), b.cy(), b.w(), b.h()));
            let fg = pal.surface0; // text-background
            if let Some(m) = text::mask(&lab.text, lab.font) {
                text::draw(&mut cv, &m, lab.x, lab.y, fg, 1.0);
            }
            draw_retry_icon(&mut cv, icon, scale, fg);
        }
    }
    Some(pm)
}

/// Nucleo `Redo2Outline18` arrow at `rect`, stroke 1.5 in 18-units.
fn draw_retry_icon(cv: &mut Canvas, rect: Rect, scale: f32, color: Rgba) {
    let (w, h) = (cv.w as u32, cv.h as u32);
    let Some(mut pm) = tiny_skia::PixmapMut::from_bytes(&mut *cv.data, w, h) else { return };
    let k = rect.w() / 18.0;
    let tf = Transform::from_scale(k * scale, k * scale).post_translate(rect.l * scale, rect.t * scale);
    let mut pb = PathBuilder::new();
    pb.move_to(15.0002, 12.0949);
    pb.cubic_to(13.8781, 14.2659, 11.6124, 15.75, 9.0, 15.75);
    pb.cubic_to(5.272, 15.75, 2.25, 12.7279, 2.25, 9.0);
    pb.cubic_to(2.25, 5.2721, 5.272, 2.25, 9.0, 2.25);
    pb.cubic_to(10.864, 2.25, 12.5515, 3.0055, 13.773, 4.227);
    let Some(arc) = pb.finish() else { return };
    let mut head = PathBuilder::new();
    head.move_to(11.614, 5.6341);
    head.line_to(15.5736, 6.2422);
    head.line_to(15.0759, 2.4461);
    head.close();
    let Some(head) = head.finish() else { return };
    let mut paint = Paint::default();
    paint.set_color(color.to_color());
    paint.anti_alias = true;
    let stroke = Stroke { width: 1.5, line_cap: LineCap::Round, line_join: LineJoin::Round, ..Default::default() };
    pm.stroke_path(&arc, &paint, &stroke, tf, None);
    pm.fill_path(&head, &paint, FillRule::Winding, tf, None);
    pm.stroke_path(&head, &paint, &stroke, tf, None);
}

/// Countdown stroke along the top edge (`M 1 16 A 15 15 0 0 1 16 1 H 426 A 15 15 0 0 1 441 16`),
/// visible portion `[0, fraction]` of the path length, 2 px foreground.
pub fn draw_countdown(pm: &mut Pixmap, layout: &CardLayout, scale: f32, color: Rgba, fraction: f32) {
    if fraction <= 0.001 {
        return;
    }
    let arc = std::f32::consts::FRAC_PI_2 * 15.0;
    let line = 410.0;
    let total = arc * 2.0 + line;
    let mut visible = fraction.clamp(0.0, 1.0) * total;
    let mut pts: Vec<(f32, f32)> = Vec::new();
    // arc 1: centre (16,16), from (1,16) to (16,1)
    let seg1 = visible.min(arc);
    let n1 = ((seg1 / arc) * 16.0).ceil().max(1.0) as i32;
    for i in 0..=n1 {
        let a = (seg1 / arc) * (i as f32 / n1 as f32) * std::f32::consts::FRAC_PI_2;
        pts.push((16.0 - 15.0 * a.cos(), 16.0 - 15.0 * a.sin()));
    }
    visible -= seg1;
    if visible > 0.0 {
        let seg2 = visible.min(line);
        pts.push((16.0 + seg2, 1.0));
        visible -= seg2;
        if visible > 0.0 {
            let seg3 = visible.min(arc);
            let n3 = ((seg3 / arc) * 16.0).ceil().max(1.0) as i32;
            for i in 1..=n3 {
                let a = (seg3 / arc) * (i as f32 / n3 as f32) * std::f32::consts::FRAC_PI_2;
                pts.push((426.0 + 15.0 * a.sin(), 16.0 - 15.0 * a.cos()));
            }
        }
    }
    let mut pb = PathBuilder::new();
    for (i, p) in pts.iter().enumerate() {
        if i == 0 {
            pb.move_to(p.0, p.1);
        } else {
            pb.line_to(p.0, p.1);
        }
    }
    let Some(path) = pb.finish() else { return };
    // svg: absolute inset-x-0 top-0 inside the 1 px border; width 440, height 16 for a 442x17 viewBox
    let sx = (CARD_W - 2.0) / 442.0;
    let sy = 16.0 / 17.0;
    let ox = layout.card.l + 1.0;
    let oy = layout.card.t + 1.0;
    let tf = Transform::from_scale(sx, sy).post_translate(ox, oy).post_scale(scale, scale);
    let mut paint = Paint::default();
    paint.set_color(color.to_color());
    paint.anti_alias = true;
    // non-scaling stroke: 2 device-independent px
    let stroke = Stroke { width: 2.0 / (sx.min(sy)), line_cap: LineCap::Butt, line_join: LineJoin::Miter, ..Default::default() };
    pm.stroke_path(&path, &paint, &stroke, tf, None);
}

static BASE_CACHE: Mutex<Option<(BaseKey, Arc<Pixmap>)>> = Mutex::new(None);

#[derive(PartialEq, Eq, Clone)]
struct BaseKey {
    title: String,
    desc: String,
    retry: bool,
    hover_retry: bool,
    scale: u32,
    dark: bool,
}

/// Static layer, cached across frames.
pub fn base_layer(card: &ErrorCard, scale: f32, pal: &Palette) -> Option<Arc<Pixmap>> {
    let key = BaseKey {
        title: card.title.clone(),
        desc: card.desc.clone(),
        retry: card.retry,
        hover_retry: card.hover == CardHover::Retry,
        scale: (scale * 64.0).round() as u32,
        dark: pal.dark,
    };
    let mut c = BASE_CACHE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((k, p)) = c.as_ref() {
        if *k == key {
            return Some(p.clone());
        }
    }
    let p = Arc::new(draw_base(card, scale, pal)?);
    *c = Some((key, p.clone()));
    Some(p)
}

#[cfg(test)]
mod tests {

    #[test]
    fn pathological_text_is_bounded() {
        let huge = "word ".repeat(20_000);
        let start = std::time::Instant::now();
        let l = compute_layout(&"T".repeat(5000), &huge, true, 1.0);
        assert!(l.desc.len() <= MAX_DESC_LINES && l.title.len() <= MAX_TITLE_LINES);
        assert!(l.win_h < 480.0, "{}", l.win_h);
        assert!(l.desc.last().is_some_and(|s| s.text.ends_with('\u{2026}')));
        assert!(start.elapsed().as_secs() < 5);
        // an ordinary message is untouched
        let n = compute_layout("Transcription failed", "The speech provider rejected your API key.", true, 1.0);
        assert!(n.desc.iter().all(|s| !s.text.ends_with('\u{2026}')));
    }
    use super::*;

    #[test]
    fn duration_buckets() {
        assert_eq!(auto_dismiss_ms("Short", "text"), 3000.0);
        assert_eq!(auto_dismiss_ms(&"a".repeat(50), &"b".repeat(60)), 4000.0);
        assert_eq!(auto_dismiss_ms(&"a".repeat(100), &"b".repeat(100)), 5000.0);
        // title + space + description counts
        assert_eq!(auto_dismiss_ms(&"a".repeat(45), &"b".repeat(45)), 4000.0);
        assert_eq!(auto_dismiss_ms(&"a".repeat(44), &"b".repeat(45)), 3000.0);
    }

    #[test]
    fn countdown_pauses_on_hover() {
        let mut c = ErrorCard::new("Title", "desc", true, 0.0);
        assert_eq!(c.remaining_fraction(0.0), 1.0);
        assert!((c.remaining_fraction(1500.0) - 0.5).abs() < 1e-5);
        c.set_hover(CardHover::Card, 1500.0);
        assert!((c.remaining_fraction(2500.0) - 0.5).abs() < 1e-5, "paused");
        assert!(!c.update(5000.0));
        c.set_hover(CardHover::None, 3000.0);
        assert!((c.remaining_fraction(3500.0) - 0.333).abs() < 1e-2);
        assert!(!c.update(4400.0));
        assert!(c.update(4500.0));
        assert!(!c.update(4600.0), "reported once");
        assert!(c.is_exiting());
        assert!(!c.finished(4600.0) && c.finished(4700.0));
    }

    #[test]
    fn presence_fades() {
        let c = ErrorCard::new("a", "b", false, 100.0);
        let (o0, y0, _) = c.presence(100.0);
        assert_eq!(o0, 0.0);
        assert_eq!(y0, 4.0);
        let (o1, y1, s1) = c.presence(400.0);
        assert_eq!((o1, y1, s1), (1.0, 0.0, 1.0));
    }

    #[test]
    fn layout_is_442_wide_with_gutter() {
        let c = ErrorCard::new("Transcription failed", "The speech provider rejected your API key. Check it in Settings.", true, 0.0);
        let l = c.layout(1.0);
        assert_eq!(l.win_w, 466.0);
        assert_eq!(l.card.w(), 442.0);
        assert_eq!(l.card.l, 12.0);
        assert!(l.win_h > 24.0 + 60.0 && l.win_h < 200.0, "{}", l.win_h);
        assert_eq!(l.card.b + 12.0, l.win_h);
        let r = l.retry.expect("retry");
        assert_eq!(r.h(), 32.0);
        assert!((r.r - (l.card.r - 9.0)).abs() < 0.01);
        assert!(!l.title.is_empty() && !l.desc.is_empty());
        assert!(c.hit_test(r.cx(), r.cy(), 1.0) == Hit::ErrorRetry);
        assert!(c.hit_test(30.0, l.card.t + 5.0, 1.0) == Hit::ErrorCard);
        assert!(c.hit_test(2.0, 2.0, 1.0) == Hit::None);
    }

    #[test]
    fn long_text_wraps_to_more_lines() {
        let long = "word ".repeat(80);
        let c = ErrorCard::new("Title", &long, false, 0.0);
        let short = ErrorCard::new("Title", "x", false, 0.0);
        assert!(c.layout(1.0).win_h > short.layout(1.0).win_h + 40.0);
    }
}
