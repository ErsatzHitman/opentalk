//! Custom-painted widgets that reproduce OpenWhispr's component look (buttons, toggle,
//! segmented control, pill tabs, inputs, selects, settings panels, chips, dialogs).
//!
//! Everything takes the [`Palette`] explicitly and animates hover/press through
//! `Context::animate_bool_with_time`, which only requests repaints while a transition runs.
use super::fonts;
use super::icons;
use super::theme::{Palette, mix, with_alpha};
use eframe::egui::{
    self, Align, Align2, Color32, CornerRadius, CursorIcon, FontId, Id, Layout, Margin, Rect, Response, RichText, Sense,
    Stroke, StrokeKind, Ui, UiBuilder, Vec2, pos2, vec2,
};

/// Corner radius helper (egui wants `u8`).
pub fn cr(r: f32) -> CornerRadius {
    CornerRadius::same(r.round().clamp(0.0, 255.0) as u8)
}

pub fn hover_anim(ui: &Ui, id: Id, on: bool) -> f32 {
    ui.ctx().animate_bool_with_time(id, on, 0.12)
}

pub fn pointer(resp: &Response) {
    if resp.hovered() {
        resp.ctx.set_cursor_icon(CursorIcon::PointingHand);
    }
}

/// Plain text label with an explicit size/colour (wraps like `ui.label`).
pub fn text(ui: &mut Ui, s: impl Into<String>, size: f32, color: Color32) -> Response {
    ui.label(RichText::new(s.into()).font(fonts::regular(size)).color(color))
}

pub fn text_semi(ui: &mut Ui, s: impl Into<String>, size: f32, color: Color32) -> Response {
    ui.label(RichText::new(s.into()).font(fonts::semibold(size)).color(color))
}

pub fn text_nowrap(ui: &mut Ui, s: impl Into<String>, font: FontId, color: Color32) -> Response {
    ui.add(egui::Label::new(RichText::new(s.into()).font(font).color(color)).wrap_mode(egui::TextWrapMode::Truncate))
}

/// Width of `s` laid out in `font` on one line.
pub fn measure(ui: &Ui, s: &str, font: FontId) -> Vec2 {
    ui.painter().layout_no_wrap(s.to_owned(), font, Color32::WHITE).size()
}

/// Draw a single icon glyph centred in `rect`.
pub fn paint_icon(ui: &Ui, rect: Rect, glyph: char, size: f32, color: Color32) {
    ui.painter().text(rect.center(), Align2::CENTER_CENTER, glyph.to_string(), fonts::icon(size), color);
}

// ---------------------------------------------------------------- buttons

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Variant {
    /// Brand capsule.
    Primary,
    /// `outline`: muted fill + border.
    Outline,
    /// `outline-flat`: transparent + hairline border.
    Flat,
    /// No chrome, hover tint.
    Ghost,
    Destructive,
    /// Outline in destructive colour (`text-destructive border-destructive/25`).
    DestructiveOutline,
}

pub struct Btn<'a> {
    pub label: &'a str,
    pub icon: Option<char>,
    pub variant: Variant,
    pub small: bool,
    pub enabled: bool,
    pub min_width: f32,
}

impl<'a> Btn<'a> {
    pub fn new(label: &'a str, variant: Variant) -> Self {
        Self { label, icon: None, variant, small: false, enabled: true, min_width: 0.0 }
    }
    pub fn icon(mut self, g: char) -> Self {
        self.icon = Some(g);
        self
    }
    pub fn small(mut self) -> Self {
        self.small = true;
        self
    }
    pub fn enabled(mut self, e: bool) -> Self {
        self.enabled = e;
        self
    }
    pub fn min_width(mut self, w: f32) -> Self {
        self.min_width = w;
        self
    }
}

pub fn button(ui: &mut Ui, p: &Palette, b: Btn) -> Response {
    let (h, fs, ics, padx) = if b.small { (28.0, 12.0, 12.0, 12.0) } else { (32.0, 13.0, 13.0, 16.0) };
    let font = fonts::semibold(fs);
    let tw = if b.label.is_empty() { 0.0 } else { measure(ui, b.label, font.clone()).x };
    let iw = if b.icon.is_some() { ics + if b.label.is_empty() { 0.0 } else { 6.0 } } else { 0.0 };
    let w = (tw + iw + padx * 2.0).max(b.min_width);
    let sense = if b.enabled { Sense::click() } else { Sense::hover() };
    let (rect, resp) = ui.allocate_exact_size(vec2(w, h), sense);
    let id = resp.id;
    let hov = hover_anim(ui, id.with("h"), resp.hovered() && b.enabled);
    let down = resp.is_pointer_button_down_on() && b.enabled;
    if b.enabled {
        pointer(&resp);
    }
    let (mut fill, mut stroke, fg) = match b.variant {
        Variant::Primary => (mix(p.primary, Color32::WHITE, 0.08 * hov), Stroke::new(1.0, mix(p.primary, Color32::BLACK, 0.12)), p.primary_fg),
        Variant::Outline => (
            mix(if p.dark { p.raised } else { with_alpha(p.muted, 70) }, if p.dark { p.raised } else { p.muted }, hov),
            Stroke::new(1.0, mix(if p.dark { p.border_hover } else { p.border_a(70) }, p.border_hover, hov)),
            p.foreground,
        ),
        Variant::Flat => (
            mix(Color32::TRANSPARENT, p.fg_a(if p.dark { 5 } else { 3 }), hov),
            Stroke::new(1.0, mix(if p.dark { with_alpha(Color32::WHITE, 10) } else { p.border_a(70) }, p.border, hov)),
            mix(p.muted_a(70), p.fg_a(80), hov),
        ),
        Variant::Ghost => (mix(Color32::TRANSPARENT, p.overlay(6), hov), Stroke::NONE, mix(p.muted_a(80), p.foreground, hov)),
        Variant::Destructive => (mix(p.destructive, Color32::WHITE, 0.06 * hov), Stroke::new(1.0, with_alpha(p.destructive, 70)), Color32::WHITE),
        Variant::DestructiveOutline => (
            mix(Color32::TRANSPARENT, p.destructive_a(8), hov),
            Stroke::new(1.0, p.destructive_a(25 + (hov * 20.0) as u8)),
            p.destructive,
        ),
    };
    if down {
        fill = mix(fill, Color32::BLACK, 0.06);
    }
    if !b.enabled {
        fill = with_alpha_mul(fill, 0.5);
        stroke.color = with_alpha_mul(stroke.color, 0.5);
    }
    let fg = if b.enabled { fg } else { with_alpha_mul(fg, 0.5) };
    let painter = ui.painter();
    painter.rect_filled(rect, cr(h / 2.0), fill);
    if stroke.width > 0.0 {
        painter.rect_stroke(rect, cr(h / 2.0), stroke, StrokeKind::Inside);
    }
    if b.variant == Variant::Primary {
        // Soft top highlight, a nod to the "brand glass" capsule.
        let hl = Rect::from_min_max(rect.min + vec2(h / 2.0, 1.0), pos2(rect.max.x - h / 2.0, rect.min.y + 2.0));
        painter.rect_filled(hl, cr(1.0), with_alpha(Color32::WHITE, 28));
    }
    let content_w = tw + iw;
    let mut x = rect.center().x - content_w / 2.0;
    if let Some(g) = b.icon {
        painter.text(pos2(x + ics / 2.0, rect.center().y), Align2::CENTER_CENTER, g.to_string(), fonts::icon(ics), fg);
        x += ics + if b.label.is_empty() { 0.0 } else { 6.0 };
    }
    if !b.label.is_empty() {
        painter.text(pos2(x, rect.center().y), Align2::LEFT_CENTER, b.label, font, fg);
    }
    resp
}

pub fn with_alpha_mul(c: Color32, k: f32) -> Color32 {
    Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), (c.a() as f32 * k) as u8)
}

/// Square/circle icon button (history row actions, window chrome, etc.).
pub fn icon_button(ui: &mut Ui, p: &Palette, glyph: char, size: f32, icon_size: f32, hover_fg: Color32, hover_bg: Color32) -> Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(size, size), Sense::click());
    let hov = hover_anim(ui, resp.id.with("h"), resp.hovered());
    pointer(&resp);
    ui.painter().rect_filled(rect, cr(size / 2.0), mix(Color32::TRANSPARENT, hover_bg, hov));
    paint_icon(ui, rect, glyph, icon_size, mix(p.muted_a(70), hover_fg, hov));
    resp
}

/// Ghost icon button using the default hover tint.
pub fn ghost_icon(ui: &mut Ui, p: &Palette, glyph: char) -> Response {
    icon_button(ui, p, glyph, 28.0, 13.0, p.foreground, p.fg_a(6))
}

// ---------------------------------------------------------------- toggle

/// 44x24 switch with a 16 px thumb (`ui/toggle.tsx`). Returns true when changed.
pub fn toggle(ui: &mut Ui, p: &Palette, value: &mut bool) -> bool {
    let (rect, resp) = ui.allocate_exact_size(vec2(44.0, 24.0), Sense::click());
    pointer(&resp);
    let mut changed = false;
    if resp.clicked() {
        *value = !*value;
        changed = true;
    }
    let t = ui.ctx().animate_bool_with_time(resp.id.with("on"), *value, 0.15);
    let hov = hover_anim(ui, resp.id.with("h"), resp.hovered());
    let off = if p.dark { mix(p.raised, p.surface3, hov) } else { with_alpha(p.muted_fg, 30 + (hov * 10.0) as u8) };
    let off = if p.dark { off } else { mix(p.background, off, 1.0) };
    let on = mix(p.primary, Color32::WHITE, 0.06 * hov);
    let track = mix(off, on, t);
    let painter = ui.painter();
    painter.rect_filled(rect, cr(12.0), track);
    let x = rect.min.x + 4.0 + 8.0 + t * 20.0;
    let c = pos2(x, rect.center().y);
    painter.circle_filled(c + vec2(0.0, 0.5), 8.0, Color32::from_black_alpha(25));
    painter.circle_filled(c, 8.0, if p.dark { mix(p.foreground, p.background, 0.0) } else { Color32::WHITE });
    changed
}

// ---------------------------------------------------------------- segmented / tabs

/// Segmented control (e.g. Light | Dark | Auto). Returns true when the selection changed.
pub fn segmented(ui: &mut Ui, p: &Palette, selected: &mut usize, labels: &[&str]) -> bool {
    let font = fonts::semibold(12.0);
    let widths: Vec<f32> = labels.iter().map(|l| measure(ui, l, font.clone()).x + 28.0).collect();
    let total: f32 = widths.iter().sum::<f32>() + 6.0;
    let (rect, _) = ui.allocate_exact_size(vec2(total, 32.0), Sense::hover());
    let painter = ui.painter().clone();
    painter.rect_filled(rect, cr(8.0), if p.dark { p.surface3 } else { p.muted });
    painter.rect_stroke(rect, cr(8.0), Stroke::new(1.0, p.panel_border()), StrokeKind::Inside);
    let mut x = rect.min.x + 3.0;
    let mut changed = false;
    for (i, l) in labels.iter().enumerate() {
        let r = Rect::from_min_size(pos2(x, rect.min.y + 3.0), vec2(widths[i], 26.0));
        let resp = ui.interact(r, ui.id().with(("seg", i, *l)), Sense::click());
        pointer(&resp);
        let sel = *selected == i;
        let t = ui.ctx().animate_bool_with_time(resp.id.with("s"), sel, 0.12);
        if t > 0.0 {
            painter.rect_filled(r, cr(6.0), with_alpha_mul(if p.dark { p.raised } else { Color32::WHITE }, t));
            if !p.dark {
                painter.rect_stroke(r, cr(6.0), Stroke::new(1.0, with_alpha_mul(p.border, t)), StrokeKind::Inside);
            }
        }
        let hov = hover_anim(ui, resp.id.with("h"), resp.hovered() && !sel);
        let col = mix(mix(p.muted_fg, p.foreground, hov), p.foreground, t);
        painter.text(r.center(), Align2::CENTER_CENTER, *l, font.clone(), col);
        if resp.clicked() && !sel {
            *selected = i;
            changed = true;
        }
        x += widths[i];
    }
    changed
}

/// Pill tabs (provider tabs, Dictionary | Snippets). Wraps onto several lines. Returns the clicked index.
pub fn pill_tabs(ui: &mut Ui, p: &Palette, selected: usize, labels: &[(&str, &str)]) -> Option<usize> {
    let font = fonts::semibold(12.0);
    let small = fonts::regular(10.0);
    let avail = ui.available_width();
    let mut x = 0.0f32;
    let mut y = 0.0f32;
    let gap = 4.0;
    let h = 28.0;
    let origin = ui.cursor().min;
    let mut clicked = None;
    let mut max_x = 0.0f32;
    for (i, (label, badge)) in labels.iter().enumerate() {
        let tw = measure(ui, label, font.clone()).x;
        let bw = if badge.is_empty() { 0.0 } else { measure(ui, badge, small.clone()).x + 4.0 };
        let w = tw + bw + 20.0;
        if x > 0.0 && x + w > avail {
            x = 0.0;
            y += h + gap;
        }
        let r = Rect::from_min_size(origin + vec2(x, y), vec2(w, h));
        let resp = ui.interact(r, ui.id().with(("pill", i)), Sense::click());
        pointer(&resp);
        let sel = i == selected;
        let t = ui.ctx().animate_bool_with_time(resp.id.with("s"), sel, 0.15);
        let hov = hover_anim(ui, resp.id.with("h"), resp.hovered() && !sel);
        let painter = ui.painter();
        let ring_idle = if p.dark { with_alpha(Color32::WHITE, 10) } else { p.border_a(60) };
        let ring = mix(ring_idle, p.primary_a(if p.dark { 25 } else { 30 }), t);
        painter.rect_filled(r, cr(h / 2.0), mix(mix(Color32::TRANSPARENT, p.fg_a(5), hov), p.primary_a(if p.dark { 15 } else { 10 }), t));
        painter.rect_stroke(r, cr(h / 2.0), Stroke::new(1.0, ring), StrokeKind::Inside);
        let col = mix(mix(p.muted_fg, p.foreground, hov), p.foreground, t);
        let tx = r.min.x + 10.0;
        painter.text(pos2(tx, r.center().y), Align2::LEFT_CENTER, label, font.clone(), col);
        if bw > 0.0 {
            painter.text(pos2(tx + tw + 4.0, r.center().y + 0.5), Align2::LEFT_CENTER, badge, small.clone(), with_alpha(p.primary, 70));
        }
        if resp.clicked() {
            clicked = Some(i);
        }
        x += w + gap;
        max_x = max_x.max(x);
    }
    ui.allocate_rect(Rect::from_min_size(origin, vec2(avail.min(max_x), y + h)), Sense::hover());
    clicked
}

// ---------------------------------------------------------------- inputs

/// Single-line text input in OpenWhispr's input chrome. `password` masks the content.
pub fn text_input(ui: &mut Ui, p: &Palette, value: &mut String, hint: &str, width: f32, password: bool) -> Response {
    text_input_h(ui, p, value, hint, width, 32.0, password, false)
}

#[allow(clippy::too_many_arguments)]
pub fn text_input_h(ui: &mut Ui, p: &Palette, value: &mut String, hint: &str, width: f32, height: f32, password: bool, round: bool) -> Response {
    let width = if width <= 0.0 { ui.available_width() } else { width };
    let (rect, _) = ui.allocate_exact_size(vec2(width, height), Sense::hover());
    let id = ui.id().with(("input", rect.min.x as i32, rect.min.y as i32, hint));
    let focused = ui.memory(|m| m.has_focus(id));
    let hov = ui.rect_contains_pointer(rect);
    let t = ui.ctx().animate_bool_with_time(id.with("f"), focused, 0.12);
    let radius = if round { height / 2.0 } else { 6.0 };
    let painter = ui.painter();
    painter.rect_filled(rect, cr(radius), p.input_fill());
    if t > 0.0 {
        painter.rect_stroke(rect.expand(1.5 * t), cr(radius + 1.5), Stroke::new(2.0, p.primary_a(12)), StrokeKind::Outside);
    }
    let base = if hov { p.border_hover } else { p.input_border() };
    painter.rect_stroke(rect, cr(radius), Stroke::new(1.0, mix(base, if p.dark { p.primary } else { p.primary }, t)), StrokeKind::Inside);
    let inner = rect.shrink2(vec2(if round { 14.0 } else { 10.0 }, 0.0));
    let edit = egui::TextEdit::singleline(value)
        .id(id)
        .frame(egui::Frame::NONE)
        .hint_text(RichText::new(hint).color(p.muted_a(70)))
        .font(fonts::regular(13.0))
        .text_color(p.foreground)
        .password(password)
        .margin(Margin::ZERO)
        .vertical_align(Align::Center)
        .desired_width(inner.width());
    ui.put(inner, edit)
}

/// Multi-line editor with the same chrome.
pub fn text_area(ui: &mut Ui, p: &Palette, value: &mut String, hint: &str, rows: usize, mono: bool) -> Response {
    let width = ui.available_width();
    let h = rows as f32 * 18.0 + 20.0;
    let (rect, _) = ui.allocate_exact_size(vec2(width, h), Sense::hover());
    let id = ui.id().with(("area", rect.min.x as i32, rect.min.y as i32, hint));
    let focused = ui.memory(|m| m.has_focus(id));
    let t = ui.ctx().animate_bool_with_time(id.with("f"), focused, 0.12);
    let painter = ui.painter();
    painter.rect_filled(rect, cr(6.0), p.input_fill());
    painter.rect_stroke(rect, cr(6.0), Stroke::new(1.0, mix(p.input_border(), p.primary, t)), StrokeKind::Inside);
    let inner = rect.shrink2(vec2(10.0, 8.0));
    let font = if mono { FontId::monospace(12.0) } else { fonts::regular(12.5) };
    let mut child = ui.new_child(UiBuilder::new().max_rect(inner).layout(Layout::top_down(Align::Min)));
    let out = egui::ScrollArea::vertical().id_salt(id.with("sa")).auto_shrink([false, false]).show(&mut child, |ui| {
        ui.add(
            egui::TextEdit::multiline(value)
                .id(id)
                .frame(egui::Frame::NONE)
                .hint_text(RichText::new(hint).color(p.muted_a(70)))
                .font(font)
                .text_color(p.foreground)
                .margin(Margin::ZERO)
                .desired_width(inner.width())
                .desired_rows(rows),
        )
    });
    out.inner
}

/// Dropdown. `options` are display labels; returns the newly chosen index. When `searchable`,
/// a filter box is shown above the list. `selected_label` is shown in the closed state.
pub struct SelectOut {
    pub chosen: Option<usize>,
}

#[allow(clippy::too_many_arguments)]
pub fn select(
    ui: &mut Ui,
    p: &Palette,
    salt: &str,
    selected_label: &str,
    options: &[String],
    selected: Option<usize>,
    width: f32,
    searchable: bool,
) -> SelectOut {
    let (rect, resp) = ui.allocate_exact_size(vec2(width, 32.0), Sense::click());
    pointer(&resp);
    let open = egui::Popup::is_id_open(ui.ctx(), egui::Popup::default_response_id(&resp));
    let hov = hover_anim(ui, resp.id.with("h"), resp.hovered() || open);
    let painter = ui.painter();
    painter.rect_filled(rect, cr(6.0), p.input_fill());
    painter.rect_stroke(rect, cr(6.0), Stroke::new(1.0, mix(p.input_border(), if open { p.primary } else { p.border_hover }, hov)), StrokeKind::Inside);
    let clip = Rect::from_min_max(rect.min, pos2(rect.max.x - 28.0, rect.max.y));
    ui.painter().with_clip_rect(clip).text(
        pos2(rect.min.x + 10.0, rect.center().y),
        Align2::LEFT_CENTER,
        selected_label,
        fonts::regular(13.0),
        p.foreground,
    );
    paint_icon(ui, Rect::from_center_size(pos2(rect.max.x - 14.0, rect.center().y), vec2(14.0, 14.0)), icons::CHEVRON_DOWN, 10.0, p.muted_a(80));

    let mut chosen = None;
    let list_w = width.max(180.0);
    let filter_id = Id::new(("select-filter", salt));
    egui::Popup::from_toggle_button_response(&resp)
        .kind(egui::PopupKind::Menu)
        .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
        .gap(4.0)
        .width(list_w)
        .frame(popup_frame(p))
        .show(|ui| {
            ui.set_min_width(list_w - 8.0);
            ui.spacing_mut().item_spacing = vec2(0.0, 1.0);
            let mut filter = ui.memory_mut(|m| m.data.get_temp::<String>(filter_id).unwrap_or_default());
            if searchable {
                ui.add_space(2.0);
                let w = ui.available_width();
                text_input_h(ui, p, &mut filter, "Search...", w, 28.0, false, false);
                ui.add_space(4.0);
                ui.memory_mut(|m| m.data.insert_temp(filter_id, filter.clone()));
            }
            let needle = filter.to_lowercase();
            egui::ScrollArea::vertical().max_height(280.0).auto_shrink([false, true]).show(ui, |ui| {
                for (i, label) in options.iter().enumerate() {
                    if !needle.is_empty() && !label.to_lowercase().contains(&needle) {
                        continue;
                    }
                    if menu_item(ui, p, label, selected == Some(i), false, None).clicked() {
                        chosen = Some(i);
                        egui::Popup::close_all(ui.ctx());
                    }
                }
            });
        });
    if chosen.is_some() {
        ui.memory_mut(|m| m.data.remove::<String>(filter_id));
    }
    SelectOut { chosen }
}

pub fn popup_frame(p: &Palette) -> egui::Frame {
    egui::Frame::new()
        .fill(if p.dark { p.surface2 } else { Color32::WHITE })
        .stroke(Stroke::new(1.0, if p.dark { with_alpha(Color32::WHITE, 12) } else { p.border }))
        .corner_radius(cr(8.0))
        .inner_margin(Margin::same(4))
        .shadow(egui::Shadow { offset: [0, 6], blur: 20, spread: 0, color: Color32::from_black_alpha(if p.dark { 120 } else { 40 }) })
}

/// A dropdown-menu row: optional leading icon, check mark when selected, destructive tint.
pub fn menu_item(ui: &mut Ui, p: &Palette, label: &str, selected: bool, destructive: bool, icon: Option<char>) -> Response {
    let w = ui.available_width();
    let (rect, resp) = ui.allocate_exact_size(vec2(w, 30.0), Sense::click());
    pointer(&resp);
    let hov = hover_anim(ui, resp.id.with("h"), resp.hovered());
    let painter = ui.painter();
    let tint = if destructive { p.destructive_a(10) } else { p.overlay(6) };
    painter.rect_filled(rect, cr(6.0), mix(Color32::TRANSPARENT, tint, hov));
    let col = if destructive { p.destructive } else { p.foreground };
    let mut x = rect.min.x + 10.0;
    if let Some(g) = icon {
        painter.text(pos2(x + 6.5, rect.center().y), Align2::CENTER_CENTER, g.to_string(), fonts::icon(13.0), if destructive { col } else { p.muted_a(90) });
        x += 22.0;
    }
    painter.text(pos2(x, rect.center().y), Align2::LEFT_CENTER, label, fonts::regular(13.0), col);
    if selected {
        painter.text(pos2(rect.max.x - 14.0, rect.center().y), Align2::CENTER_CENTER, icons::CHECK.to_string(), fonts::icon(11.0), p.primary);
    }
    resp
}

// ---------------------------------------------------------------- panels and rows

/// Settings panel: `rounded-lg border bg-card/50` containing divided rows.
pub fn panel<R>(ui: &mut Ui, p: &Palette, add: impl FnOnce(&mut Ui) -> R) -> R {
    let frame = egui::Frame::new().fill(p.panel_fill()).stroke(Stroke::new(1.0, p.panel_border())).corner_radius(cr(8.0));
    frame
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing = vec2(0.0, 0.0);
            ui.set_width(ui.available_width());
            add(ui)
        })
        .inner
}

pub fn divider(ui: &mut Ui, p: &Palette) {
    let w = ui.available_width();
    let (r, _) = ui.allocate_exact_size(vec2(w, 1.0), Sense::hover());
    ui.painter().rect_filled(r, 0.0, p.divider());
}

/// Label + description on the left and a control of width `ctrl_w` on the right.
pub fn row(ui: &mut Ui, p: &Palette, label: &str, desc: &str, ctrl_w: f32, control: impl FnOnce(&mut Ui)) {
    let pad_x = 16.0;
    let pad_y = 12.0;
    let full = ui.available_width();
    let top = ui.cursor().min;
    let label_w = (full - pad_x * 2.0 - ctrl_w - 16.0).max(80.0);
    let mut left = ui.new_child(
        UiBuilder::new().max_rect(Rect::from_min_size(top + vec2(pad_x, pad_y), vec2(label_w, 400.0))).layout(Layout::top_down(Align::Min)),
    );
    left.spacing_mut().item_spacing = vec2(0.0, 2.0);
    text_semi(&mut left, label, 12.5, p.foreground);
    if !desc.is_empty() {
        text(&mut left, desc, 12.0, p.muted_a(80));
    }
    let lh = left.min_rect().height();
    let row_h = (lh + pad_y * 2.0).max(32.0 + pad_y * 2.0 - 8.0);
    let cr_rect = Rect::from_min_max(pos2(top.x + full - pad_x - ctrl_w, top.y), pos2(top.x + full - pad_x, top.y + row_h));
    let mut right = ui.new_child(UiBuilder::new().max_rect(cr_rect).layout(Layout::right_to_left(Align::Center)));
    right.spacing_mut().item_spacing = vec2(8.0, 0.0);
    control(&mut right);
    ui.allocate_rect(Rect::from_min_size(top, vec2(full, row_h)), Sense::hover());
}

/// Row whose control sits under the label instead of beside it (wide inputs).
pub fn row_stacked(ui: &mut Ui, p: &Palette, label: &str, desc: &str, control: impl FnOnce(&mut Ui)) {
    egui::Frame::new().inner_margin(Margin::symmetric(16, 12)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.spacing_mut().item_spacing = vec2(0.0, 2.0);
        text_semi(ui, label, 12.5, p.foreground);
        if !desc.is_empty() {
            text(ui, desc, 12.0, p.muted_a(80));
        }
        ui.add_space(8.0);
        control(ui);
    });
}

/// Padding-only container for free-form panel content.
pub fn panel_pad<R>(ui: &mut Ui, x: i8, y: i8, add: impl FnOnce(&mut Ui) -> R) -> R {
    egui::Frame::new().inner_margin(Margin::symmetric(x, y)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        add(ui)
    }).inner
}

/// `<h3 text-xs font-semibold>` section title with optional description.
pub fn section_title(ui: &mut Ui, p: &Palette, title: &str, desc: &str) {
    text_semi(ui, title, 12.5, p.foreground);
    if !desc.is_empty() {
        ui.add_space(2.0);
        text(ui, desc, 12.0, p.muted_a(80));
    }
    ui.add_space(10.0);
}

// ---------------------------------------------------------------- chips

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ChipKind {
    Neutral,
    Primary,
    // Status tints, kept for the palette's success/warning pair; no page raises them yet.
    #[allow(dead_code)]
    Success,
    #[allow(dead_code)]
    Warning,
    Destructive,
}

/// Small rounded tag. Returns its response (for tooltips).
pub fn chip(ui: &mut Ui, p: &Palette, label: &str, kind: ChipKind) -> Response {
    let font = fonts::semibold(11.0);
    let tw = measure(ui, label, font.clone()).x;
    let (rect, resp) = ui.allocate_exact_size(vec2(tw + 12.0, 20.0), Sense::hover());
    let (bg, fg) = match kind {
        ChipKind::Neutral => (if p.dark { p.surface3 } else { with_alpha(p.muted, 100) }, p.muted_fg),
        ChipKind::Primary => (p.primary_a(10), p.primary),
        ChipKind::Success => (p.success_a(12), p.success),
        ChipKind::Warning => (p.warning_a(14), p.warning),
        ChipKind::Destructive => (p.destructive_a(10), p.destructive),
    };
    ui.painter().rect_filled(rect, cr(4.0), bg);
    ui.painter().text(rect.center(), Align2::CENTER_CENTER, label, font, fg);
    resp
}

/// Coloured circle with an initial: stand-in for the target application's icon.
pub fn avatar(ui: &mut Ui, p: &Palette, name: &str, size: f32) {
    let (rect, _) = ui.allocate_exact_size(vec2(size, size), Sense::hover());
    let hue = name.bytes().fold(7u32, |a, b| a.wrapping_mul(31).wrapping_add(b as u32)) % 360;
    let (r, g, b) = hsl_to_rgb(hue as f32, if p.dark { 0.45 } else { 0.55 }, if p.dark { 0.45 } else { 0.50 });
    ui.painter().rect_filled(rect, cr(size * 0.28), Color32::from_rgb(r, g, b));
    let initial = name.chars().find(|c| c.is_alphanumeric()).map(|c| c.to_uppercase().to_string()).unwrap_or_else(|| "?".into());
    ui.painter().text(rect.center(), Align2::CENTER_CENTER, initial, fonts::semibold(size * 0.56), Color32::WHITE);
}

pub fn hsl_to_rgb(h: f32, s: f32, l: f32) -> (u8, u8, u8) {
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let hp = (h % 360.0) / 60.0;
    let x = c * (1.0 - (hp % 2.0 - 1.0).abs());
    let (r1, g1, b1) = match hp as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = l - c / 2.0;
    let f = |v: f32| ((v + m) * 255.0).round().clamp(0.0, 255.0) as u8;
    (f(r1), f(g1), f(b1))
}

// ---------------------------------------------------------------- dialogs

pub enum DialogResult {
    Open,
    Confirm,
    Cancel,
}

/// Modal confirmation dialog (`Delete`, `Clear all`, `Reset`). `destructive` colours the action.
pub fn confirm_dialog(ctx: &egui::Context, p: &Palette, id: &str, title: &str, body: &str, action: &str, destructive: bool) -> DialogResult {
    let mut result = DialogResult::Open;
    let modal = egui::Modal::new(Id::new(id))
        .backdrop_color(Color32::from_black_alpha(if p.dark { 150 } else { 90 }))
        .frame(
            egui::Frame::new()
                .fill(if p.dark { p.surface1 } else { Color32::WHITE })
                .stroke(Stroke::new(1.0, if p.dark { with_alpha(Color32::WHITE, 12) } else { p.border }))
                .corner_radius(cr(12.0))
                .inner_margin(Margin::same(20))
                .shadow(egui::Shadow { offset: [0, 20], blur: 50, spread: 0, color: Color32::from_black_alpha(80) }),
        )
        .show(ctx, |ui| {
            ui.set_width(380.0);
            ui.spacing_mut().item_spacing = vec2(0.0, 6.0);
            text_semi(ui, title, 15.0, p.foreground);
            text(ui, body, 13.0, p.muted_a(90));
            ui.add_space(12.0);
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.spacing_mut().item_spacing = vec2(8.0, 0.0);
                let v = if destructive { Variant::Destructive } else { Variant::Primary };
                if button(ui, p, Btn::new(action, v).min_width(84.0)).clicked() {
                    result = DialogResult::Confirm;
                }
                if button(ui, p, Btn::new("Cancel", Variant::Outline).min_width(84.0)).clicked() {
                    result = DialogResult::Cancel;
                }
            });
        });
    if modal.should_close() && matches!(result, DialogResult::Open) {
        result = DialogResult::Cancel;
    }
    result
}

/// Open a URL, file or folder with the shell's default handler.
pub fn shell_open(target: &str) {
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    use windows::core::{HSTRING, PCWSTR, w};
    let t = HSTRING::from(target);
    // SAFETY: all strings are valid NUL-terminated UTF-16 for the duration of the call.
    unsafe {
        ShellExecuteW(None, w!("open"), &t, PCWSTR::null(), PCWSTR::null(), SW_SHOWNORMAL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hsl_primaries() {
        assert_eq!(hsl_to_rgb(0.0, 1.0, 0.5), (255, 0, 0));
        assert_eq!(hsl_to_rgb(120.0, 1.0, 0.5), (0, 255, 0));
        assert_eq!(hsl_to_rgb(240.0, 1.0, 0.5), (0, 0, 255));
    }

    #[test]
    fn alpha_mul() {
        assert_eq!(with_alpha_mul(Color32::from_rgba_unmultiplied(1, 2, 3, 200), 0.5).a(), 100);
    }
}
