//! Custom-painted widgets in the liquid-glass look (buttons, toggle, segmented control, pill
//! tabs, inputs, selects, settings panels, chips, menus, dialogs). See `glass.rs` and
//! docs/GLASS.md for the tiers and the four rules these implement.
//!
//! Everything takes the [`Palette`] explicitly and animates hover/press through
//! `Context::animate_bool_with_time`, which only requests repaints while a transition runs.
//!
//! Rule 3 (a child of glass paints no surface of its own) is enforced here: `panel`,
//! `menu_surface` and `glass_modal` mark their content as "inside glass", and every widget below
//! then draws a soft wash instead of its own chip / border.
use super::fonts;
use super::glass::{self, Gl};
use super::icons;
use super::theme::{Palette, mix, with_alpha};
use eframe::egui::{
    self, Align, Align2, Color32, CornerRadius, CursorIcon, FontId, Id, Layout, Margin, Rect, Response, RichText, Sense, Shape,
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

// ---------------------------------------------------------------- glass context

fn glass_flag() -> Id {
    Id::new("opentalk-inside-glass")
}

/// True while drawing the content of a glass surface (rule 3: no nested surfaces).
pub fn in_glass(ctx: &egui::Context) -> bool {
    ctx.data(|d| d.get_temp::<bool>(glass_flag())).unwrap_or(false)
}

/// Mark the content being drawn as inside (or outside) a glass surface; returns the previous value.
pub fn set_in_glass(ctx: &egui::Context, on: bool) -> bool {
    let prev = in_glass(ctx);
    ctx.data_mut(|d| d.insert_temp(glass_flag(), on));
    prev
}

/// The glass painting context (theme + window size) for this frame.
pub fn gl(ui: &Ui, p: &Palette) -> Gl {
    Gl::new(ui.ctx(), p.dark)
}

/// Wash fading from level `from` to level `to` as `t` goes 0 -> 1.
fn wash_to(p: &Palette, from: u8, to: u8, t: f32) -> Color32 {
    mix(p.wash(from), p.wash(to), t)
}

/// Draw `shapes` behind content that is laid out afterwards: reserve a slot now, fill it once the
/// content's rect is known.
fn reserve(ui: &Ui) -> egui::layers::ShapeIdx {
    ui.painter().add(Shape::Noop)
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
    /// Brand coloured glass.
    Primary,
    /// `outline`: a chip on the page, a wash inside a panel.
    Outline,
    /// `outline-flat`: the lighter chip.
    Flat,
    /// No chrome, hover wash.
    Ghost,
    /// Destructive coloured glass.
    Destructive,
    /// Destructive text on a destructive wash.
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
    let hv = if down { 1.0 } else { hov };
    let g = gl(ui, p);
    let inside = in_glass(ui.ctx());
    let mut painter = ui.painter().clone();
    if !b.enabled {
        painter.set_opacity(0.5);
    }
    let r = h / 2.0;
    let fg = match b.variant {
        // Coloured glass: only the tint is replaced, rim and border stay (recipes.md).
        Variant::Primary => {
            glass::colored(&painter, &g, rect, r, p.primary, hv, !inside);
            p.primary_fg
        }
        Variant::Destructive => {
            glass::colored(&painter, &g, rect, r, p.destructive, hv, !inside);
            Color32::WHITE
        }
        Variant::Outline => {
            if inside {
                painter.rect_filled(rect, cr(r), wash_to(p, 1, 2, hv));
            } else {
                glass::chip(&painter, &g, rect, r, hv);
            }
            p.foreground
        }
        Variant::Flat => {
            if inside {
                painter.rect_filled(rect, cr(r), wash_to(p, 0, 1, hv));
            } else {
                glass::chip(&painter, &g, rect, r, hv * 0.6);
            }
            mix(p.muted_a(80), p.fg_a(90), hov)
        }
        Variant::Ghost => {
            painter.rect_filled(rect, cr(r), with_alpha_mul(p.wash(1), hv));
            mix(p.muted_a(80), p.foreground, hov)
        }
        Variant::DestructiveOutline => {
            if !inside {
                glass::chip(&painter, &g, rect, r, hv);
            }
            painter.rect_filled(rect, cr(r), mix(p.destructive_a(8), p.destructive_a(18), hv));
            p.destructive
        }
    };
    let fg = if b.enabled { fg } else { with_alpha_mul(fg, 0.7) };
    let content_w = tw + iw;
    let mut x = rect.center().x - content_w / 2.0;
    let text_y = rect.center().y + if down { 0.5 } else { 0.0 };
    if let Some(glyph) = b.icon {
        painter.text(pos2(x + ics / 2.0, text_y), Align2::CENTER_CENTER, glyph.to_string(), fonts::icon(ics), fg);
        x += ics + if b.label.is_empty() { 0.0 } else { 6.0 };
    }
    if !b.label.is_empty() {
        painter.text(pos2(x, text_y), Align2::LEFT_CENTER, b.label, font, fg);
    }
    resp
}

pub fn with_alpha_mul(c: Color32, k: f32) -> Color32 {
    Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), (c.a() as f32 * k) as u8)
}

/// Square/circle icon button (history row actions, window chrome, etc.): a wash on hover, never a
/// surface of its own.
pub fn icon_button(ui: &mut Ui, p: &Palette, glyph: char, size: f32, icon_size: f32, hover_fg: Color32, hover_bg: Color32) -> Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(size, size), Sense::click());
    let hov = hover_anim(ui, resp.id.with("h"), resp.hovered());
    pointer(&resp);
    // Callers still pass the old flat-design tint; the default one becomes the glass wash, a
    // deliberate colour (e.g. destructive) is kept.
    let bg = if hover_bg == p.fg_a(6) || hover_bg.a() == 0 { p.wash(1) } else { hover_bg };
    ui.painter().rect_filled(rect, cr(size / 2.0), mix(Color32::TRANSPARENT, bg, hov));
    paint_icon(ui, rect, glyph, icon_size, mix(p.muted_a(75), hover_fg, hov));
    resp
}

/// Ghost icon button using the default hover wash.
pub fn ghost_icon(ui: &mut Ui, p: &Palette, glyph: char) -> Response {
    icon_button(ui, p, glyph, 28.0, 13.0, p.foreground, p.fg_a(6))
}

// ---------------------------------------------------------------- toggle

/// 44x24 switch with a 16 px thumb (`ui/toggle.tsx`). Returns true when changed. On is coloured
/// glass in the brand hue, off a wash with a hairline.
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
    let g = gl(ui, p);
    let painter = ui.painter();
    painter.rect_filled(rect, cr(12.0), wash_to(p, 1, 2, hov));
    painter.rect_stroke(rect, cr(12.0), Stroke::new(1.0, p.glass().border.color()), StrokeKind::Inside);
    if t > 0.0 {
        let mut on = painter.clone();
        on.set_opacity(t);
        glass::colored(&on, &g, rect, 12.0, p.primary, hov, false);
    }
    let x = rect.min.x + 4.0 + 8.0 + t * 20.0;
    let c = pos2(x, rect.center().y);
    painter.circle_filled(c + vec2(0.0, 1.0), 8.5, Color32::from_black_alpha(if p.dark { 70 } else { 34 }));
    painter.circle_filled(c, 8.0, if p.dark { Color32::from_rgb(0xee, 0xee, 0xf2) } else { Color32::WHITE });
    painter.circle_stroke(c, 8.0, Stroke::new(1.0, Color32::from_white_alpha(if p.dark { 90 } else { 200 })));
    changed
}

// ---------------------------------------------------------------- segmented / tabs

/// Segmented control (e.g. Light | Dark | Auto). Returns true when the selection changed.
/// A chip when it sits on the page, a wash inside a panel; the selected segment is a primary wash.
pub fn segmented(ui: &mut Ui, p: &Palette, selected: &mut usize, labels: &[&str]) -> bool {
    let font = fonts::semibold(12.0);
    let widths: Vec<f32> = labels.iter().map(|l| measure(ui, l, font.clone()).x + 28.0).collect();
    let total: f32 = widths.iter().sum::<f32>() + 6.0;
    let (rect, _) = ui.allocate_exact_size(vec2(total, 32.0), Sense::hover());
    let painter = ui.painter().clone();
    if in_glass(ui.ctx()) {
        painter.rect_filled(rect, cr(16.0), p.wash(1));
    } else {
        glass::chip(&painter, &gl(ui, p), rect, 16.0, 0.0);
    }
    let mut x = rect.min.x + 3.0;
    let mut changed = false;
    for (i, l) in labels.iter().enumerate() {
        let r = Rect::from_min_size(pos2(x, rect.min.y + 3.0), vec2(widths[i], 26.0));
        let resp = ui.interact(r, ui.id().with(("seg", i, *l)), Sense::click());
        pointer(&resp);
        let sel = *selected == i;
        let t = ui.ctx().animate_bool_with_time(resp.id.with("s"), sel, 0.12);
        let hov = hover_anim(ui, resp.id.with("h"), resp.hovered() && !sel);
        if hov > 0.0 {
            painter.rect_filled(r, cr(13.0), with_alpha_mul(p.wash(1), hov));
        }
        if t > 0.0 {
            painter.rect_filled(r, cr(13.0), with_alpha_mul(p.primary_wash(1), t));
        }
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
    let g = gl(ui, p);
    let inside = in_glass(ui.ctx());
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
        if inside {
            painter.rect_filled(r, cr(h / 2.0), mix(Color32::TRANSPARENT, p.wash(1), hov));
        } else {
            glass::chip(painter, &g, r, h / 2.0, hov);
        }
        if t > 0.0 {
            painter.rect_filled(r, cr(h / 2.0), with_alpha_mul(p.primary_wash(1), t));
        }
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

/// The input chrome of rule 3: no box. A hairline along the bottom, a soft wash on hover and, when
/// focused, the hairline in the brand colour plus a low-alpha primary focus ring. `round` is the
/// search-field variant, a wash pill.
fn input_chrome(ui: &Ui, p: &Palette, rect: Rect, focus_t: f32, hover_t: f32, round: bool) {
    let painter = ui.painter();
    let radius = if round { rect.height() / 2.0 } else { 8.0 };
    if round {
        painter.rect_filled(rect, cr(radius), wash_to(p, 0, 1, hover_t));
    } else if hover_t > 0.0 {
        painter.rect_filled(rect, cr(radius), with_alpha_mul(p.wash(0), hover_t));
    }
    if focus_t > 0.0 {
        painter.rect_filled(rect, cr(radius), with_alpha_mul(p.primary_a(6), focus_t));
        painter.rect_stroke(rect.expand(1.0), cr(radius + 1.0), Stroke::new(2.0, p.primary_a(if p.dark { 38 } else { 32 }).gamma_multiply(focus_t)), StrokeKind::Outside);
    }
    if !round {
        let y = rect.max.y - 0.5;
        let line = mix(mix(p.input_border(), p.fg_a(if p.dark { 40 } else { 45 }), hover_t), p.primary, focus_t);
        painter.line_segment([pos2(rect.min.x + 1.0, y), pos2(rect.max.x - 1.0, y)], Stroke::new(1.0, line));
    }
}

/// Single-line text input in the glass chrome. `password` masks the content.
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
    let ht = ui.ctx().animate_bool_with_time(id.with("h"), hov && !focused, 0.12);
    input_chrome(ui, p, rect, t, ht, round);
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
    let hov = ui.rect_contains_pointer(rect);
    let t = ui.ctx().animate_bool_with_time(id.with("f"), focused, 0.12);
    let ht = ui.ctx().animate_bool_with_time(id.with("h"), hov && !focused, 0.12);
    input_chrome(ui, p, rect, t, ht, false);
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
    let hov = hover_anim(ui, resp.id.with("h"), resp.hovered() && !open);
    let ot = hover_anim(ui, resp.id.with("o"), open);
    // Closed state is an input, so it follows rule 3: hairline, wash on hover, brand line when open.
    input_chrome(ui, p, rect, 0.0, hov, false);
    if ot > 0.0 {
        let y = rect.max.y - 0.5;
        ui.painter().line_segment([pos2(rect.min.x + 1.0, y), pos2(rect.max.x - 1.0, y)], Stroke::new(1.0, p.primary.gamma_multiply(ot)));
    }
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
        .frame(egui::Frame::NONE)
        .show(|ui| {
            menu_surface(ui, p, |ui| {
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
        });
    if chosen.is_some() {
        ui.memory_mut(|m| m.data.remove::<String>(filter_id));
    }
    SelectOut { chosen }
}

/// Menu tier surface behind `add` (dropdowns, popovers, context menus): a 55 % wash of the menu
/// surface with border, shadows and top rim. Use it as the only content of a `Popup` shown with
/// `.frame(Frame::NONE)`.
pub fn menu_surface<R>(ui: &mut Ui, p: &Palette, add: impl FnOnce(&mut Ui) -> R) -> R {
    let g = gl(ui, p);
    let slot = reserve(ui);
    let prev = set_in_glass(ui.ctx(), true);
    let out = egui::Frame::new().inner_margin(Margin::same(4)).show(ui, add);
    set_in_glass(ui.ctx(), prev);
    ui.painter().set(slot, Shape::Vec(glass::menu_shapes(&g, out.response.rect, glass::RADIUS)));
    out.inner
}

/// An egui frame approximating the menu tier, for popups whose content is not wrapped in
/// [`menu_surface`] yet: the same surface and border, opaque enough to read over anything. Prefer
/// `.frame(Frame::NONE)` + `menu_surface`, which also gets the shadow and the rim.
pub fn popup_frame(p: &Palette) -> egui::Frame {
    let t = p.glass();
    egui::Frame::new()
        .fill(t.menu_surface.scaled(0.96))
        .stroke(Stroke::new(1.0, t.border.color()))
        .corner_radius(cr(glass::RADIUS))
        .inner_margin(Margin::same(4))
        .shadow(egui::Shadow { offset: [0, 6], blur: 20, spread: 0, color: Color32::from_black_alpha(if p.dark { 120 } else { 40 }) })
}

/// A dropdown-menu row: optional leading icon, check mark when selected, destructive tint. Hover is
/// a wash with no border (rule 3).
pub fn menu_item(ui: &mut Ui, p: &Palette, label: &str, selected: bool, destructive: bool, icon: Option<char>) -> Response {
    let w = ui.available_width();
    let (rect, resp) = ui.allocate_exact_size(vec2(w, 30.0), Sense::click());
    pointer(&resp);
    let hov = hover_anim(ui, resp.id.with("h"), resp.hovered());
    let painter = ui.painter();
    let tint = if destructive { p.destructive_a(12) } else { p.wash(1) };
    painter.rect_filled(rect, cr(8.0), mix(Color32::TRANSPARENT, tint, hov));
    let col = if destructive { p.destructive } else { p.foreground };
    let mut x = rect.min.x + 10.0;
    if let Some(g) = icon {
        painter.text(pos2(x + 6.5, rect.center().y), Align2::CENTER_CENTER, g.to_string(), fonts::icon(13.0), if destructive { col } else { p.muted_a(90) });
        x += 22.0;
    }
    painter.text(pos2(x, rect.center().y), Align2::LEFT_CENTER, label, if selected { fonts::semibold(13.0) } else { fonts::regular(13.0) }, col);
    if selected {
        painter.text(pos2(rect.max.x - 14.0, rect.center().y), Align2::CENTER_CENTER, icons::CHECK.to_string(), fonts::icon(11.0), p.primary);
    }
    resp
}

// ---------------------------------------------------------------- panels and rows

/// Settings panel: one glass panel (refraction band, tint, rim, shadow) containing transparent
/// rows. Content is drawn "inside glass": rows, chips and buttons paint washes, not surfaces.
pub fn panel<R>(ui: &mut Ui, p: &Palette, add: impl FnOnce(&mut Ui) -> R) -> R {
    let g = gl(ui, p);
    let slot = reserve(ui);
    // Rule 3 / "no glass nested in glass": a panel inside a modal (or any other glass) is only a soft
    // grouping wash; its rows are separated by spacing and dividers.
    let nested = in_glass(ui.ctx());
    let prev = set_in_glass(ui.ctx(), true);
    let out = egui::Frame::new().show(ui, |ui| {
        ui.spacing_mut().item_spacing = vec2(0.0, 0.0);
        ui.set_width(ui.available_width());
        add(ui)
    });
    set_in_glass(ui.ctx(), prev);
    let rect = out.response.rect;
    if nested {
        ui.painter().set(slot, Shape::rect_filled(rect, cr(glass::RADIUS), p.wash(0)));
    } else {
        ui.painter().set(slot, Shape::Vec(glass::panel_shapes(&g, rect, glass::RADIUS, 0.0)));
    }
    out.inner
}

/// Row divider inside a panel: the glass border at 60 %, inset from the rim so it reads as spacing.
pub fn divider(ui: &mut Ui, p: &Palette) {
    let w = ui.available_width();
    let (r, _) = ui.allocate_exact_size(vec2(w, 1.0), Sense::hover());
    ui.painter().rect_filled(Rect::from_min_max(pos2(r.min.x + 16.0, r.min.y), pos2(r.max.x - 16.0, r.max.y)), 0.0, p.divider());
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

/// Small tag. Chip tier on the page; inside a panel a tinted wash (rule 3). Returns its response
/// (for tooltips).
pub fn chip(ui: &mut Ui, p: &Palette, label: &str, kind: ChipKind) -> Response {
    let font = fonts::semibold(11.0);
    let tw = measure(ui, label, font.clone()).x;
    let (rect, resp) = ui.allocate_exact_size(vec2(tw + 14.0, 20.0), Sense::hover());
    let (bg, fg) = match kind {
        ChipKind::Neutral => (p.wash(1), p.muted_fg),
        ChipKind::Primary => (p.primary_a(if p.dark { 18 } else { 13 }), p.primary),
        ChipKind::Success => (p.success_a(if p.dark { 18 } else { 13 }), p.success),
        ChipKind::Warning => (p.warning_a(if p.dark { 20 } else { 15 }), p.warning),
        ChipKind::Destructive => (p.destructive_a(if p.dark { 18 } else { 12 }), p.destructive),
    };
    if !in_glass(ui.ctx()) {
        glass::chip(ui.painter(), &gl(ui, p), rect, 10.0, 0.0);
    }
    ui.painter().rect_filled(rect, cr(10.0), bg);
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

/// Modal tier: `egui::Modal` over `--glass-scrim`, the surface a glass panel with the frost layer
/// at `--glass-radius-lg`. `margin` is the padding inside the surface. The content is drawn inside
/// glass (rule 3). Esc and a click on the scrim close it (`should_close`).
pub fn glass_modal<R>(ctx: &egui::Context, p: &Palette, id: Id, margin: i8, add: impl FnOnce(&mut Ui) -> R) -> egui::ModalResponse<R> {
    glass_modal_at(ctx, p, id, margin, None, add)
}

/// [`glass_modal`] placed by a custom `Area` (e.g. anchored near the top, like the Ctrl+K palette).
pub fn glass_modal_at<R>(
    ctx: &egui::Context,
    p: &Palette,
    id: Id,
    margin: i8,
    area: Option<egui::Area>,
    add: impl FnOnce(&mut Ui) -> R,
) -> egui::ModalResponse<R> {
    let g = Gl::new(ctx, p.dark);
    let mut modal = egui::Modal::new(id).backdrop_color(glass::scrim_color(p.dark)).frame(egui::Frame::NONE);
    if let Some(a) = area {
        modal = modal.area(a);
    }
    modal.show(ctx, |ui| {
        let slot = reserve(ui);
        let prev = set_in_glass(ui.ctx(), true);
        let out = egui::Frame::new().inner_margin(Margin::same(margin)).show(ui, add);
        set_in_glass(ui.ctx(), prev);
        ui.painter().set(slot, Shape::Vec(glass::modal_shapes(&g, out.response.rect, glass::RADIUS_LG)));
        out.inner
    })
}

/// Modal confirmation dialog (`Delete`, `Clear all`, `Reset`). `destructive` colours the action.
pub fn confirm_dialog(ctx: &egui::Context, p: &Palette, id: &str, title: &str, body: &str, action: &str, destructive: bool) -> DialogResult {
    let mut result = DialogResult::Open;
    let modal = glass_modal(ctx, p, Id::new(id), 24, |ui| {
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

    #[test]
    fn the_inside_glass_flag_nests_and_restores() {
        let ctx = egui::Context::default();
        assert!(!in_glass(&ctx));
        let outer = set_in_glass(&ctx, true);
        assert!(!outer && in_glass(&ctx));
        let inner = set_in_glass(&ctx, true);
        assert!(inner, "a nested surface sees that it is already inside glass");
        set_in_glass(&ctx, inner);
        assert!(in_glass(&ctx));
        set_in_glass(&ctx, outer);
        assert!(!in_glass(&ctx));
    }
}
