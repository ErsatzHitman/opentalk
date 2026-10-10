//! History page: dictation log grouped by day, with search, per-row actions and stats.
//! The grouping/filtering/stats are pure functions in `core::histview`.
use super::fonts;
use super::icons;
use super::theme::{Palette, mix};
use super::timefmt;
pub use crate::core::histview::{thousands, visible};
use crate::core::histview::{format_latency, group_by_day, meta_line, stats};
use super::widgets::{self as w, Btn, ChipKind, Variant, cr};
use super::{Cmd, Env};
use crate::core::history::{Entry, Status};
use eframe::egui::{self, Align, Color32, Layout, Margin, Rect, Sense, Shape, Stroke, StrokeKind, Ui, UiBuilder, vec2};
use std::collections::HashSet;

// ---------------------------------------------------------------- UI state + view

#[derive(Default)]
pub struct HistoryState {
    pub entries: Vec<Entry>,
    pub loaded: bool,
    pub query: String,
    pub show_discarded: bool,
    raw_open: HashSet<i64>,
    pub confirm_delete: Option<i64>,
    pub confirm_clear: bool,
    /// Row to scroll to and flash (from the command palette).
    pub highlight: Option<(i64, f64)>,
    scrolled: bool,
}

impl HistoryState {
    pub fn reload(&mut self) {
        self.entries = super::safe(crate::core::history::load_all).unwrap_or_default();
        self.loaded = true;
    }

    pub fn focus(&mut self, id: i64, now: f64) {
        self.highlight = Some((id, now));
        self.scrolled = false;
        if let Some(e) = self.entries.iter().find(|e| e.id == id)
            && e.status == Status::Discarded {
                self.show_discarded = true;
            }
        self.query.clear();
    }

    fn delete(&mut self, id: i64) {
        let _ = super::safe(|| crate::core::history::delete(id));
        self.entries.retain(|e| e.id != id);
    }
}

pub fn show(ui: &mut Ui, env: &mut Env, st: &mut HistoryState) {
    let p = *env.p;
    if !st.loaded {
        st.reload();
    }
    let now_ms = env.now_ms;
    let offset_of = |ts: i64| timefmt::local_offset_secs(ts);
    let idx = visible(&st.entries, &st.query, st.show_discarded);
    let groups = group_by_day(&st.entries, &idx, now_ms, &offset_of);
    let has_history = !st.entries.is_empty();

    egui::ScrollArea::vertical().id_salt("history-scroll").auto_shrink([false, false]).show(ui, |ui| {
        let avail = ui.available_width();
        let content_w = (avail - 48.0).clamp(300.0, 976.0);
        let side = ((avail - content_w) / 2.0).max(0.0);
        ui.horizontal_top(|ui| {
            ui.add_space(side);
            ui.vertical(|ui| {
                ui.set_width(content_w);
                ui.spacing_mut().item_spacing = vec2(0.0, 0.0);
                ui.add_space(16.0);
                header(ui, env, st, has_history);
                ui.add_space(12.0);

                if !env.cfg.history_enabled {
                    banner(ui, &p, "History is turned off. New dictations are not saved. Turn it back on in Settings > Privacy & Data.");
                    ui.add_space(12.0);
                }

                if st.entries.is_empty() {
                    empty_state(ui, env);
                } else if groups.is_empty() {
                    ui.add_space(40.0);
                    ui.vertical_centered(|ui| {
                        w::text(ui, "No dictations match your search.", 13.0, p.muted_a(80));
                    });
                } else {
                    for (gi, g) in groups.iter().enumerate() {
                        if gi > 0 {
                            ui.add_space(24.0);
                        }
                        group_header(ui, &p, st, &g.label, gi == 0);
                        let fill = p.panel_fill();
                        egui::Frame::new()
                            .fill(fill)
                            .stroke(Stroke::new(1.0, p.panel_border()))
                            .corner_radius(cr(16.0))
                            .show(ui, |ui| {
                                ui.set_width(content_w - 2.0);
                                for (k, &i) in g.items.iter().enumerate() {
                                    if k > 0 {
                                        w::divider(ui, &p);
                                    }
                                    row(ui, env, st, i, &offset_of, k == 0, k + 1 == g.items.len());
                                }
                            });
                    }
                }
                ui.add_space(32.0);
            });
        });
    });

    dialogs(ui.ctx(), env, st);
}

fn banner(ui: &mut Ui, p: &Palette, msg: &str) {
    egui::Frame::new()
        .fill(p.warning_a(if p.dark { 10 } else { 6 }))
        .stroke(Stroke::new(1.0, p.warning_a(30)))
        .corner_radius(cr(8.0))
        .inner_margin(Margin::symmetric(14, 10))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                w::text(ui, "\u{2298}", 14.0, p.warning);
                w::text(ui, msg, 12.0, if p.dark { w::with_alpha_mul(p.warning, 0.95) } else { mix(p.warning, Color32::BLACK, 0.25) });
            });
        });
}

fn header(ui: &mut Ui, env: &mut Env, st: &mut HistoryState, has_history: bool) {
    let p = *env.p;
    let title = if has_history { "Welcome back" } else { "Welcome" };
    w::text(ui, title, 20.0, p.foreground);
    if has_history {
        ui.add_space(6.0);
        let s = stats(&st.entries);
        let mut line = format!("{} dictations · {} words", thousands(s.dictations), thousands(s.words));
        if let Some(l) = s.avg_latency_ms {
            line.push_str(&format!(" · avg {}", format_latency(l)));
        }
        let failed = st.entries.iter().filter(|e| e.status == Status::Failed).count();
        if failed > 0 {
            line.push_str(&format!(" · {failed} failed"));
        }
        ui.horizontal(|ui| {
            w::text(ui, line, 12.5, p.muted_a(85));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                search_box(ui, &p, st);
            });
        });
    }
}

fn search_box(ui: &mut Ui, p: &Palette, st: &mut HistoryState) {
    let width = 240.0;
    let mut q = std::mem::take(&mut st.query);
    let (rect, _) = ui.allocate_exact_size(vec2(width, 32.0), Sense::hover());
    ui.scope_builder(UiBuilder::new().max_rect(rect), |ui| {
        w::text_input_h(ui, p, &mut q, "Search history", width, 32.0, false, true);
    });
    if !q.is_empty() {
        let r = Rect::from_center_size(egui::pos2(rect.max.x - 16.0, rect.center().y), vec2(20.0, 20.0));
        let resp = ui.interact(r, ui.id().with("hist-clear-q"), Sense::click());
        w::pointer(&resp);
        w::paint_icon(ui, r, icons::CLOSE, 10.0, if resp.hovered() { p.foreground } else { p.muted_a(70) });
        if resp.clicked() {
            q.clear();
        }
    } else {
        let r = Rect::from_center_size(egui::pos2(rect.max.x - 16.0, rect.center().y), vec2(20.0, 20.0));
        w::paint_icon(ui, r, icons::SEARCH, 11.0, p.muted_a(60));
    }
    st.query = q;
}

fn group_header(ui: &mut Ui, p: &Palette, st: &mut HistoryState, label: &str, first: bool) {
    ui.horizontal(|ui| {
        ui.set_height(30.0);
        w::text(ui, label, 13.0, p.muted_fg);
        if first {
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                let r = small_link(ui, p, icons::DELETE, "Clear all", true);
                if r.clicked() {
                    st.confirm_clear = true;
                }
                let lbl = if st.show_discarded { "Hide discarded" } else { "Show discarded" };
                if small_link(ui, p, icons::ARCHIVE, lbl, false).clicked() {
                    st.show_discarded = !st.show_discarded;
                }
            });
        }
    });
    ui.add_space(4.0);
}

/// Tiny icon+label text button (`text-[11px]`), destructive on hover when asked.
fn small_link(ui: &mut Ui, p: &Palette, glyph: char, label: &str, destructive: bool) -> egui::Response {
    let font = fonts::regular(11.0);
    let tw = w::measure(ui, label, font.clone()).x;
    let (rect, resp) = ui.allocate_exact_size(vec2(tw + 28.0, 22.0), Sense::click());
    w::pointer(&resp);
    let hov = w::hover_anim(ui, resp.id.with("h"), resp.hovered());
    let (fg, bg) = if destructive { (p.destructive, p.destructive_a(8)) } else { (p.foreground, p.overlay(5)) };
    ui.painter().rect_filled(rect, cr(4.0), mix(Color32::TRANSPARENT, bg, hov));
    let col = mix(p.muted_a(70), fg, hov);
    ui.painter().text(egui::pos2(rect.min.x + 10.0, rect.center().y), egui::Align2::CENTER_CENTER, glyph.to_string(), fonts::icon(10.0), col);
    ui.painter().text(egui::pos2(rect.min.x + 20.0, rect.center().y), egui::Align2::LEFT_CENTER, label, font, col);
    resp
}

fn empty_state(ui: &mut Ui, env: &mut Env) {
    let p = *env.p;
    ui.add_space(48.0);
    ui.vertical_centered(|ui| {
        let (rect, _) = ui.allocate_exact_size(vec2(64.0, 64.0), Sense::hover());
        ui.painter().circle_filled(rect.center(), 32.0, p.primary_a(if p.dark { 14 } else { 9 }));
        ui.painter().circle_stroke(rect.center(), 32.0, Stroke::new(1.0, p.primary_a(25)));
        w::paint_icon(ui, rect, icons::MIC, 26.0, p.primary);
        ui.add_space(20.0);
        w::text_semi(ui, "Start your first dictation", 18.0, p.foreground);
        ui.add_space(8.0);
        w::text(ui, "Press your hotkey, speak, and the text appears wherever your cursor is.", 13.5, p.muted_a(85));
        ui.add_space(14.0);
        ui.horizontal(|ui| {
            let parts = super::hotkey_capture::parts(&env.cfg.hotkey);
            let est = parts.iter().map(|k| w::measure(ui, k, fonts::semibold(11.0)).x + 24.0).sum::<f32>() + 150.0;
            ui.add_space(((ui.available_width() - est) / 2.0).max(0.0));
            w::text(ui, "Press", 13.0, p.muted_a(85));
            for (i, k) in parts.iter().enumerate() {
                if i > 0 {
                    w::text(ui, "+", 12.0, p.muted_a(60));
                }
                keycap(ui, &p, k);
            }
            w::text(ui, "to start and stop.", 13.0, p.muted_a(85));
        });
        ui.add_space(16.0);
        if w::button(ui, &p, Btn::new("Open settings", Variant::Outline).icon(icons::SETTINGS).small()).clicked() {
            env.cmds.push(Cmd::OpenSettings(super::settings::Section::Hotkeys));
        }
    });
}

pub fn keycap(ui: &mut Ui, p: &Palette, label: &str) {
    let font = fonts::semibold(11.5);
    let tw = w::measure(ui, label, font.clone()).x;
    let (rect, _) = ui.allocate_exact_size(vec2(tw + 16.0, 24.0), Sense::hover());
    ui.painter().rect_filled(rect, cr(6.0), if p.dark { p.surface3 } else { p.muted });
    ui.painter().rect_stroke(rect, cr(6.0), Stroke::new(1.0, p.border_hover), StrokeKind::Inside);
    ui.painter().text(rect.center(), egui::Align2::CENTER_CENTER, label, font, p.foreground);
}

fn row(ui: &mut Ui, env: &mut Env, st: &mut HistoryState, i: usize, offset_of: &dyn Fn(i64) -> i64, first: bool, last: bool) {
    let p = *env.p;
    let e = st.entries[i].clone();
    let id = ui.id().with(("hrow", e.id));
    let prev: Option<Rect> = ui.data(|d| d.get_temp(id));
    let hot = prev.map(|r| ui.rect_contains_pointer(r)).unwrap_or(false);
    let menu_open = egui::Popup::is_id_open(ui.ctx(), id.with("menu"));
    let actions_on = ui.ctx().animate_bool_with_time(id.with("act"), hot || menu_open, 0.12);
    let failed = e.status == Status::Failed;
    let discarded = e.status == Status::Discarded;
    let flash = match st.highlight {
        Some((hid, t0)) if hid == e.id => {
            let age = ui.input(|i| i.time) - t0;
            if age < 2.2 {
                ui.ctx().request_repaint();
                (1.0 - (age / 2.2) as f32).clamp(0.0, 1.0)
            } else {
                0.0
            }
        }
        _ => 0.0,
    };

    let bg = ui.painter().add(Shape::Noop);
    let resp = egui::Frame::new().inner_margin(Margin::symmetric(16, 12)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.spacing_mut().item_spacing = vec2(8.0, 6.0);
        if discarded {
            ui.set_opacity(0.8);
        }
        ui.horizontal(|ui| {
            ui.set_min_height(28.0);
            w::text(ui, timefmt::clock(e.ts, offset_of(e.ts)), 12.0, p.muted_fg);
            if let Some(app) = e.app_name.as_deref().filter(|a| !a.is_empty()) {
                w::avatar(ui, &p, app, 16.0);
                w::text(ui, app, 12.0, p.muted_a(85));
            }
            let meta = meta_line(&e);
            if !meta.is_empty() {
                w::text(ui, "·", 12.0, p.muted_a(50));
                w::text(ui, meta, 11.5, p.muted_a(65));
            }
            if e.ai_cleaned {
                w::chip(ui, &p, "AI cleaned", ChipKind::Primary);
            }
            if failed {
                w::chip(ui, &p, "Failed", ChipKind::Destructive);
            }
            if discarded {
                w::chip(ui, &p, "Discarded", ChipKind::Neutral);
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = 4.0;
                ui.scope(|ui| {
                    ui.set_opacity(if failed || discarded { 1.0 } else { actions_on });
                    let more = w::icon_button(ui, &p, icons::MORE, 28.0, 14.0, p.foreground, p.fg_a(6));
                    row_menu(ui, env, st, &e, i, id, &more);
                    if !failed && !discarded && w::ghost_icon(ui, &p, icons::COPY).on_hover_text("Copy text").clicked() {
                        env.cmds.push(Cmd::Copy(e.text.clone()));
                    }
                });
            });
        });
        if failed {
            let msg = e.error.clone().unwrap_or_else(|| "Transcription failed.".into());
            w::text(ui, msg, 13.0, p.destructive);
        } else if !e.text.is_empty() {
            ui.add(egui::Label::new(egui::RichText::new(&e.text).font(fonts::regular(13.5)).color(p.foreground)).selectable(true));
        } else {
            w::text(ui, "(empty)", 13.5, p.muted_a(60));
        }
        if st.raw_open.contains(&e.id) {
            egui::Frame::new().fill(if p.dark { p.surface1 } else { p.muted }).corner_radius(cr(8.0)).inner_margin(Margin::symmetric(12, 8)).show(ui, |ui| {
                ui.set_width(ui.available_width());
                w::text_semi(ui, "Raw transcript", 11.0, p.muted_a(75));
                ui.add_space(2.0);
                w::text(ui, if e.raw.is_empty() { "(not recorded)" } else { &e.raw }, 12.5, p.muted_a(90));
            });
        }
    });
    let rect = resp.response.rect;
    ui.data_mut(|d| d.insert_temp(id, rect));
    let hot_now = ui.rect_contains_pointer(rect);
    let tint = if failed {
        p.destructive_a(5)
    } else if discarded {
        w::with_alpha_mul(p.muted, 0.5)
    } else if hot_now {
        if p.dark { Color32::from_white_alpha(5) } else { w::with_alpha_mul(p.muted, 0.4) }
    } else {
        Color32::TRANSPARENT
    };
    let rounding = egui::CornerRadius { nw: if first { 15 } else { 0 }, ne: if first { 15 } else { 0 }, sw: if last { 15 } else { 0 }, se: if last { 15 } else { 0 } };
    ui.painter().set(bg, Shape::rect_filled(rect, rounding, tint));
    if flash > 0.0 {
        ui.painter().rect_filled(rect, rounding, p.primary_a((flash * 18.0) as u8));
    }
    if matches!(st.highlight, Some((hid, _)) if hid == e.id) && !st.scrolled {
        ui.scroll_to_rect(rect, Some(Align::Center));
        st.scrolled = true;
    }
    if hot != hot_now && prev.is_some() {
        ui.ctx().request_repaint();
    }
}

fn row_menu(ui: &mut Ui, env: &mut Env, st: &mut HistoryState, e: &Entry, idx: usize, id: egui::Id, anchor: &egui::Response) {
    let p = *env.p;
    let can_retry = e.status != Status::Ok && idx == 0 && env.cfg.keep_failed_audio;
    egui::Popup::from_toggle_button_response(anchor)
        .id(id.with("menu"))
        .kind(egui::PopupKind::Menu)
        .close_behavior(egui::PopupCloseBehavior::CloseOnClick)
        .align(egui::RectAlign::BOTTOM_END)
        .gap(4.0)
        .frame(w::popup_frame(&p))
        .show(|ui| {
            ui.set_min_width(210.0);
            ui.spacing_mut().item_spacing = vec2(0.0, 1.0);
            if can_retry && w::menu_item(ui, &p, if e.status == Status::Discarded { "Recover recording" } else { "Retry transcription" }, false, false, Some(icons::RETRY)).clicked() {
                env.cmds.push(Cmd::RetryLast);
            }
            if e.status == Status::Ok {
                let open = st.raw_open.contains(&e.id);
                let lbl = if open { "Hide raw transcript" } else { "View raw transcript" };
                if w::menu_item(ui, &p, lbl, false, false, Some(icons::DOC)).clicked() {
                    if open {
                        st.raw_open.remove(&e.id);
                    } else {
                        st.raw_open.insert(e.id);
                    }
                }
            }
            if !e.text.is_empty() && w::menu_item(ui, &p, "Copy text", false, false, Some(icons::COPY)).clicked() {
                env.cmds.push(Cmd::Copy(e.text.clone()));
            }
            if w::menu_item(ui, &p, "Delete", false, true, Some(icons::DELETE)).clicked() {
                st.confirm_delete = Some(e.id);
            }
        });
    let _ = ui;
}

fn dialogs(ctx: &egui::Context, env: &mut Env, st: &mut HistoryState) {
    let p = *env.p;
    if let Some(id) = st.confirm_delete {
        match w::confirm_dialog(ctx, &p, "confirm-delete-entry", "Delete this dictation?", "It will be removed from your history. This cannot be undone.", "Delete", true) {
            w::DialogResult::Confirm => {
                st.delete(id);
                st.confirm_delete = None;
                env.cmds.push(Cmd::Toast("Dictation deleted".into()));
            }
            w::DialogResult::Cancel => st.confirm_delete = None,
            w::DialogResult::Open => {}
        }
    }
    if st.confirm_clear {
        match w::confirm_dialog(ctx, &p, "confirm-clear-history", "Clear all history?", "Every saved dictation will be deleted. This cannot be undone.", "Clear all", true) {
            w::DialogResult::Confirm => {
                let _ = super::safe(crate::core::history::clear);
                st.entries.clear();
                st.confirm_clear = false;
                env.cmds.push(Cmd::Toast("History cleared".into()));
            }
            w::DialogResult::Cancel => st.confirm_clear = false,
            w::DialogResult::Open => {}
        }
    }
}
