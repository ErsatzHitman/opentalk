//! SDF-drawn controls: toggle switch, segmented control, radio dot, buttons, input box, chips,
//! chevrons, back arrow, check, x, trash, plus, search, caret, scrollbar. No icon font, no bitmaps:
//! every shape is a signed-distance function on the pill's software rasteriser, so a control looks
//! the same on every Windows version and DPI. [`Painter`] bundles the canvas, the theme and the text
//! path (with a clip rectangle, since the body is clipped to its viewport).

use super::metrics::TextMetrics;
use super::{BtnStyle, Tone};
use crate::daemon::pill::kit::{Canvas, Font, Rect, Rgba, content_height, sd_segment, text_mask};
use crate::daemon::tray::popup_draw::{Theme, WHITE, rr, ring};

/// Colour of a tone.
pub fn tone_color(th: &Theme, tone: Tone) -> Rgba {
    match tone {
        Tone::Info => th.primary,
        Tone::Warn => th.warn,
        Tone::Ok => th.success,
        Tone::Err => th.destructive,
    }
}

/// Everything a control needs to paint itself.
pub struct Painter<'a, 'b> {
    pub cv: Canvas<'a>,
    pub th: Theme,
    /// Draw the GDI text (off for the deterministic structural goldens).
    pub text: bool,
    pub m: &'b dyn TextMetrics,
    /// Text is clipped to this logical rect (canvas space).
    pub clip: Option<Rect>,
}

impl<'a, 'b> Painter<'a, 'b> {
    pub fn new(cv: Canvas<'a>, th: Theme, text: bool, m: &'b dyn TextMetrics) -> Painter<'a, 'b> {
        crate::daemon::tray::popup_draw::use_system_icons(text);
        Painter { cv, th, text, m, clip: None }
    }

    /// Text with its glyph cell centred on `cy`, left edge at `x`.
    #[allow(clippy::too_many_arguments)]
    pub fn text(&mut self, s: &str, px: f32, weight: i32, x: f32, cy: f32, color: Rgba, alpha: f32) {
        if !self.text || s.is_empty() {
            return;
        }
        let scale = self.cv.scale;
        let font = Font::new(px, scale, weight);
        let Some(mask) = text_mask(s, font) else { return };
        let cell = content_height(font) / scale;
        let a = color.a * alpha;
        if a <= 0.0 {
            return;
        }
        let ox = (x * scale).round() as i32 - 1; // the mask has 1 px of left padding
        let oy = ((cy - cell * 0.5) * scale).round() as i32;
        let (cx0, cy0, cx1, cy1) = match self.clip {
            Some(c) => (((c.l * scale).floor() as i32).max(0), ((c.t * scale).floor() as i32).max(0), (c.r * scale).ceil() as i32, (c.b * scale).ceil() as i32),
            None => (0, 0, i32::MAX, i32::MAX),
        };
        for j in 0..mask.h {
            let y = oy + j;
            if y < cy0 || y >= cy1 {
                continue;
            }
            let row = &mask.data[(j * mask.w) as usize..((j + 1) * mask.w) as usize];
            for (i, &c) in row.iter().enumerate() {
                let x = ox + i as i32;
                if c == 0 || x < cx0 || x >= cx1 {
                    continue;
                }
                let k = c as f32 / 255.0 * a;
                self.cv.blend(x, y, [color.r * k, color.g * k, color.b * k, k]);
            }
        }
    }

    /// Width of `s` as drawn.
    pub fn width(&self, s: &str, px: f32, weight: i32) -> f32 {
        self.m.width(s, px, weight)
    }

    /// `s` cut to `max_w` with an ellipsis.
    pub fn fit(&self, s: &str, px: f32, weight: i32, max_w: f32) -> String {
        fit_text(s, max_w, &|t| self.m.width(t, px, weight))
    }

    pub fn fill(&mut self, r: Rect, radius: f32, color: Rgba, alpha: f32) {
        rr(&mut self.cv, r, radius, color, alpha);
    }

    pub fn stroke(&mut self, r: Rect, radius: f32, sw: f32, color: Rgba, alpha: f32) {
        ring(&mut self.cv, r, radius, sw, color, alpha);
    }

    pub fn hline(&mut self, l: f32, r: f32, y: f32, color: Rgba, alpha: f32) {
        rr(&mut self.cv, Rect::new(l, y - 0.5, r, y + 0.5), 0.0, color, alpha);
    }

    pub fn focus_ring(&mut self, r: Rect, radius: f32) {
        let c = self.th.primary;
        self.stroke(r.inflate(1.0), radius + 1.0, 1.75, c, 1.0);
    }

    pub fn circle(&mut self, cx: f32, cy: f32, rad: f32, color: Rgba, alpha: f32) {
        self.cv.fill_sdf(Rect::new(cx - rad - 1.0, cy - rad - 1.0, cx + rad + 1.0, cy + rad + 1.0), color, alpha, |x, y| (x - cx).hypot(y - cy) - rad);
    }

    pub fn ring_circle(&mut self, cx: f32, cy: f32, rad: f32, sw: f32, color: Rgba, alpha: f32) {
        self.cv.fill_sdf(Rect::new(cx - rad - 1.0, cy - rad - 1.0, cx + rad + 1.0, cy + rad + 1.0), color, alpha, |x, y| ((x - cx).hypot(y - cy) - (rad - sw * 0.5)).abs() - sw * 0.5);
    }

    /// Hover / pressed wash over a rect.
    pub fn wash(&mut self, r: Rect, radius: f32, pressed: bool) {
        let (c, a) = (self.th.overlay, self.th.hover_alpha * if pressed { 2.0 } else { 1.0 });
        self.fill(r, radius, c, a);
    }
}

/// `s` cut to fit `max_w`, ending in an ellipsis when cut (binary search over chars).
pub fn fit_text(s: &str, max_w: f32, width: &dyn Fn(&str) -> f32) -> String {
    if width(s) <= max_w {
        return s.to_string();
    }
    let chars: Vec<char> = s.chars().collect();
    let (mut lo, mut hi) = (0usize, chars.len());
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        let cand: String = chars[..mid].iter().collect::<String>().trim_end().to_string() + "\u{2026}";
        if width(&cand) <= max_w {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    chars[..lo].iter().collect::<String>().trim_end().to_string() + "\u{2026}"
}

// ---------------------------------------------------------------------------------------------
// Glyphs (16 px box, stroke 1.5)
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Glyph {
    ChevronRight,
    ChevronLeft,
    Close,
}

const GSW: f32 = 1.5;

fn seg(x: f32, y: f32, a: (f32, f32), b: (f32, f32)) -> f32 {
    sd_segment(x, y, a.0, a.1, b.0, b.1) - GSW * 0.5
}

fn poly(x: f32, y: f32, pts: &[(f32, f32)]) -> f32 {
    pts.windows(2).map(|w| seg(x, y, w[0], w[1])).fold(f32::MAX, f32::min)
}

/// Signed distance of a glyph at (x, y) in its 16 x 16 box.
pub fn glyph_sd(g: Glyph, x: f32, y: f32) -> f32 {
    match g {
        Glyph::ChevronRight => poly(x, y, &[(6.0, 3.5), (10.5, 8.0), (6.0, 12.5)]),
        Glyph::ChevronLeft => poly(x, y, &[(10.0, 3.5), (5.5, 8.0), (10.0, 12.5)]),
        Glyph::Close => poly(x, y, &[(4.0, 4.0), (12.0, 12.0)]).min(poly(x, y, &[(12.0, 4.0), (4.0, 12.0)])),
    }
}

impl Painter<'_, '_> {
    /// A glyph with its 16 px box at (x, cy - 8).
    pub fn glyph(&mut self, g: Glyph, x: f32, cy: f32, color: Rgba, alpha: f32) {
        let top = cy - 8.0;
        self.cv.fill_sdf(Rect::new(x - 1.0, top - 1.0, x + 17.0, top + 17.0), color, alpha, |px, py| glyph_sd(g, px - x, py - top));
    }

    /// A glyph centred in `r`.
    pub fn glyph_in(&mut self, g: Glyph, r: Rect, color: Rgba, alpha: f32) {
        self.glyph(g, r.cx() - 8.0, r.cy(), color, alpha);
    }

    /// Toggle switch (36 x 20).
    pub fn switch(&mut self, r: Rect, on: bool, hover: bool, pressed: bool) {
        let th = self.th;
        let (c, a) = if on { (th.primary, if pressed { 0.85 } else { 1.0 }) } else { (th.fg, if hover || pressed { 0.34 } else { 0.24 }) };
        self.fill(r, r.h() * 0.5, c, a);
        let kx = if on { r.r - r.h() * 0.5 } else { r.l + r.h() * 0.5 };
        // A faint shadow under the knob so it reads on the pale track.
        self.circle(kx, r.cy() + 0.6, r.h() * 0.5 - 1.8, Rgba::new(0.0, 0.0, 0.0, 1.0), 0.18);
        self.circle(kx, r.cy(), r.h() * 0.5 - 2.2, WHITE, 1.0);
    }

    /// Radio dot (16 px) centred at (cx, cy).
    pub fn radio_dot(&mut self, cx: f32, cy: f32, on: bool) {
        let th = self.th;
        if on {
            self.ring_circle(cx, cy, 8.0, 1.75, th.primary, 1.0);
            self.circle(cx, cy, 3.6, th.primary, 1.0);
        } else {
            self.ring_circle(cx, cy, 8.0, 1.25, th.fg, 0.34);
        }
    }

    /// Segmented control bar; `labels` are drawn centred, the selected one on a raised pill.
    pub fn segmented(&mut self, ctl: Rect, labels: &[String], sel: usize, hover: Option<usize>, pressed: Option<usize>) {
        let th = self.th;
        self.fill(ctl, 8.0, th.overlay, if th.hover_alpha > 0.07 { 0.07 } else { 0.06 });
        self.stroke(ctl, 8.0, 1.0, th.border, 1.0);
        let n = labels.len().max(1);
        let w = ctl.w() / n as f32;
        for (i, label) in labels.iter().enumerate() {
            let seg = Rect::new(ctl.l + w * i as f32, ctl.t, ctl.l + w * (i + 1) as f32, ctl.b);
            let inner = Rect::new(seg.l + 2.5, seg.t + 2.5, seg.r - 2.5, seg.b - 2.5);
            let selected = i == sel;
            if selected {
                let (face, edge) = if th.hover_alpha > 0.07 { (th.border, th.border_hover) } else { (WHITE, th.border) };
                self.fill(inner.offset(0.0, 0.6), 6.0, Rgba::new(0.0, 0.0, 0.0, 1.0), 0.10);
                self.fill(inner, 6.0, face, 1.0);
                self.stroke(inner, 6.0, 1.0, edge, 1.0);
            } else if hover == Some(i) || pressed == Some(i) {
                self.wash(inner, 6.0, pressed == Some(i));
            }
            let (weight, color, alpha) = if selected { (600, th.fg, 1.0) } else { (400, th.fg, 0.72) };
            let text = self.fit(label, 12.5, weight, w - 10.0);
            let tw = self.width(&text, 12.5, weight);
            self.text(&text, 12.5, weight, seg.cx() - tw * 0.5, seg.cy(), color, alpha);
        }
    }

    /// Push button.
    #[allow(clippy::too_many_arguments)]
    pub fn button(&mut self, r: Rect, label: &str, style: BtnStyle, enabled: bool, hover: bool, pressed: bool, focus: bool) {
        let th = self.th;
        let dim = if enabled { 1.0 } else { 0.45 };
        let shade = if !enabled { 0.0 } else if pressed { 0.2 } else if hover { 0.1 } else { 0.0 };
        match style {
            BtnStyle::Primary | BtnStyle::Destructive => {
                let base = if style == BtnStyle::Primary { th.primary } else { th.destructive };
                self.fill(r, 7.0, Rgba::new(base.r * (1.0 - shade), base.g * (1.0 - shade), base.b * (1.0 - shade), 1.0), dim);
                self.centered(label, r, 600, WHITE, dim);
            }
            BtnStyle::Outline => {
                if hover || pressed {
                    self.wash(r, 7.0, pressed);
                }
                self.stroke(r, 7.0, 1.0, th.fg, if hover { 0.34 } else { 0.24 } * dim);
                self.centered(label, r, 600, th.fg, dim);
            }
            BtnStyle::Link => {
                let c = if pressed { th.primary.lerp(Rgba::new(0.0, 0.0, 0.0, 1.0), 0.25) } else if hover { th.primary.lerp(th.fg, 0.2) } else { th.primary };
                let tw = self.width(label, 13.0, 600);
                self.text(label, 13.0, 600, r.l, r.cy(), c, dim);
                if hover {
                    self.hline(r.l, r.l + tw, r.cy() + 8.5, c, 0.8);
                }
            }
        }
        if focus && enabled {
            self.focus_ring(r, 7.0);
        }
    }

    fn centered(&mut self, label: &str, r: Rect, weight: i32, color: Rgba, alpha: f32) {
        let text = self.fit(label, 13.0, weight, r.w() - 12.0);
        let tw = self.width(&text, 13.0, weight);
        self.text(&text, 13.0, weight, r.cx() - tw * 0.5, r.cy(), color, alpha);
    }

    /// Input box. `state`: 0 idle, 1 hover, 2 focused; `error` draws the error outline.
    pub fn input_box(&mut self, r: Rect, hover: bool, focused: bool, error: bool) {
        let th = self.th;
        self.fill(r, 7.0, th.popover, 1.0);
        if th.hover_alpha > 0.07 {
            self.fill(r, 7.0, WHITE, 0.035);
        }
        if error {
            self.stroke(r, 7.0, 1.25, th.destructive, 1.0);
        } else if focused {
            self.stroke(r, 7.0, 1.75, th.primary, 1.0);
        } else {
            self.stroke(r, 7.0, 1.0, th.fg, if hover { 0.36 } else { 0.24 });
        }
    }

    /// Small tinted chip; returns its width. `x` is the left edge, `cy` the centre line.
    pub fn chip(&mut self, x: f32, cy: f32, label: &str, tone: Tone) -> f32 {
        let c = tone_color(&self.th, tone);
        let w = self.width(label, 10.5, 600) + 12.0;
        self.fill(Rect::new(x, cy - 8.0, x + w, cy + 8.0), 8.0, c, 0.16);
        self.text(label, 10.5, 600, x + 6.0, cy, c, 1.0);
        w
    }

    /// Caret bar.
    pub fn caret(&mut self, x: f32, top: f32, h: f32) {
        let c = self.th.fg;
        self.fill(Rect::new(x - 0.75, top, x + 0.75, top + h), 0.0, c, 1.0);
    }

    /// Scrollbar thumb in canvas space.
    pub fn thumb(&mut self, r: Rect, hot: bool) {
        let c = self.th.fg;
        self.fill(r, r.w() * 0.5, c, if hot { 0.5 } else { 0.28 });
    }

    /// Key cap (hotkey display): returns its width.
    pub fn keycap(&mut self, x: f32, cy: f32, label: &str, active: bool) -> f32 {
        let th = self.th;
        let w = self.width(label, 11.5, 600) + 14.0;
        let r = Rect::new(x, cy - 11.0, x + w, cy + 11.0);
        self.fill(r, 6.0, th.overlay, th.hover_alpha);
        self.stroke(r, 6.0, 1.0, if active { th.primary } else { th.fg }, if active { 0.9 } else { 0.28 });
        self.text(label, 11.5, 600, x + 7.0, cy, th.fg, 1.0);
        w
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::tray::page::metrics::FixedMetrics;
    use crate::daemon::tray::popup_draw::theme;
    use tiny_skia::Pixmap;

    fn paint<'a>(pm: &'a mut Pixmap, dark: bool) -> Painter<'a, 'static> {
        Painter::new(Canvas::new(pm, 1.0), theme(dark), false, &FixedMetrics)
    }

    fn px(pm: &Pixmap, x: u32, y: u32) -> [u8; 4] {
        let p = pm.pixel(x, y).unwrap();
        [p.red(), p.green(), p.blue(), p.alpha()]
    }

    fn lit(pm: &Pixmap) -> usize {
        pm.pixels().iter().filter(|p| p.alpha() > 60).count()
    }

    #[test]
    fn every_glyph_draws_inside_its_box() {
        for g in [Glyph::ChevronRight, Glyph::ChevronLeft, Glyph::Close] {
            let mut pm = Pixmap::new(24, 24).unwrap();
            paint(&mut pm, false).glyph(g, 4.0, 12.0, WHITE, 1.0);
            assert!(lit(&pm) > 14, "{g:?} draws ({} px)", lit(&pm));
            for y in 0..24u32 {
                for x in 0..24u32 {
                    if !(3..21).contains(&x) || !(3..21).contains(&y) {
                        assert_eq!(pm.pixel(x, y).unwrap().alpha(), 0, "{g:?} leaks at {x},{y}");
                    }
                }
            }
        }
        // Chevrons point opposite ways.
        assert!(glyph_sd(Glyph::ChevronRight, 10.4, 8.0) < 0.0 && glyph_sd(Glyph::ChevronRight, 5.0, 8.0) > 0.5);
        assert!(glyph_sd(Glyph::ChevronLeft, 5.6, 8.0) < 0.0 && glyph_sd(Glyph::ChevronLeft, 11.0, 8.0) > 0.5);
    }

    #[test]
    fn switch_is_blue_when_on_grey_when_off_and_the_knob_moves() {
        let r = Rect::from_xywh(2.0, 2.0, 36.0, 20.0);
        for dark in [false, true] {
            let th = theme(dark);
            let mut on = Pixmap::new(40, 24).unwrap();
            paint(&mut on, dark).switch(r, true, false, false);
            let mut off = Pixmap::new(40, 24).unwrap();
            paint(&mut off, dark).switch(r, false, false, false);
            let near = |p: [u8; 4], c: Rgba| (p[0] as f32 - c.r * 255.0).abs() < 3.0 && (p[1] as f32 - c.g * 255.0).abs() < 3.0 && (p[2] as f32 - c.b * 255.0).abs() < 3.0;
            // Track colour at the left end (on: primary; knob is on the right).
            assert!(near(px(&on, 6, 12), th.primary), "dark={dark}: on track {:?}", px(&on, 6, 12));
            assert!(!near(px(&off, 6, 12), th.primary));
            // The knob is white at the right when on, at the left when off.
            let white = |p: [u8; 4]| p[0] > 240 && p[1] > 240 && p[2] > 240 && p[3] > 240;
            assert!(white(px(&on, 29, 12)), "dark={dark}: knob right {:?}", px(&on, 29, 12));
            assert!(white(px(&off, 10, 12)), "dark={dark}: knob left {:?}", px(&off, 10, 12));
            assert!(!white(px(&off, 29, 12)));
        }
    }

    #[test]
    fn segmented_marks_the_selection() {
        let ctl = Rect::from_xywh(2.0, 2.0, 150.0, 28.0);
        let labels: Vec<String> = ["a", "b", "c"].iter().map(|s| s.to_string()).collect();
        let mut a = Pixmap::new(160, 34).unwrap();
        paint(&mut a, false).segmented(ctl, &labels, 0, None, None);
        let mut b = Pixmap::new(160, 34).unwrap();
        paint(&mut b, false).segmented(ctl, &labels, 2, None, None);
        // Light theme: the selected segment is the white raised pill, the others the tinted track.
        assert_eq!(px(&a, 25, 16)[..3], [255, 255, 255]);
        assert_ne!(px(&a, 125, 16)[..3], [255, 255, 255]);
        assert_eq!(px(&b, 125, 16)[..3], [255, 255, 255]);
        assert_ne!(px(&b, 25, 16)[..3], [255, 255, 255]);
        assert_ne!(a.data(), b.data());
    }

    #[test]
    fn button_styles_and_states_differ() {
        let r = Rect::from_xywh(2.0, 2.0, 100.0, 32.0);
        let render = |style, enabled, hover, pressed, focus| {
            let mut pm = Pixmap::new(110, 40).unwrap();
            paint(&mut pm, false).button(r, "Go", style, enabled, hover, pressed, focus);
            pm
        };
        let base = render(BtnStyle::Primary, true, false, false, false);
        let th = theme(false);
        let p = px(&base, 12, 18);
        assert!((p[0] as f32 - th.primary.r * 255.0).abs() < 3.0 && p[3] == 255, "primary fill {p:?}");
        assert_ne!(base.data(), render(BtnStyle::Primary, true, true, false, false).data(), "hover");
        assert_ne!(render(BtnStyle::Primary, true, true, false, false).data(), render(BtnStyle::Primary, true, true, true, false).data(), "pressed");
        assert_ne!(base.data(), render(BtnStyle::Primary, true, false, false, true).data(), "focus ring");
        assert_ne!(base.data(), render(BtnStyle::Primary, false, false, false, false).data(), "disabled is dimmed");
        let d = render(BtnStyle::Destructive, true, false, false, false);
        let pd = px(&d, 12, 18);
        assert!((pd[0] as f32 - th.destructive.r * 255.0).abs() < 3.0 && pd[1] < 80, "destructive fill {pd:?}");
        // Outline is transparent inside; its border is drawn.
        let o = render(BtnStyle::Outline, true, false, false, false);
        assert_eq!(px(&o, 12, 18)[3], 0);
        assert!(px(&o, 30, 2)[3] > 20, "border");
        // A focus ring is not drawn on a disabled button.
        assert_eq!(render(BtnStyle::Outline, false, false, false, true).data(), render(BtnStyle::Outline, false, false, false, false).data());
    }

    #[test]
    fn input_box_states() {
        let r = Rect::from_xywh(2.0, 2.0, 120.0, 34.0);
        let render = |hover, focused, error, dark| {
            let mut pm = Pixmap::new(128, 40).unwrap();
            paint(&mut pm, dark).input_box(r, hover, focused, error);
            pm
        };
        for dark in [false, true] {
            let th = theme(dark);
            let idle = render(false, false, false, dark);
            // The surface is the popover colour.
            let p = px(&idle, 60, 20);
            assert!((p[0] as f32 - th.popover.r * 255.0).abs() < 12.0 && p[3] == 255, "dark={dark} {p:?}");
            let near = |p: [u8; 4], c: Rgba| (p[0] as f32 - c.r * 255.0).abs() < 4.0 && (p[1] as f32 - c.g * 255.0).abs() < 4.0 && (p[2] as f32 - c.b * 255.0).abs() < 4.0;
            // Focus draws the primary ring; an error the destructive one.
            assert!(near(px(&render(false, true, false, dark), 40, 2), th.primary), "dark={dark}: focus ring {:?}", px(&render(false, true, false, dark), 40, 2));
            assert!(near(px(&render(false, false, true, dark), 40, 2), th.destructive) || near(px(&render(false, false, true, dark), 40, 3), th.destructive));
            assert_ne!(idle.data(), render(true, false, false, dark).data(), "hover");
        }
    }

    #[test]
    fn radio_dot_fills_when_on() {
        let mut on = Pixmap::new(24, 24).unwrap();
        paint(&mut on, false).radio_dot(12.0, 12.0, true);
        let mut off = Pixmap::new(24, 24).unwrap();
        paint(&mut off, false).radio_dot(12.0, 12.0, false);
        assert!(px(&on, 12, 12)[3] > 200, "inner dot");
        assert_eq!(px(&off, 12, 12)[3], 0, "an unselected dot is hollow");
        assert!(px(&off, 12, 4)[3] > 20, "its ring is there");
    }

    #[test]
    fn chip_returns_its_width_and_tints_by_tone() {
        let mut pm = Pixmap::new(80, 24).unwrap();
        let mut p = paint(&mut pm, false);
        let w = p.chip(4.0, 12.0, "Failed", Tone::Err);
        assert_eq!(w, 6.0 * 10.5 * FixedMetrics::EM + 12.0);
        let th = theme(false);
        let q = px(&pm, 8, 12);
        // Red tint: red channel clearly above blue.
        assert!(q[3] > 10 && q[0] > q[2], "{q:?} {:?}", th.destructive);
        assert_ne!(tone_color(&th, Tone::Warn), tone_color(&th, Tone::Info));
        assert_ne!(tone_color(&th, Tone::Ok), tone_color(&th, Tone::Err));
    }

    #[test]
    fn fit_text_cuts_with_an_ellipsis_only_when_needed() {
        let w = |s: &str| s.chars().count() as f32 * 6.0;
        assert_eq!(fit_text("short", 100.0, &w), "short");
        let cut = fit_text("a very long sentence indeed", 60.0, &w);
        assert!(cut.ends_with('\u{2026}') && w(&cut) <= 60.0, "{cut}");
        assert_eq!(fit_text("", 10.0, &w), "");
        assert_eq!(fit_text("abcdef", 0.0, &w), "\u{2026}", "nothing fits: just the ellipsis");
        // Trailing spaces never precede the ellipsis.
        assert!(!fit_text("word      word", 40.0, &w).contains(" \u{2026}"));
    }

    #[test]
    fn text_is_clipped_to_the_clip_rect() {
        // Real GDI text: skipped when there is no font (nothing to assert then).
        let mut pm = Pixmap::new(200, 30).unwrap();
        let m = crate::daemon::tray::page::metrics::GdiMetrics { scale: 1.0 };
        let mut p = Painter::new(Canvas::new(&mut pm, 1.0), theme(false), true, &m);
        p.clip = Some(Rect::new(0.0, 0.0, 40.0, 30.0));
        p.text("Clipping a long line of text", 13.0, 400, 2.0, 15.0, WHITE, 1.0);
        let inside = (0..40u32).flat_map(|x| (0..30u32).map(move |y| (x, y))).filter(|&(x, y)| pm.pixel(x, y).unwrap().alpha() > 0).count();
        let outside = (40..200u32).flat_map(|x| (0..30u32).map(move |y| (x, y))).filter(|&(x, y)| pm.pixel(x, y).unwrap().alpha() > 0).count();
        assert!(inside > 10, "text drawn inside the clip ({inside})");
        assert_eq!(outside, 0, "nothing outside the clip");
    }

    #[test]
    fn thumb_and_caret_draw() {
        let mut pm = Pixmap::new(20, 60).unwrap();
        let mut p = paint(&mut pm, false);
        p.thumb(Rect::new(6.0, 4.0, 12.0, 40.0), false);
        p.caret(16.0, 10.0, 14.0);
        assert!(px(&pm, 9, 20)[3] > 40);
        assert!(px(&pm, 16, 16)[3] > 120);
        assert_eq!(px(&pm, 3, 20)[3], 0);
    }
}
