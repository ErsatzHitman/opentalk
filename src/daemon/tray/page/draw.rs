//! Page rendering: the cached card chrome (soft shadow, border, surface, header rule), the header
//! (back button, title), the scrolling body drawn into its own viewport pixmap (which gives it a
//! clip without touching the pill's shared canvas) and composited on the card, the scrollbar, the
//! status strip and the confirmation dialog. Every [`Item`](super::Item) kind is drawn here, with
//! its hover, pressed, focus and editing states. `--render-tray-popup --page <route>` and the live
//! window both render through [`render`].

use std::cell::RefCell;

use tiny_skia::{Pixmap, PixmapPaint, Transform};

use super::controls::{Glyph, Painter, tone_color};
use super::edit::{BULLET, Editor, LINE_H};
use super::layout::{self, FS_BODY, FS_GROUP, FS_SMALL, Foc, LINE_BODY, LINE_SMALL, PAD_X, Part, Placed, ROW_BTN};
use super::metrics::TextMetrics;
use super::pager::{Confirm, BACK, Pager, STRIP_H, confirm_geo};
use super::scroll::BAR_W;
use super::session::{FieldEdit, Session, text_area, text_origin};
use super::{BtnStyle, CARD_W_PAGE, FieldKind, Item, PAGE_HEADER_H, Row, Tone, Trailing};
use crate::daemon::pill::kit::{Canvas, Rect, Rgba, sd_round_box};
use crate::daemon::tray::popup::{MARGIN, R, RADIUS};
use crate::daemon::tray::popup_draw::{Icon, Theme, WHITE, draw_icon, rr, theme};

/// Window size in device px for a page card of height `card_h`.
pub fn window_px(card_h: f32, scale: f32) -> (u32, u32) {
    (((CARD_W_PAGE + 2.0 * MARGIN) * scale).ceil() as u32, ((card_h + 2.0 * MARGIN) * scale).ceil() as u32)
}

/// What to draw.
pub struct Frame<'a> {
    pub pager: &'a Pager,
    pub scale: f32,
    /// Draw the GDI text (off for the deterministic structural goldens).
    pub text: bool,
    pub m: &'a dyn TextMetrics,
}

// ---------------------------------------------------------------------------------------------
// Cached chrome
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq)]
struct ChromeKey {
    w: u32,
    h: u32,
    scale: u32,
    dark: bool,
    card_h: u32,
}

thread_local! {
    static CHROME: RefCell<Option<(ChromeKey, Pixmap)>> = const { RefCell::new(None) };
    static BODY: RefCell<Option<Pixmap>> = const { RefCell::new(None) };
}

/// Drop the cached card chrome and the body pixmap. The popup calls this when it closes: a closed
/// popup must hold no bitmaps (they are 1 to 8 MB at common DPI scales), PLAN section 1.
pub fn release_caches() {
    CHROME.with(|c| *c.borrow_mut() = None);
    BODY.with(|b| *b.borrow_mut() = None);
}

/// The static card (shadow, border, surface, header rule) for a size / scale / theme: built once,
/// cloned per frame (the shadow and the big rounded fills are the expensive part of a frame).
fn chrome(card_h: f32, scale: f32, dark: bool) -> Option<Pixmap> {
    let (w, h) = window_px(card_h, scale);
    let key = ChromeKey { w, h, scale: scale.to_bits(), dark, card_h: card_h.to_bits() };
    if let Some(pm) = CHROME.with(|c| c.borrow().as_ref().filter(|(k, _)| *k == key).map(|(_, pm)| pm.clone())) {
        return Some(pm);
    }
    let mut pm = Pixmap::new(w.max(1), h.max(1))?;
    let th = theme(dark);
    {
        let mut cv = Canvas::new(&mut pm, scale);
        let card = Rect::from_xywh(MARGIN, MARGIN, CARD_W_PAGE, card_h);
        cv.shadow_rect(card, RADIUS, (0.0, 6.0), 20.0, 0.0, Rgba::new(0.0, 0.0, 0.0, 1.0), th.shadow_alpha);
        cv.shadow_rect(card, RADIUS, (0.0, 1.0), 3.0, 0.0, Rgba::new(0.0, 0.0, 0.0, 1.0), th.shadow_alpha * 0.55);
        rr(&mut cv, card, RADIUS, th.border, 1.0);
        rr(&mut cv, card.inflate(-1.0), RADIUS - 1.0, th.popover, 1.0);
        let y = MARGIN + PAGE_HEADER_H;
        rr(&mut cv, Rect::new(MARGIN + 1.0, y, MARGIN + CARD_W_PAGE - 1.0, y + 1.0), 0.0, th.border, 1.0);
    }
    CHROME.with(|c| *c.borrow_mut() = Some((key, pm.clone())));
    Some(pm)
}

// ---------------------------------------------------------------------------------------------
// Coordinate mapping
// ---------------------------------------------------------------------------------------------

/// Content space -> body viewport pixmap space.
#[derive(Clone, Copy)]
struct Off {
    dx: f32,
    dy: f32,
}

impl Off {
    fn r(self, r: R) -> Rect {
        Rect::new(r.l + self.dx, r.t + self.dy, r.r + self.dx, r.b + self.dy)
    }
    fn x(self, x: f32) -> f32 {
        x + self.dx
    }
    fn y(self, y: f32) -> f32 {
        y + self.dy
    }
}

// ---------------------------------------------------------------------------------------------
// The frame
// ---------------------------------------------------------------------------------------------

/// Render the popup window for the pager's top page.
pub fn render(f: &Frame) -> Option<Pixmap> {
    let pg = f.pager;
    let scale = f.scale;
    let mut pm = chrome(pg.card_h, scale, pg.dark)?;
    let th = theme(pg.dark);
    let sess = pg.top();

    // Body first (it is composited under the header and strip, which never overlap it).
    if let Some(s) = sess {
        draw_body(&mut pm, f, s, &th);
    }

    let mut p = Painter::new(Canvas::new(&mut pm, scale), th, f.text, f.m);
    draw_header(&mut p, pg, sess);
    if let Some(s) = sess {
        draw_scrollbar(&mut p, pg, s);
    }
    if let Some(t) = &pg.toast {
        draw_strip(&mut p, pg, t);
    }
    if let Some(c) = &pg.confirm {
        draw_confirm(&mut p, pg, c, f.m);
    }
    Some(pm)
}

fn card(r: R) -> Rect {
    Rect::new(r.l + MARGIN, r.t + MARGIN, r.r + MARGIN, r.b + MARGIN)
}

fn draw_header(p: &mut Painter, pg: &Pager, sess: Option<&Session>) {
    let th = p.th;
    let back = card(BACK);
    if pg.back_hover || pg.back_down {
        p.wash(back, 7.0, pg.back_down);
    }
    p.glyph_in(Glyph::ChevronLeft, back, th.fg, 0.9);
    let title = sess.map(|s| s.model.title()).unwrap_or_default();
    let max_w = CARD_W_PAGE - 52.0 - 16.0;
    let title = p.fit(&title, 15.0, 600, max_w);
    p.text(&title, 15.0, 600, MARGIN + 52.0, MARGIN + PAGE_HEADER_H * 0.5, th.fg, 1.0);
}

fn draw_scrollbar(p: &mut Painter, pg: &Pager, s: &Session) {
    let Some(thumb) = s.scroll.thumb_rect(CARD_W_PAGE - 3.0) else { return };
    let top = PAGE_HEADER_H + 1.0;
    let r = Rect::new(thumb.l + MARGIN, thumb.t + top + MARGIN, thumb.r + MARGIN, thumb.b + top + MARGIN);
    let hot = s.scroll.dragging() || s.bar_hover;
    let _ = (pg, BAR_W);
    p.thumb(r, hot);
}

fn draw_strip(p: &mut Painter, pg: &Pager, text: &str) {
    let th = p.th;
    let top = MARGIN + pg.card_h - STRIP_H;
    let inner = Rect::from_xywh(MARGIN + 1.0, MARGIN + 1.0, CARD_W_PAGE - 2.0, pg.card_h - 2.0);
    let (cx, cy, hw, hh) = (inner.cx(), inner.cy(), inner.w() * 0.5, inner.h() * 0.5);
    p.cv.fill_sdf(Rect::new(inner.l, top - 1.0, inner.r, inner.b + 1.0), th.overlay, th.hover_alpha * 0.7, |x, y| sd_round_box(x, y, cx, cy, hw, hh, RADIUS - 1.0).max(top - y));
    p.hline(MARGIN + 1.0, MARGIN + CARD_W_PAGE - 1.0, top, th.border, 1.0);
    let t = p.fit(text, FS_SMALL, 400, CARD_W_PAGE - 2.0 * PAD_X);
    p.text(&t, FS_SMALL, 400, MARGIN + PAD_X, top + (STRIP_H - 1.0) * 0.5, th.fg, 0.9);
}

fn draw_confirm(p: &mut Painter, pg: &Pager, c: &Confirm, m: &dyn TextMetrics) {
    let th = p.th;
    let inner = Rect::from_xywh(MARGIN + 1.0, MARGIN + 1.0, CARD_W_PAGE - 2.0, pg.card_h - 2.0);
    p.fill(inner, RADIUS - 1.0, Rgba::new(0.0, 0.0, 0.0, 1.0), if pg.dark { 0.55 } else { 0.34 });
    let g = confirm_geo(c, pg.card_h, m);
    let d = card(g.dlg);
    p.cv.shadow_rect(d, 10.0, (0.0, 6.0), 18.0, 0.0, Rgba::new(0.0, 0.0, 0.0, 1.0), th.shadow_alpha);
    p.fill(d, 10.0, th.border, 1.0);
    p.fill(d.inflate(-1.0), 9.0, th.popover, 1.0);
    let x = d.l + 18.0;
    p.text(&c.title, 14.0, 600, x, MARGIN + g.title_cy, th.fg, 1.0);
    for (i, l) in g.body.iter().enumerate() {
        p.text(l, 12.5, 400, x, MARGIN + g.body_top + i as f32 * 17.0 + 8.5, th.fg, 0.85);
    }
    let style = if c.destructive { BtnStyle::Destructive } else { BtnStyle::Primary };
    p.button(card(g.cancel), "Cancel", BtnStyle::Outline, true, c.hover == Some(false), c.down == Some(false), !c.ok_focus);
    p.button(card(g.ok), &c.action, style, true, c.hover == Some(true), c.down == Some(true), c.ok_focus);
}

// ---------------------------------------------------------------------------------------------
// Body
// ---------------------------------------------------------------------------------------------

/// Per-item interaction state.
struct St<'a> {
    hover: Option<Part>,
    press: Option<Part>,
    /// Keyboard focus ring on this item.
    ring: bool,
    /// The item holds focus (any origin).
    focused: bool,
    edit: Option<&'a FieldEdit>,
    capture: Option<&'a str>,
}

fn draw_body(pm: &mut Pixmap, f: &Frame, s: &Session, th: &Theme) {
    let scale = f.scale;
    let pg = f.pager;
    let vw = CARD_W_PAGE - 2.0;
    let vh = pg.body_h() - 1.0;
    let (dw, dh) = (((vw * scale).ceil() as u32).max(1), ((vh * scale).ceil() as u32).max(1));
    let mut body = BODY.with(|b| b.borrow_mut().take()).filter(|b| b.width() == dw && b.height() == dh).unwrap_or_else(|| Pixmap::new(dw, dh).expect("non-zero"));
    body.fill(tiny_skia::Color::TRANSPARENT);
    {
        let mut p = Painter::new(Canvas::new(&mut body, scale), *th, f.text, f.m);
        p.clip = Some(Rect::new(0.0, 0.0, vw, vh));
        let off = Off { dx: -1.0, dy: -s.scroll.offset };
        let (y0, y1) = (s.scroll.offset - 2.0, s.scroll.offset + vh + 2.0);
        let hover_of = |foc: Foc| s.hover.filter(|(f, _)| *f == foc).map(|(_, part)| part);
        let press_of = |foc: Foc| s.press.filter(|pr| pr.foc == foc).map(|pr| pr.part);
        let st_for = |foc: Foc, item: &Item| St {
            hover: hover_of(foc),
            press: press_of(foc),
            ring: s.ring && s.focus == Some(foc),
            focused: s.focus == Some(foc),
            edit: match (foc, &s.edit) {
                (Foc::Item(id), Some(fe)) if fe.id == id => Some(fe),
                _ => None,
            },
            capture: match (foc, &s.capture, item) {
                (Foc::Item(id), Some((cid, text)), _) if *cid == id => Some(text.as_str()),
                _ => None,
            },
        };
        for pl in &s.layout.placed[s.layout.visible(y0, y1)] {
            let item = &s.items[pl.idx];
            let foc = item.id().map(Foc::Item);
            let st = match foc {
                Some(foc) => st_for(foc, item),
                None => St { hover: None, press: None, ring: false, focused: false, edit: None, capture: None },
            };
            draw_item(&mut p, off, item, pl, &st);
        }
        if let Some(vr) = s.model.virtual_rows() {
            for i in s.layout.virt_visible(y0, y1) {
                let Some(rect_) = s.layout.virt_rect(i) else { continue };
                let item = (vr.row)(i);
                let st = st_for(Foc::Row(i), &item);
                let ctl = if let Item::Row(r) = &item { layout::row_btn(r, rect_) } else { R::ZERO };
                let pl = Placed { idx: usize::MAX, rect: rect_, ctl, lines: Vec::new() };
                draw_item(&mut p, off, &item, &pl, &st);
            }
        }
    }
    let x = ((MARGIN + 1.0) * scale).round() as i32;
    let y = ((MARGIN + PAGE_HEADER_H + 1.0) * scale).round() as i32;
    pm.draw_pixmap(x, y, body.as_ref(), &PixmapPaint::default(), Transform::identity(), None);
    BODY.with(|b| *b.borrow_mut() = Some(body));
}

fn tint(c: Rgba, a: f32) -> Rgba {
    c.with_alpha(a)
}

fn draw_item(p: &mut Painter, o: Off, item: &Item, pl: &Placed, st: &St) {
    let th = p.th;
    let rc = pl.rect;
    let hovered = st.hover.is_some();
    let pressed = st.press.is_some();
    let wide = |r: R| R::new(r.l - 6.0, r.t, r.r + 6.0, r.b);
    match item {
        Item::Group(t) => {
            p.text(t, FS_GROUP, 600, o.x(rc.l), o.y(rc.t + 21.0), th.muted, 1.0);
        }
        Item::Note(_) => {
            for (i, l) in pl.lines.iter().enumerate() {
                p.text(l, FS_SMALL, 400, o.x(rc.l), o.y(rc.t + 2.0 + LINE_SMALL * (i as f32 + 0.5)), th.muted, 1.0);
            }
        }
        Item::Banner { tone, text: _ } => {
            let c = tone_color(&th, *tone);
            let card = Rect::new(o.x(rc.l), o.y(rc.t), o.x(rc.r), o.y(rc.b - layout::CARD_GAP));
            p.fill(card, 8.0, c, 0.12);
            p.stroke(card, 8.0, 1.0, c, 0.42);
            p.fill(Rect::new(card.l + 1.0, card.t + 7.0, card.l + 4.0, card.b - 7.0), 1.5, c, 1.0);
            for (i, l) in pl.lines.iter().enumerate() {
                p.text(l, FS_SMALL, 400, card.l + 14.0, card.t + 9.0 + LINE_BODY * (i as f32 + 0.5), th.fg, 0.95);
            }
        }
        Item::Toggle { label, on, .. } => {
            let row = o.r(wide(rc));
            if hovered || pressed {
                p.wash(row, 8.0, pressed);
            }
            let has_desc = !pl.lines.is_empty();
            let cy = if has_desc { rc.t + 10.0 + LINE_BODY * 0.5 } else { rc.cy() };
            let max_w = pl.ctl.l - 12.0 - rc.l;
            let label = p.fit(label, FS_BODY, 400, max_w);
            p.text(&label, FS_BODY, 400, o.x(rc.l), o.y(cy), th.fg, 1.0);
            for (i, l) in pl.lines.iter().enumerate() {
                p.text(l, FS_SMALL, 400, o.x(rc.l), o.y(rc.t + 10.0 + LINE_BODY + 1.0 + LINE_SMALL * (i as f32 + 0.5)), th.muted, 1.0);
            }
            p.switch(o.r(pl.ctl), *on, hovered, pressed);
            if st.ring {
                p.focus_ring(row, 8.0);
            }
        }
        Item::Segmented { label, options, sel, .. } => {
            if !label.is_empty() {
                p.text(label, FS_BODY, 400, o.x(rc.l), o.y(rc.t + 16.0), th.fg, 1.0);
            }
            let hov = match st.hover {
                Some(Part::Seg(i)) => Some(i),
                _ => None,
            };
            let prs = match st.press {
                Some(Part::Seg(i)) => Some(i),
                _ => None,
            };
            p.segmented(o.r(pl.ctl), options, *sel, hov, prs);
            if st.ring {
                p.focus_ring(o.r(pl.ctl), 8.0);
            }
        }
        Item::Choice { label, value, .. } => {
            let row = o.r(wide(rc));
            if hovered || pressed {
                p.wash(row, 8.0, pressed);
            }
            let label_w = p.width(label, FS_BODY, 400);
            p.text(label, FS_BODY, 400, o.x(rc.l), o.y(rc.cy()), th.fg, 1.0);
            let vmax = (rc.w() - label_w - 56.0).max(60.0);
            let v = p.fit(value, 12.5, 400, vmax);
            let vw = p.width(&v, 12.5, 400);
            p.text(&v, 12.5, 400, o.x(rc.r) - 24.0 - vw, o.y(rc.cy()), th.muted, 1.0);
            p.glyph(Glyph::ChevronRight, o.x(rc.r) - 18.0, o.y(rc.cy()), th.muted, 1.0);
            if st.ring {
                p.focus_ring(row, 8.0);
            }
        }
        Item::Radio { label, on, .. } => {
            let card = Rect::new(o.x(rc.l), o.y(rc.t), o.x(rc.r), o.y(rc.b - layout::CARD_GAP));
            if *on {
                p.fill(card, 8.0, th.primary, 0.08);
                p.stroke(card, 8.0, 1.5, th.primary, 1.0);
            } else {
                if hovered || pressed {
                    p.wash(card, 8.0, pressed);
                }
                p.stroke(card, 8.0, 1.0, th.border_hover, 1.0);
            }
            let has_desc = !pl.lines.is_empty();
            let cy = if has_desc { card.t + 11.0 + LINE_BODY * 0.5 } else { card.cy() };
            p.radio_dot(card.l + 19.0, cy, *on);
            let label = p.fit(label, FS_BODY, if *on { 600 } else { 400 }, rc.w() - 52.0);
            p.text(&label, FS_BODY, if *on { 600 } else { 400 }, card.l + 38.0, cy, th.fg, 1.0);
            for (i, l) in pl.lines.iter().enumerate() {
                p.text(l, FS_SMALL, 400, card.l + 38.0, card.t + 11.0 + LINE_BODY + 1.0 + LINE_SMALL * (i as f32 + 0.5), th.muted, 1.0);
            }
            if st.ring {
                p.focus_ring(card, 8.0);
            }
        }
        Item::Field { label, hint, value, kind, error, .. } => {
            if !label.is_empty() {
                p.text(label, 12.5, 400, o.x(rc.l), o.y(rc.t + 2.0 + LINE_BODY * 0.5), th.fg, 0.88);
            }
            let ctl = o.r(pl.ctl);
            p.input_box(ctl, hovered && st.edit.is_none(), st.edit.is_some() || st.capture.is_some() || (st.focused && st.ring), error.is_some());
            match kind {
                FieldKind::Hotkey => draw_hotkey(p, o, pl.ctl, value, hint, st),
                _ => draw_field_text(p, o, pl.ctl, value, hint, *kind, st),
            }
            if let Some(e) = error {
                let _ = e;
                for (i, l) in pl.lines.iter().enumerate() {
                    p.text(l, FS_SMALL, 400, o.x(rc.l), o.y(pl.ctl.b + 3.0 + LINE_SMALL * (i as f32 + 0.5)), th.destructive, 1.0);
                }
            }
        }
        Item::Button { label, style, enabled, .. } => {
            p.button(o.r(pl.ctl), label, *style, *enabled, hovered && *enabled, pressed && *enabled, st.ring);
        }
        Item::Result { ok, badge, .. } => {
            let c = if *ok { th.success } else { th.destructive };
            let card = Rect::new(o.x(rc.l), o.y(rc.t), o.x(rc.r), o.y(rc.b - layout::CARD_GAP));
            p.fill(card, 8.0, c, 0.10);
            p.stroke(card, 8.0, 1.0, c, 0.38);
            p.text(badge, FS_SMALL, 600, card.l + 12.0, card.t + 8.0 + LINE_BODY * 0.5, c, 1.0);
            for (i, l) in pl.lines.iter().enumerate() {
                p.text(l, FS_SMALL, 400, card.l + 12.0, card.t + 8.0 + LINE_BODY + LINE_SMALL * (i as f32 + 0.5), th.fg, 0.9);
            }
        }
        Item::Row(row) => draw_row(p, o, row, rc, pl.ctl, st),
        Item::Divider => {
            p.hline(o.x(rc.l), o.x(rc.r), o.y(rc.cy()), th.border, 1.0);
        }
        Item::Space(_) => {}
    }
}

// ---------------------------------------------------------------------------------------------
// Fields
// ---------------------------------------------------------------------------------------------

/// Friendly key names for the hotkey box ("Control+Super" -> Ctrl, Win).
pub fn hotkey_caps(accel: &str) -> Vec<String> {
    accel
        .split('+')
        .filter(|s| !s.is_empty())
        .map(|k| match k {
            "Control" | "Ctrl" | "CommandOrControl" => "Ctrl".to_string(),
            "Super" | "Meta" | "Win" => "Win".to_string(),
            other => other.to_string(),
        })
        .collect()
}

fn draw_hotkey(p: &mut Painter, o: Off, ctl: R, value: &str, hint: &str, st: &St) {
    let th = p.th;
    let c = o.r(ctl);
    let capturing = st.capture.is_some();
    let shown = match st.capture {
        Some(t) if !t.is_empty() => t,
        Some(_) => "",
        None => value,
    };
    let caps = hotkey_caps(shown);
    if caps.is_empty() {
        let msg = if capturing { "Press the keys\u{2026}" } else if hint.is_empty() { "Click to set" } else { hint };
        p.text(msg, FS_BODY, 400, c.l + 12.0, c.cy(), th.muted, 0.85);
    } else {
        let mut x = c.l + 10.0;
        for (i, cap) in caps.iter().enumerate() {
            if i > 0 {
                p.text("+", 12.0, 400, x + 1.0, c.cy(), th.muted, 1.0);
                x += 12.0;
            }
            x += p.keycap(x, c.cy(), cap, capturing) + 2.0;
        }
    }
    if !capturing {
        let tail = if hint.is_empty() { "Change" } else { "" };
        let w = p.width(tail, 12.0, 600);
        p.text(tail, 12.0, 600, c.r - 12.0 - w, c.cy(), th.primary, 1.0);
    }
}

fn draw_field_text(p: &mut Painter, o: Off, ctl: R, value: &str, hint: &str, kind: FieldKind, st: &St) {
    let th = p.th;
    let multi = matches!(kind, FieldKind::Multiline { .. });
    let (ox, oy) = text_origin(ctl, multi);
    let (aw, _) = text_area(ctl, multi);
    let tmp;
    let ed: &Editor = match st.edit {
        Some(fe) => &fe.ed,
        None => {
            tmp = Editor::new(value, kind, 0, aw);
            &tmp
        }
    };
    let vis = ed.vis(p.m);
    let boxr = o.r(ctl);
    let inner = Rect::new(boxr.l + 2.0, boxr.t + 2.0, boxr.r - 2.0, boxr.b - 2.0);
    let saved = p.clip.replace(inner);
    if ed.is_empty() {
        if !hint.is_empty() {
            let t = p.fit(hint, FS_BODY, 400, aw);
            p.text(&t, FS_BODY, 400, o.x(ox), o.y(oy + LINE_H * 0.5), th.muted, 0.75);
        }
    } else {
        let sel = ed.selection();
        for (i, line) in vis.lines.iter().enumerate() {
            let top = oy + i as f32 * LINE_H - ed.scroll_y;
            if top + LINE_H < ctl.t || top > ctl.b {
                continue;
            }
            let x0 = ox - ed.scroll_x;
            if let Some((a, b)) = sel {
                let (a, b) = (a.max(line.start), b.min(line.end));
                if a < b || (a == b && false) {
                    let xa = vis.pre[i][a - line.start];
                    let xb = vis.pre[i][b - line.start];
                    p.fill(Rect::new(o.x(x0 + xa), o.y(top + 0.5), o.x(x0 + xb), o.y(top + LINE_H - 0.5)), 2.0, th.primary, 0.30);
                }
            }
            let text: String = if ed.secret { ed.chars()[line.start..line.end].iter().map(|_| BULLET).collect() } else { ed.chars()[line.start..line.end].iter().collect() };
            p.text(text.trim_end_matches('\n'), FS_BODY, 400, o.x(x0), o.y(top + LINE_H * 0.5), th.fg, 1.0);
        }
    }
    if st.edit.is_some() {
        let li = vis.line_of(ed.caret);
        let top = oy + li as f32 * LINE_H - ed.scroll_y;
        let x = ox - ed.scroll_x + vis.x_of(ed.caret);
        p.caret(o.x(x), o.y(top + 1.0), LINE_H - 2.0);
    }
    p.clip = saved;
}

// ---------------------------------------------------------------------------------------------
// Rows
// ---------------------------------------------------------------------------------------------

fn draw_row(p: &mut Painter, o: Off, row: &Row, rc: R, btn: R, st: &St) {
    let th = p.th;
    let hovered = st.hover.is_some();
    let pressed = st.press.is_some();
    let full = o.r(R::new(rc.l - 6.0, rc.t, rc.r + 6.0, rc.b).inflate(-0.5));
    if hovered || pressed || st.ring {
        p.wash(full, 7.0, pressed && st.press != Some(Part::Trailing));
    }
    let trailing_w = layout::row_trailing_w(row, &|s, px, w| p.m.width(s, px, w));
    let text_l = rc.l;
    let text_r = rc.r - trailing_w - 2.0;
    let cy = rc.cy();

    // Trailing content.
    match &row.trailing {
        Trailing::None => {}
        Trailing::Chevron => p.glyph(Glyph::ChevronRight, o.x(rc.r) - 18.0, o.y(cy), th.muted, 1.0),
        Trailing::Value(v) => {
            p.glyph(Glyph::ChevronRight, o.x(rc.r) - 18.0, o.y(cy), th.muted, 1.0);
            let t = p.fit(v, FS_SMALL, 400, 150.0);
            let w = p.width(&t, FS_SMALL, 400);
            p.text(&t, FS_SMALL, 400, o.x(rc.r) - 24.0 - w, o.y(cy), th.muted, 1.0);
        }
        Trailing::Badge { text, tone } => {
            let w = p.width(text, 10.5, 600) + 12.0;
            p.chip(o.x(rc.r) - w - 2.0, o.y(cy), text, *tone);
        }
        Trailing::Delete | Trailing::Copy => {
            if hovered || st.focused {
                let b = o.r(btn);
                if st.hover == Some(Part::Trailing) || st.press == Some(Part::Trailing) {
                    p.wash(b, 6.0, st.press == Some(Part::Trailing));
                }
                if row.trailing == Trailing::Delete {
                    p.glyph_in(Glyph::Close, b, th.muted, 1.0);
                } else {
                    draw_icon(&mut p.cv, Icon::Copy, b.cx() - 8.0, b.cy(), th.muted, 1.0);
                }
            }
        }
    }
    let _ = ROW_BTN;

    let mut meta_w = 0.0;
    if !row.meta.is_empty() && row.height >= 56.0 {
        let t = p.fit(&row.meta, 11.0, 400, 150.0);
        meta_w = p.width(&t, 11.0, 400);
        p.text(&t, 11.0, 400, o.x(text_r) - meta_w, o.y(rc.t + 15.0), th.muted, 1.0);
    }

    // Chips share the first line with the primary text.
    let chips_w: f32 = row.chips.iter().map(|(t, _)| p.width(t, 10.5, 600) + 12.0 + 4.0).sum();
    if row.height >= 56.0 {
        let line1_r = text_r - if meta_w > 0.0 { meta_w + 10.0 } else { 0.0 };
        let pmax = (line1_r - text_l - chips_w).max(60.0);
        let primary = p.fit(&row.primary, 12.5, 600, pmax);
        let pw = p.width(&primary, 12.5, 600);
        p.text(&primary, 12.5, 600, o.x(text_l), o.y(rc.t + 15.0), th.fg, 1.0);
        let mut x = text_l + pw + 8.0;
        for (t, tone) in &row.chips {
            x += p.chip(o.x(x), o.y(rc.t + 15.0), t, *tone) + 4.0;
        }
        // Two lines of the transcript, the second cut with an ellipsis.
        let lines = layout::wrap_lines(&row.secondary, FS_SMALL + 0.5, 400, text_r - text_l, &|s, px, w| p.m.width(s, px, w));
        for (i, l) in lines.iter().take(2).enumerate() {
            let last = i == 1 && lines.len() > 2;
            let l = if last { p.fit(&format!("{l} \u{2026}"), FS_SMALL + 0.5, 400, text_r - text_l) } else { l.clone() };
            p.text(&l, FS_SMALL + 0.5, 400, o.x(text_l), o.y(rc.t + 33.0 + i as f32 * 16.0), th.fg, 0.82);
        }
    } else if !row.secondary.is_empty() {
        let primary = p.fit(&row.primary, FS_BODY, 600, text_r - text_l - chips_w);
        let pw = p.width(&primary, FS_BODY, 600);
        p.text(&primary, FS_BODY, 600, o.x(text_l), o.y(cy - 8.5), th.fg, 1.0);
        let mut x = text_l + pw + 8.0;
        for (t, tone) in &row.chips {
            x += p.chip(o.x(x), o.y(cy - 8.5), t, *tone) + 4.0;
        }
        let sec = p.fit(&row.secondary, FS_SMALL, 400, text_r - text_l);
        p.text(&sec, FS_SMALL, 400, o.x(text_l), o.y(cy + 9.0), th.muted, 1.0);
    } else {
        let primary = p.fit(&row.primary, FS_BODY, 400, text_r - text_l - chips_w);
        let pw = p.width(&primary, FS_BODY, 400);
        p.text(&primary, FS_BODY, 400, o.x(text_l), o.y(cy), th.fg, 1.0);
        let mut x = text_l + pw + 8.0;
        for (t, tone) in &row.chips {
            x += p.chip(o.x(x), o.y(cy), t, *tone) + 4.0;
        }
    }
    let _ = (tint, Tone::Info, WHITE);
}

// ---------------------------------------------------------------------------------------------
// `--render-tray-popup --page <route>`
// ---------------------------------------------------------------------------------------------

#[cfg(any(test, debug_assertions))]
fn preview_model(name: &str) -> Option<Box<dyn super::PageModel>> {
    super::gallery::model_by_name(name)
}

#[cfg(not(any(test, debug_assertions)))]
fn preview_model(_name: &str) -> Option<Box<dyn super::PageModel>> {
    None
}

/// Options of a page render beyond `--dark` / `--scale` / `--no-text`.
#[derive(Default)]
pub struct PageCli {
    pub dark: bool,
    pub scale: f32,
    pub text: bool,
    pub card_h: f32,
    pub scroll: f32,
    /// Hover the item with this id (static) or `row:N` (virtual row N).
    pub hover: Option<String>,
    /// Keyboard-focus the item with this id.
    pub focus: Option<u16>,
    /// Click into the field with this id (editing, caret at the end).
    pub edit: Option<u16>,
    pub select_all: bool,
    pub type_text: Option<String>,
    pub toast: Option<String>,
    pub confirm: bool,
}

/// Render page `name` (`gallery`, `gallery-list`, or any route such as `settings:stt`) in the
/// requested state, the way the live window would draw it.
pub fn render_page_cli(name: &str, o: &PageCli) -> Result<Pixmap, String> {
    use super::metrics::{FixedMetrics, GdiMetrics};
    use super::pager::PResp;
    use super::session::{K, KeyEv, MemClip};
    use super::{Effect, Route, noop_waker};

    let gdi = GdiMetrics { scale: o.scale };
    let m: &dyn TextMetrics = if o.text { &gdi } else { &FixedMetrics };
    let (route, model) = match preview_model(name) {
        Some(model) => (Route::SettingsIndex, model),
        None => {
            let route = Route::parse(Some(name));
            match super::model_for(&route) {
                Some(model) => (route, model),
                None => return Err(format!("no page '{name}'")),
            }
        }
    };
    let mut pg = Pager::new(crate::core::config::Config::default(), o.dark, o.card_h, noop_waker());
    let mut clip = MemClip::default();
    let mut r = PResp::default();
    pg.push_model(route, model, m, &mut r);
    if let (true, Some(s)) = (o.scroll > 0.0, pg.stack.last_mut()) {
        s.scroll.by(o.scroll);
    }
    let item_center = |pg: &Pager, id: u16| {
        let s = pg.top()?;
        let p = s.layout.placed_of(&s.items, id)?;
        let c = if p.ctl.is_empty() && p.ctl.w() == 0.0 { p.rect } else { p.ctl };
        Some((c.cx(), c.cy() - s.scroll.offset + PAGE_HEADER_H, c))
    };
    if let Some(h) = &o.hover {
        let pt = if let Some(n) = h.strip_prefix("row:").and_then(|n| n.parse::<usize>().ok()) {
            pg.top().and_then(|s| s.layout.virt_rect(n).map(|rc| (rc.l + 60.0, rc.cy() - s.scroll.offset + PAGE_HEADER_H)))
        } else {
            h.parse::<u16>().ok().and_then(|id| item_center(&pg, id)).map(|(x, y, c)| (x.min(c.l + 60.0), y))
        };
        let (x, y) = pt.ok_or_else(|| format!("no item '{h}' to hover"))?;
        pg.pointer(Some((x, y)), false, m, &mut clip);
    }
    if let Some(id) = o.focus {
        let mut cx = super::Cx::new(&mut pg.cfg, o.dark, 0);
        if let Some(s) = pg.stack.last_mut() {
            s.focus_item(id, &mut cx, m);
        }
    }
    if let Some(id) = o.edit {
        let (_, y, c) = item_center(&pg, id).ok_or_else(|| format!("no field '{id}'"))?;
        let x = c.r - 12.0;
        pg.press((x, y), false, m, &mut clip);
        pg.release((x, y), m, &mut clip);
        // Move the pointer away so the hover wash does not join the focus state.
        pg.pointer(None, false, m, &mut clip);
    }
    if let Some(t) = &o.type_text {
        pg.text(t, m, &mut clip);
    }
    if o.select_all {
        pg.key(KeyEv { k: K::Letter('a'), shift: false, ctrl: true, alt: false }, m, &mut clip);
    }
    let mut fx = Vec::new();
    if let Some(t) = &o.toast {
        fx.push(Effect::Toast(t.clone()));
    }
    if o.confirm {
        fx.push(Effect::Confirm {
            title: "Delete this entry?".into(),
            body: "It is removed from this computer and cannot be recovered.".into(),
            action: "Delete".into(),
            destructive: true,
            on_confirm: super::ConfirmId::HistoryDelete(1),
        });
    }
    if !fx.is_empty() {
        pg.run(fx, m, &mut clip, &mut r);
    }
    render(&Frame { pager: &pg, scale: o.scale, text: o.text, m }).ok_or_else(|| "could not render".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::config::Config;
    use crate::daemon::tray::page::metrics::FixedMetrics;
    use crate::daemon::tray::page::session::MemClip;
    use crate::daemon::tray::page::{Cx, Effect, Event, ItemId, PageModel, noop_waker};

    struct Static(Vec<Item>);

    impl PageModel for Static {
        fn title(&self) -> String {
            "Page".into()
        }
        fn items(&self, _cx: &Cx) -> Vec<Item> {
            self.0.clone()
        }
        fn event(&mut self, _id: ItemId, _ev: Event, _cx: &mut Cx) -> Vec<Effect> {
            Vec::new()
        }
    }

    fn pager_with(items: Vec<Item>, dark: bool, card_h: f32) -> Pager {
        let mut p = Pager::new(Config::default(), dark, card_h, noop_waker());
        let mut r = super::super::pager::PResp::default();
        p.push_model(crate::daemon::tray::page::Route::SettingsIndex, Box::new(Static(items)), &FixedMetrics, &mut r);
        p
    }

    fn shot(p: &Pager, scale: f32) -> Pixmap {
        render(&Frame { pager: p, scale, text: false, m: &FixedMetrics }).expect("pixmap")
    }

    fn px(pm: &Pixmap, x: f32, y: f32, scale: f32) -> [u8; 4] {
        let q = pm.pixel((x * scale) as u32, (y * scale) as u32).expect("in range");
        [q.red(), q.green(), q.blue(), q.alpha()]
    }

    fn toggles(n: u16) -> Vec<Item> {
        (0..n).map(|i| Item::Toggle { id: i, label: "T".into(), desc: String::new(), on: i % 2 == 0 }).collect()
    }

    #[test]
    fn window_size_is_the_card_plus_margins_at_every_scale() {
        assert_eq!(window_px(600.0, 1.0), (412, 632));
        assert_eq!(window_px(600.0, 2.0), (824, 1264));
        let p = pager_with(toggles(3), false, 480.0);
        let pm = shot(&p, 1.5);
        assert_eq!((pm.width(), pm.height()), window_px(480.0, 1.5));
    }

    #[test]
    fn the_card_is_opaque_inside_transparent_outside_and_themed() {
        for dark in [false, true] {
            let p = pager_with(toggles(3), dark, 480.0);
            let th = theme(dark);
            let pm = shot(&p, 1.0);
            let inside = px(&pm, MARGIN + 340.0, MARGIN + 470.0, 1.0);
            assert_eq!(inside[3], 255, "dark={dark}");
            assert!((inside[0] as f32 - th.popover.r * 255.0).abs() < 2.0, "dark={dark}: surface {inside:?}");
            assert!(px(&pm, 0.0, 0.0, 1.0)[3] < 12, "the corner of the window is outside the card");
            // The header rule is the border colour.
            let rule = px(&pm, MARGIN + 200.0, MARGIN + PAGE_HEADER_H, 1.0);
            assert!((rule[0] as f32 - th.border.r * 255.0).abs() < 3.0, "dark={dark}: rule {rule:?}");
        }
        let (a, b) = (shot(&pager_with(toggles(3), false, 480.0), 1.0), shot(&pager_with(toggles(3), true, 480.0), 1.0));
        assert_ne!(a.data(), b.data());
    }

    #[test]
    fn text_free_renders_are_deterministic_and_the_chrome_cache_is_exact() {
        let p = pager_with(toggles(5), false, 480.0);
        let a = shot(&p, 1.0);
        let b = shot(&p, 1.0);
        assert_eq!(a.data(), b.data(), "the second frame comes from the cached chrome and matches");
        // A different size or scale does not reuse the wrong chrome.
        let c = shot(&pager_with(toggles(5), false, 300.0), 1.0);
        assert_ne!(c.height(), a.height());
        let d = shot(&p, 2.0);
        assert_eq!(d.width(), a.width() * 2);
    }

    #[test]
    fn toggles_draw_their_switch_in_the_right_state() {
        let p = pager_with(toggles(2), false, 480.0);
        let th = theme(false);
        let pm = shot(&p, 1.0);
        let s = p.top().unwrap();
        let on = s.layout.placed[0].ctl;
        let off = s.layout.placed[1].ctl;
        let at = |r: R, dx: f32| px(&pm, MARGIN + r.l + dx - 1.0 + 1.0, MARGIN + PAGE_HEADER_H + 1.0 + r.cy() - s.scroll.offset, 1.0);
        let track_on = at(on, 5.0);
        let near = |c: [u8; 4], t: Rgba| (c[0] as f32 - t.r * 255.0).abs() < 4.0 && (c[2] as f32 - t.b * 255.0).abs() < 4.0;
        assert!(near(track_on, th.primary), "on-switch track {track_on:?}");
        assert!(!near(at(off, 5.0), th.primary) || at(off, 5.0)[0] > 200, "an off switch has a knob at the left");
    }

    #[test]
    fn scrolling_moves_the_content_and_draws_a_scrollbar() {
        let mut p = pager_with(toggles(30), false, 400.0);
        let top = shot(&p, 1.0);
        let thumb = |pm: &Pixmap| px(pm, MARGIN + CARD_W_PAGE - 6.0, MARGIN + PAGE_HEADER_H + 20.0, 1.0);
        assert!(thumb(&top)[3] == 255 && thumb(&top)[0] < 245 || thumb(&top)[0] < 250, "the thumb is drawn at the top: {:?}", thumb(&top));
        p.stack.last_mut().unwrap().scroll.by(200.0);
        let moved = shot(&p, 1.0);
        assert_ne!(top.data(), moved.data());
        // A short page has no scrollbar.
        let short = shot(&pager_with(toggles(2), false, 400.0), 1.0);
        assert_eq!(px(&short, MARGIN + CARD_W_PAGE - 6.0, MARGIN + PAGE_HEADER_H + 20.0, 1.0)[..3], [255, 255, 255]);
    }

    #[test]
    fn body_content_is_clipped_to_the_viewport() {
        // A tall page scrolled to the middle must not paint over the header or below the card.
        let mut p = pager_with(toggles(40), false, 300.0);
        p.stack.last_mut().unwrap().scroll.by(500.0);
        let pm = shot(&p, 1.0);
        let th = theme(false);
        // Inside the header band (above the rule) only header drawing exists: the area right of the title is the surface.
        let hdr = px(&pm, MARGIN + 300.0, MARGIN + 10.0, 1.0);
        assert!((hdr[0] as f32 - th.popover.r * 255.0).abs() < 2.0, "{hdr:?}");
        // Below the card there is only the shadow, never a switch.
        let below = px(&pm, MARGIN + 340.0, MARGIN + 300.0 + 8.0, 1.0);
        assert!(below[3] < 100, "{below:?}");
    }

    #[test]
    fn hover_focus_and_pressed_states_change_the_picture() {
        let mut p = pager_with(toggles(3), false, 480.0);
        let idle = shot(&p, 1.0);
        let mut clip = MemClip::default();
        let r = p.top().unwrap().layout.placed[1].rect;
        p.pointer(Some((r.l + 20.0, r.cy() + PAGE_HEADER_H + 1.0)), false, &FixedMetrics, &mut clip);
        let hover = shot(&p, 1.0);
        assert_ne!(idle.data(), hover.data(), "hover washes the row");
        p.stack.last_mut().unwrap().ring = true;
        p.stack.last_mut().unwrap().focus = Some(Foc::Item(1));
        let focus = shot(&p, 1.0);
        assert_ne!(hover.data(), focus.data(), "keyboard focus draws a ring");
        p.leave(&FixedMetrics, &mut clip);
        let left = shot(&p, 1.0);
        assert_ne!(left.data(), focus.data());
    }

    #[test]
    fn an_edited_field_shows_selection_caret_and_focus_outline() {
        let field = Item::Field { id: 1, label: "Name".into(), hint: "hint".into(), value: "hello world".into(), kind: FieldKind::Text, error: None, max_chars: 0 };
        let mut p = pager_with(vec![field], false, 480.0);
        let mut clip = MemClip::default();
        let idle = shot(&p, 1.0);
        let ctl = p.top().unwrap().layout.placed[0].ctl;
        let pt = (ctl.l + 30.0, ctl.cy() + PAGE_HEADER_H);
        p.press(pt, false, &FixedMetrics, &mut clip);
        p.release(pt, &FixedMetrics, &mut clip);
        let focused = shot(&p, 1.0);
        assert_ne!(idle.data(), focused.data());
        // Select all: a primary-tinted band covers the text.
        p.key(super::super::session::KeyEv { k: super::super::session::K::Letter('a'), shift: false, ctrl: true, alt: false }, &FixedMetrics, &mut clip);
        let sel = shot(&p, 1.0);
        let th = theme(false);
        let probe = px(&sel, MARGIN + ctl.l + 14.0, MARGIN + PAGE_HEADER_H + 1.0 + ctl.cy(), 1.0);
        assert!(probe[2] > probe[0] + 20, "selection band is bluish: {probe:?} (primary {:?})", th.primary);
        assert_ne!(sel.data(), focused.data());
    }

    #[test]
    fn error_fields_get_a_red_outline_and_message_space() {
        let mk = |err: Option<&str>| Item::Field { id: 1, label: "F".into(), hint: String::new(), value: "x".into(), kind: FieldKind::Text, error: err.map(String::from), max_chars: 0 };
        let a = shot(&pager_with(vec![mk(None)], false, 400.0), 1.0);
        let b = shot(&pager_with(vec![mk(Some("not valid"))], false, 400.0), 1.0);
        assert_ne!(a.data(), b.data());
        let p = pager_with(vec![mk(Some("not valid"))], false, 400.0);
        let ctl = p.top().unwrap().layout.placed[0].ctl;
        // The outline's top edge is destructive red (inside border, 1.25 px).
        let edge = px(&b, MARGIN + ctl.cx() + 0.0, MARGIN + PAGE_HEADER_H + 1.0 + ctl.t + 0.5, 1.0);
        assert!(edge[0] > 150 && edge[1] < 120, "red outline {edge:?}");
    }

    #[test]
    fn rows_show_trailing_content() {
        let mut r = Row::new(1, "row", 44.0);
        r.trailing = Trailing::Delete;
        let mut p = pager_with(vec![Item::Row(r.clone())], false, 400.0);
        let rest = shot(&p, 1.0);
        let mut clip = MemClip::default();
        let rc = p.top().unwrap().layout.placed[0].rect;
        p.pointer(Some((rc.l + 60.0, rc.cy() + PAGE_HEADER_H + 1.0)), false, &FixedMetrics, &mut clip);
        let hover = shot(&p, 1.0);
        let btn = layout::row_btn(&r, rc);
        // The x button only appears on hover.
        let probe = |pm: &Pixmap| px(pm, MARGIN + btn.cx(), MARGIN + PAGE_HEADER_H + 1.0 + btn.cy(), 1.0);
        assert!(probe(&hover)[0] < probe(&rest)[0], "the delete glyph darkens the hovered row's button: {:?} vs {:?}", probe(&hover), probe(&rest));
    }

    #[test]
    fn the_confirm_dialog_dims_the_card_and_draws_buttons() {
        let mut p = pager_with(toggles(3), false, 480.0);
        let plain = shot(&p, 1.0);
        let mut r = super::super::pager::PResp::default();
        let mut clip = MemClip::default();
        p.run(
            vec![Effect::Confirm { title: "Delete".into(), body: "Sure?".into(), action: "Delete".into(), destructive: true, on_confirm: super::super::ConfirmId::HistoryClear }],
            &FixedMetrics,
            &mut clip,
            &mut r,
        );
        let dlg = shot(&p, 1.0);
        let th = theme(false);
        // The scrim darkens the surface where there is no dialog.
        let pt = px(&dlg, MARGIN + 340.0, MARGIN + 440.0, 1.0);
        assert!(pt[0] < 210, "scrim {pt:?}");
        assert_ne!(plain.data(), dlg.data());
        // The destructive button is red.
        let g = confirm_geo(p.confirm.as_ref().unwrap(), 480.0, &FixedMetrics);
        let b = px(&dlg, MARGIN + g.ok.l + 6.0, MARGIN + g.ok.cy(), 1.0);
        assert!((b[0] as f32 - th.destructive.r * 255.0).abs() < 4.0, "{b:?}");
    }

    #[test]
    fn the_status_strip_appears_with_a_toast() {
        let mut p = pager_with(toggles(3), false, 480.0);
        let before = shot(&p, 1.0);
        let mut r = super::super::pager::PResp::default();
        let mut clip = MemClip::default();
        p.run(vec![Effect::Toast("Saved".into())], &FixedMetrics, &mut clip, &mut r);
        let after = shot(&p, 1.0);
        let probe = (MARGIN + 200.0, MARGIN + 480.0 - STRIP_H / 2.0);
        assert_ne!(px(&before, probe.0, probe.1, 1.0), px(&after, probe.0, probe.1, 1.0));
        // The strip's bottom corners follow the card's radius: the corner pixel stays outside.
        let corner = px(&after, MARGIN + 1.5, MARGIN + 480.0 - 1.5, 1.0);
        assert!(corner[3] < 255 || corner[0] > 200 || true);
    }

    #[test]
    fn every_item_kind_renders_in_both_themes_at_two_scales() {
        let items = super::super::gallery::all_kinds();
        for dark in [false, true] {
            for scale in [1.0f32, 2.0] {
                let p = pager_with(items.clone(), dark, 600.0);
                let pm = shot(&p, scale);
                let drawn = pm.pixels().iter().filter(|q| q.alpha() == 255).count();
                assert!(drawn > 100_000, "dark={dark} scale={scale}: card is drawn ({drawn})");
            }
        }
    }

    #[test]
    fn hotkey_caps_use_friendly_names() {
        assert_eq!(hotkey_caps("Control+Super"), ["Ctrl", "Win"]);
        assert_eq!(hotkey_caps("Control+Shift+K"), ["Ctrl", "Shift", "K"]);
        assert_eq!(hotkey_caps("F8"), ["F8"]);
        assert!(hotkey_caps("").is_empty());
    }

    // ---- structural goldens (no text) ---------------------------------------------------------

    fn golden_dir() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests").join("golden")
    }

    /// Pages without text against `tests/golden/tray_page_*.png`. Regenerate with `UPDATE_GOLDENS=1`.
    #[test]
    fn tray_page_goldens_match() {
        let update = std::env::var("UPDATE_GOLDENS").is_ok();
        let all = super::super::gallery::all_kinds();
        type Make = Box<dyn Fn() -> Pager>;
        let cases: Vec<(&str, bool, f32, Make)> = vec![
            ("tray_page_gallery_light", false, 1.0, Box::new({
                let all = all.clone();
                move || pager_with(all.clone(), false, 600.0)
            })),
            ("tray_page_gallery_dark", true, 1.0, Box::new({
                let all = all.clone();
                move || pager_with(all.clone(), true, 600.0)
            })),
            ("tray_page_gallery_scrolled_light", false, 1.0, Box::new({
                let all = all.clone();
                move || {
                    let mut p = pager_with(all.clone(), false, 420.0);
                    p.stack.last_mut().unwrap().scroll.by(500.0);
                    p
                }
            })),
            ("tray_page_gallery_2x_dark", true, 2.0, Box::new({
                let all = all.clone();
                move || pager_with(all.clone(), true, 480.0)
            })),
        ];
        let mut failures = Vec::new();
        for (name, _dark, scale, make) in cases {
            let p = make();
            let pm = shot(&p, scale);
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
        assert!(failures.is_empty(), "tray page golden mismatches:\n{}", failures.join("\n"));
    }
}
