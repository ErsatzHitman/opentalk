//! The eframe application: frameless window shell, sidebar, top bar, page routing, toasts.
use super::dictionary::{self, DictState};
use super::history::{self, HistoryState};
use super::palette::{self, Item};
use super::settings::{self, Section, SettingsState};
use super::theme::{self, Palette, mix};
use super::widgets::{self as w, cr};
use super::{Cmd, Env, Launch, Page, fonts, icons};
use crate::core::config::Config;
use crate::core::ipc::{self, IpcMsg};
use eframe::egui::{
    self, Align2, Color32, CursorIcon, Id, Key, Modifiers, Rect, ResizeDirection, Sense, Stroke, StrokeKind, Ui, UiBuilder, ViewportCommand, pos2, vec2,
};

pub const SIDEBAR_W: f32 = 192.0;
pub const TOPBAR_H: f32 = 48.0;
const RESIZE_BORDER: f32 = 6.0;

pub fn native_options() -> eframe::NativeOptions {
    let viewport = egui::ViewportBuilder::default()
        .with_title("OpenTalk")
        .with_app_id("OpenTalk")
        .with_inner_size([1200.0, 800.0])
        .with_min_inner_size([760.0, 520.0])
        .with_decorations(false)
        .with_icon(std::sync::Arc::new(app_icon()));
    eframe::NativeOptions { viewport, centered: true, ..Default::default() }
}

pub struct App {
    cfg: Config,
    page: Page,
    hist: HistoryState,
    dict: DictState,
    settings_open: bool,
    settings: SettingsState,
    palette: palette::State,
    toasts: Vec<(String, f64)>,
    cmds: Vec<Cmd>,
    sys_dark: bool,
    applied_dark: Option<bool>,
    was_focused: bool,
    support_open_id: Id,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>, launch: Launch) -> Self {
        fonts::install(&cc.egui_ctx);
        let cfg = Config::load();
        let mut app = App {
            cfg,
            page: launch.page,
            hist: HistoryState::default(),
            dict: DictState::default(),
            settings_open: launch.settings.is_some(),
            settings: SettingsState::default(),
            palette: palette::State::default(),
            toasts: Vec::new(),
            cmds: Vec::new(),
            sys_dark: !theme::system_apps_use_light_theme(),
            applied_dark: None,
            was_focused: true,
            support_open_id: Id::new("support-menu"),
        };
        if let Some(s) = launch.settings {
            app.settings.section = s;
        }
        app
    }

    fn dark(&self) -> bool {
        match self.cfg.theme {
            crate::core::config::Theme::Auto => self.sys_dark,
            t => theme::resolve_dark(t),
        }
    }

    /// Persist `config.json` and tell the daemon.
    fn save(&mut self) {
        if let Err(e) = self.cfg.save() {
            crate::log_warn!("config save failed: {e}");
            self.toast(format!("Could not save settings: {e}"));
            return;
        }
        let _ = super::safe(|| ipc::notify_daemon(IpcMsg::Reload));
    }

    fn toast(&mut self, msg: impl Into<String>) {
        self.cmds.push(Cmd::Toast(msg.into()));
    }

    fn handle_cmds(&mut self, ctx: &egui::Context) {
        let now = ctx.input(|i| i.time);
        let cmds = std::mem::take(&mut self.cmds);
        for c in cmds {
            match c {
                Cmd::Toast(m) => self.toasts.push((m, now)),
                Cmd::Copy(t) => {
                    ctx.copy_text(t);
                    self.toasts.push(("Copied to clipboard".into(), now));
                }
                Cmd::OpenSettings(s) => {
                    self.settings.section = s;
                    self.settings_open = true;
                }
                Cmd::RetryLast => {
                    let sent = super::safe(|| ipc::notify_daemon(IpcMsg::RetryLast)).unwrap_or(false);
                    self.toasts.push((if sent { "Retrying the last dictation".into() } else { "OpenTalk is not running in the background".into() }, now));
                }
                Cmd::ReloadHistory => self.hist.reload(),
            }
        }
    }
}

impl eframe::App for App {
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        let c = Palette::for_dark(self.dark()).window;
        [c.r() as f32 / 255.0, c.g() as f32 / 255.0, c.b() as f32 / 255.0, 1.0]
    }

    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();

        // Follow Windows when Auto: re-read the registry whenever the window regains focus.
        let focused = ctx.input(|i| i.viewport().focused).unwrap_or(true);
        if focused && !self.was_focused {
            self.sys_dark = !theme::system_apps_use_light_theme();
            self.hist.reload();
        }
        self.was_focused = focused;

        let dark = self.dark();
        let p = Palette::for_dark(dark);
        if self.applied_dark != Some(dark) {
            ctx.set_theme(if dark { egui::Theme::Dark } else { egui::Theme::Light });
            theme::apply(&ctx, &p);
            self.applied_dark = Some(dark);
        }

        // Keyboard shortcuts.
        if ctx.input_mut(|i| i.consume_key(Modifiers::COMMAND, Key::K)) {
            self.palette.toggle();
        }
        if ctx.input_mut(|i| i.consume_key(Modifiers::COMMAND, Key::Comma)) {
            self.settings_open = true;
        }

        let screen = ui.max_rect();
        ui.painter().rect_filled(screen, 0.0, p.window);
        let maximized = ctx.input(|i| i.viewport().maximized).unwrap_or(false);
        self.window_resize_handles(&ctx, screen, maximized);

        let side = Rect::from_min_max(screen.min, pos2(screen.min.x + SIDEBAR_W, screen.max.y));
        let main = Rect::from_min_max(pos2(screen.min.x + SIDEBAR_W, screen.min.y), screen.max).shrink(8.0);
        self.sidebar(ui, &p, side);
        self.main_card(ui, &ctx, &p, main, maximized);

        // Overlays: settings modal, palette, toasts.
        let now_ms = crate::core::timing::unix_ms();
        let mut cmds = std::mem::take(&mut self.cmds);
        if self.settings_open {
            let mut env = Env { p: &p, cfg: &mut self.cfg, cmds: &mut cmds, now_ms };
            let out = settings::show(&ctx, &mut env, &mut self.settings);
            if out.changed {
                // Theme may have changed: re-evaluate next frame.
                self.applied_dark = None;
                self.save_deferred(&mut cmds);
            }
            if out.close {
                self.settings_open = false;
                self.settings.on_close(&mut cmds);
            }
        }
        self.cmds = cmds;
        if let Some(item) = palette::show(&ctx, &p, &mut self.palette, &self.hist.entries, now_ms) {
            match item {
                Item::Page(pg) => self.page = pg,
                Item::Settings(s) => {
                    self.settings.section = s;
                    self.settings_open = true;
                }
                Item::Entry(id) => {
                    self.page = Page::History;
                    let t = ctx.input(|i| i.time);
                    self.hist.focus(id, t);
                }
            }
        }
        self.handle_cmds(&ctx);
        self.paint_toasts(&ctx, &p);
    }
}

impl App {
    /// Settings changed inside the modal: write the file and notify the daemon.
    fn save_deferred(&mut self, _cmds: &mut Vec<Cmd>) {
        self.save();
    }

    fn window_resize_handles(&self, ctx: &egui::Context, screen: Rect, maximized: bool) {
        if maximized {
            return;
        }
        let Some(pos) = ctx.input(|i| i.pointer.hover_pos()) else { return };
        if !screen.contains(pos) {
            return;
        }
        let b = RESIZE_BORDER;
        let left = pos.x < screen.min.x + b;
        let right = pos.x > screen.max.x - b;
        let top = pos.y < screen.min.y + b;
        let bottom = pos.y > screen.max.y - b;
        let dir = match (left, right, top, bottom) {
            (true, _, true, _) => Some((ResizeDirection::NorthWest, CursorIcon::ResizeNwSe)),
            (_, true, true, _) => Some((ResizeDirection::NorthEast, CursorIcon::ResizeNeSw)),
            (true, _, _, true) => Some((ResizeDirection::SouthWest, CursorIcon::ResizeNeSw)),
            (_, true, _, true) => Some((ResizeDirection::SouthEast, CursorIcon::ResizeNwSe)),
            (true, ..) => Some((ResizeDirection::West, CursorIcon::ResizeHorizontal)),
            (_, true, ..) => Some((ResizeDirection::East, CursorIcon::ResizeHorizontal)),
            (_, _, true, _) => Some((ResizeDirection::North, CursorIcon::ResizeVertical)),
            (_, _, _, true) => Some((ResizeDirection::South, CursorIcon::ResizeVertical)),
            _ => None,
        };
        if let Some((d, cursor)) = dir {
            ctx.set_cursor_icon(cursor);
            if ctx.input(|i| i.pointer.primary_pressed()) {
                ctx.send_viewport_cmd(ViewportCommand::BeginResize(d));
            }
        }
    }

    fn drag_region(&self, ui: &mut Ui, rect: Rect, id: &str) {
        let resp = ui.interact(rect, Id::new(id), Sense::click_and_drag());
        if resp.drag_started_by(egui::PointerButton::Primary) {
            ui.ctx().send_viewport_cmd(ViewportCommand::StartDrag);
        }
        if resp.double_clicked() {
            let max = ui.ctx().input(|i| i.viewport().maximized).unwrap_or(false);
            ui.ctx().send_viewport_cmd(ViewportCommand::Maximized(!max));
        }
    }

    // ------------------------------------------------------------ sidebar

    fn sidebar(&mut self, ui: &mut Ui, p: &Palette, rect: Rect) {
        self.drag_region(ui, Rect::from_min_size(rect.min, vec2(rect.width(), 40.0)), "sidebar-drag");
        let mut y = rect.min.y + 40.0 + 8.0;
        let items = [(Page::History, "History", icons::HISTORY), (Page::Dictionary, "Dictionary", icons::BOOK)];
        for (page, label, glyph) in items {
            let r = Rect::from_min_size(pos2(rect.min.x + 8.0, y), vec2(rect.width() - 16.0, 32.0));
            if nav_row(ui, p, r, glyph, label, self.page == page && !self.settings_open) {
                self.page = page;
            }
            y += 34.0;
        }

        // Bottom rows: Settings, Support, divider, app identity.
        let bottom = rect.max.y - 8.0;
        let ident = Rect::from_min_size(pos2(rect.min.x + 8.0, bottom - 40.0), vec2(rect.width() - 16.0, 40.0));
        let div_y = ident.min.y - 6.0;
        let support = Rect::from_min_size(pos2(rect.min.x + 8.0, div_y - 6.0 - 32.0), vec2(rect.width() - 16.0, 32.0));
        let settings_r = Rect::from_min_size(pos2(rect.min.x + 8.0, support.min.y - 2.0 - 32.0), vec2(rect.width() - 16.0, 32.0));
        if nav_row(ui, p, settings_r, icons::SETTINGS, "Settings", self.settings_open) {
            self.settings_open = true;
        }
        let sup_resp = nav_row_resp(ui, p, support, icons::HELP, "Support", false);
        self.support_menu(&sup_resp, p);
        ui.painter().line_segment([pos2(rect.min.x + 12.0, div_y), pos2(rect.max.x - 12.0, div_y)], Stroke::new(1.0, if p.dark { Color32::from_white_alpha(15) } else { p.border_a(60) }));
        // Identity.
        draw_logo(ui, pos2(ident.min.x + 22.0, ident.center().y), 11.0, p);
        ui.painter().text(pos2(ident.min.x + 42.0, ident.center().y - 7.0), Align2::LEFT_CENTER, "OpenTalk", fonts::semibold(12.5), p.fg_a(85));
        ui.painter().text(pos2(ident.min.x + 42.0, ident.center().y + 8.0), Align2::LEFT_CENTER, format!("v{}", env!("CARGO_PKG_VERSION")), fonts::regular(11.0), p.fg_a(50));
    }

    fn support_menu(&mut self, anchor: &egui::Response, p: &Palette) {
        let _ = self.support_open_id;
        let mut open_settings: Option<Section> = None;
        let mut folder: Option<std::path::PathBuf> = None;
        egui::Popup::from_toggle_button_response(anchor)
            .kind(egui::PopupKind::Menu)
            .close_behavior(egui::PopupCloseBehavior::CloseOnClick)
            .align(egui::RectAlign::TOP_START)
            .gap(6.0)
            .frame(w::popup_frame(p))
            .show(|ui| {
                ui.set_min_width(210.0);
                ui.spacing_mut().item_spacing = vec2(0.0, 1.0);
                if w::menu_item(ui, p, "Keyboard shortcuts", false, false, Some(icons::KEYBOARD)).clicked() {
                    open_settings = Some(Section::Hotkeys);
                }
                if w::menu_item(ui, p, "Speech provider setup", false, false, Some(icons::MIC)).clicked() {
                    open_settings = Some(Section::SpeechToText);
                }
                if w::menu_item(ui, p, "Open logs folder", false, false, Some(icons::FOLDER)).clicked() {
                    folder = Some(crate::core::paths::log_dir());
                }
                if w::menu_item(ui, p, "About OpenTalk", false, false, Some(icons::INFO)).clicked() {
                    open_settings = Some(Section::System);
                }
            });
        if let Some(s) = open_settings {
            self.cmds.push(Cmd::OpenSettings(s));
        }
        if let Some(f) = folder {
            w::shell_open(&f.display().to_string());
        }
    }

    // ------------------------------------------------------------ main card

    fn main_card(&mut self, ui: &mut Ui, ctx: &egui::Context, p: &Palette, rect: Rect, maximized: bool) {
        let radius = 12.0;
        let painter = ui.painter();
        painter.rect_filled(rect, cr(radius), p.background);
        let top = Rect::from_min_size(rect.min, vec2(rect.width(), TOPBAR_H));
        let body = Rect::from_min_max(pos2(rect.min.x, rect.min.y + TOPBAR_H), rect.max);

        // Top bar: drag region first so the controls on top win the hit test.
        self.drag_region(ui, top, "topbar-drag");
        ui.painter().line_segment(
            [pos2(top.min.x, top.max.y - 0.5), pos2(top.max.x, top.max.y - 0.5)],
            Stroke::new(1.0, if p.dark { Color32::from_white_alpha(25) } else { p.border }),
        );
        let title = match self.page {
            Page::History => "History",
            Page::Dictionary => "Dictionary",
        };
        ui.painter().text(pos2(top.min.x + 16.0, top.center().y), Align2::LEFT_CENTER, title, fonts::semibold(14.0), p.foreground);
        // Search pill, centred in the bar.
        let pill_w = 340.0f32.min((top.width() - 2.0 * 190.0).max(180.0));
        let pill = Rect::from_center_size(pos2(top.center().x, top.center().y), vec2(pill_w, 32.0));
        let sresp = ui.interact(pill, Id::new("topbar-search"), Sense::click());
        w::pointer(&sresp);
        let hov = w::hover_anim(ui, sresp.id.with("h"), sresp.hovered());
        ui.painter().rect_filled(pill, cr(16.0), mix(if p.dark { Color32::from_white_alpha(13) } else { p.fg_a(4) }, if p.dark { Color32::from_white_alpha(20) } else { p.fg_a(6) }, hov));
        ui.painter().rect_stroke(pill, cr(16.0), Stroke::new(1.0, if p.dark { Color32::from_white_alpha(25) } else { p.border }), StrokeKind::Inside);
        w::paint_icon(ui, Rect::from_center_size(pos2(pill.min.x + 20.0, pill.center().y), vec2(16.0, 16.0)), icons::SEARCH, 13.0, p.muted_a(70));
        ui.painter().text(pos2(pill.min.x + 36.0, pill.center().y), Align2::LEFT_CENTER, "Search", fonts::regular(13.0), p.muted_a(70));
        {
            let font = fonts::semibold(10.0);
            let label = "Ctrl + K";
            let tw = w::measure(ui, label, font.clone()).x;
            let kr = Rect::from_center_size(pos2(pill.max.x - 14.0 - (tw + 12.0) / 2.0, pill.center().y), vec2(tw + 12.0, 18.0));
            ui.painter().rect_filled(kr, cr(9.0), if p.dark { Color32::from_white_alpha(20) } else { p.fg_a(6) });
            ui.painter().text(kr.center(), Align2::CENTER_CENTER, label, font, p.muted_a(70));
        }
        if sresp.clicked() {
            self.palette.toggle();
        }
        // Window controls.
        self.window_controls(ui, ctx, p, top, maximized);

        // Body (clipped to the rounded card).
        let mut body_ui = ui.new_child(UiBuilder::new().max_rect(body).layout(egui::Layout::top_down(egui::Align::Min)));
        body_ui.set_clip_rect(body.intersect(ui.clip_rect()));
        let now_ms = crate::core::timing::unix_ms();
        let mut cmds = std::mem::take(&mut self.cmds);
        let mut changed = false;
        {
            let mut env = Env { p, cfg: &mut self.cfg, cmds: &mut cmds, now_ms };
            match self.page {
                Page::History => history::show(&mut body_ui, &mut env, &mut self.hist),
                Page::Dictionary => changed |= dictionary::show(&mut body_ui, &mut env, &mut self.dict),
            }
        }
        self.cmds = cmds;
        // Card border on top so scrolled content never covers it.
        ui.painter().rect_stroke(rect, cr(radius), Stroke::new(1.0, if p.dark { Color32::from_white_alpha(25) } else { p.border }), StrokeKind::Inside);
        if changed {
            self.save();
        }
    }

    fn window_controls(&mut self, ui: &mut Ui, ctx: &egui::Context, p: &Palette, top: Rect, maximized: bool) {
        let bw = 40.0;
        let h = 32.0;
        let y = top.center().y - h / 2.0;
        let mut x = top.max.x - 8.0 - bw * 3.0;
        for (i, glyph) in [icons::MIN, if maximized { icons::RESTORE } else { icons::MAX }, icons::WIN_CLOSE].into_iter().enumerate() {
            let r = Rect::from_min_size(pos2(x, y), vec2(bw, h));
            let resp = ui.interact(r, Id::new(("winctl", i)), Sense::click());
            w::pointer(&resp);
            let hov = w::hover_anim(ui, resp.id.with("h"), resp.hovered());
            let close = i == 2;
            let bg = if close { mix(Color32::TRANSPARENT, Color32::from_rgb(0xe8, 0x11, 0x23), hov) } else { mix(Color32::TRANSPARENT, p.overlay(8), hov) };
            ui.painter().rect_filled(r, cr(if close { 8.0 } else { 6.0 }), bg);
            let fg = if close { mix(p.muted_a(85), Color32::WHITE, hov) } else { mix(p.muted_a(85), p.foreground, hov) };
            w::paint_icon(ui, r, glyph, 10.0, fg);
            if resp.clicked() {
                match i {
                    0 => ctx.send_viewport_cmd(ViewportCommand::Minimized(true)),
                    1 => ctx.send_viewport_cmd(ViewportCommand::Maximized(!maximized)),
                    _ => ctx.send_viewport_cmd(ViewportCommand::Close),
                }
            }
            x += bw;
        }
    }

    fn paint_toasts(&mut self, ctx: &egui::Context, p: &Palette) {
        let now = ctx.input(|i| i.time);
        const LIFE: f64 = 2.6;
        self.toasts.retain(|(_, t0)| now - t0 < LIFE);
        if self.toasts.is_empty() {
            return;
        }
        let screen = ctx.content_rect();
        let mut y = screen.max.y - 28.0;
        for (i, (msg, t0)) in self.toasts.iter().enumerate().rev() {
            let age = now - t0;
            let a = (((LIFE - age) / 0.3).min(1.0).min(age / 0.12 + 0.01)).clamp(0.0, 1.0) as f32;
            egui::Area::new(Id::new(("toast", i, (*t0 * 1000.0) as i64)))
                .order(egui::Order::Tooltip)
                .interactable(false)
                .fixed_pos(pos2(screen.center().x, y))
                .pivot(Align2::CENTER_BOTTOM)
                .show(ctx, |ui| {
                    ui.set_opacity(a);
                    egui::Frame::new()
                        .fill(if p.dark { p.raised } else { Color32::from_rgb(0x1f, 0x1f, 0x1f) })
                        .stroke(Stroke::new(1.0, if p.dark { Color32::from_white_alpha(25) } else { Color32::TRANSPARENT }))
                        .corner_radius(cr(18.0))
                        .inner_margin(egui::Margin::symmetric(16, 9))
                        .shadow(egui::Shadow { offset: [0, 6], blur: 18, spread: 0, color: Color32::from_black_alpha(70) })
                        .show(ui, |ui| {
                            ui.label(egui::RichText::new(msg).font(fonts::regular(13.0)).color(if p.dark { p.foreground } else { Color32::WHITE }));
                        });
                });
            y -= 44.0;
        }
        ctx.request_repaint_after(std::time::Duration::from_millis(60));
    }
}

fn nav_row(ui: &mut Ui, p: &Palette, rect: Rect, glyph: char, label: &str, active: bool) -> bool {
    nav_row_resp(ui, p, rect, glyph, label, active).clicked()
}

/// Sidebar row (`h-8 px-2.5 rounded-md`, active `bg-primary/8`).
fn nav_row_resp(ui: &mut Ui, p: &Palette, rect: Rect, glyph: char, label: &str, active: bool) -> egui::Response {
    let resp = ui.interact(rect, Id::new(("nav", label)), Sense::click());
    w::pointer(&resp);
    let hov = w::hover_anim(ui, resp.id.with("h"), resp.hovered() && !active);
    let t = ui.ctx().animate_bool_with_time(resp.id.with("a"), active, 0.12);
    let painter = ui.painter();
    let hover_bg = if p.dark { Color32::from_white_alpha(10) } else { p.fg_a(4) };
    painter.rect_filled(rect, cr(6.0), mix(mix(Color32::TRANSPARENT, hover_bg, hov), p.primary_a(if p.dark { 10 } else { 8 }), t));
    let ic = mix(mix(p.fg_a(70), p.fg_a(90), hov), p.primary, t);
    painter.text(pos2(rect.min.x + 10.0 + 8.0, rect.center().y), Align2::CENTER_CENTER, glyph.to_string(), fonts::icon(15.0), ic);
    let font = if active { fonts::semibold(13.0) } else { fonts::regular(13.0) };
    let col = mix(mix(p.fg_a(88), p.foreground, hov), p.foreground, t);
    painter.text(pos2(rect.min.x + 10.0 + 16.0 + 10.0, rect.center().y), Align2::LEFT_CENTER, label, font, col);
    resp
}

/// The OpenTalk mark: ring with three waveform bars, drawn at `center` with radius `r`.
pub fn draw_logo(ui: &Ui, center: egui::Pos2, r: f32, p: &Palette) {
    let s = r / 10.0; // the SVG is a 24 viewBox with a radius-10 ring
    let painter = ui.painter();
    painter.circle_stroke(center, r - s, Stroke::new(2.0 * s, p.foreground));
    for (dx, half) in [(-3.25f32, 2.0f32), (0.0, 4.0), (3.25, 2.0)] {
        painter.line_segment([pos2(center.x + dx * s, center.y - half * s), pos2(center.x + dx * s, center.y + half * s)], Stroke::new(2.0 * s, p.foreground));
        painter.circle_filled(pos2(center.x + dx * s, center.y - half * s), s, p.foreground);
        painter.circle_filled(pos2(center.x + dx * s, center.y + half * s), s, p.foreground);
    }
}

/// 64x64 window icon: blue disc with three white waveform bars, rendered with 4x4 supersampling.
pub fn app_icon() -> egui::IconData {
    const N: usize = 64;
    let mut rgba = vec![0u8; N * N * 4];
    let bars = [(-13.0f32, 8.0f32), (0.0, 16.0), (13.0, 8.0)]; // (x offset from centre, half height)
    let (cx, cy) = (N as f32 / 2.0, N as f32 / 2.0);
    for y in 0..N {
        for x in 0..N {
            let (mut disc, mut bar) = (0u32, 0u32);
            for sy in 0..4 {
                for sx in 0..4 {
                    let px = x as f32 + (sx as f32 + 0.5) / 4.0 - cx;
                    let py = y as f32 + (sy as f32 + 0.5) / 4.0 - cy;
                    if px * px + py * py <= 30.0 * 30.0 {
                        disc += 1;
                    }
                    for (bx, half) in bars {
                        // Capsule: distance to the vertical segment <= 4.
                        let dy = (py.abs() - half).max(0.0);
                        let dx = px - bx;
                        if dx * dx + dy * dy <= 16.0 {
                            bar += 1;
                            break;
                        }
                    }
                }
            }
            let i = (y * N + x) * 4;
            let (dr, dg, db) = (0x45u32, 0x77u32, 0xe9u32);
            let a = disc * 255 / 16;
            let bar_cov = bar.min(disc);
            // Blend white over blue by bar coverage within the disc.
            let mix = |c: u32| (c * (16 - bar_cov) + 255 * bar_cov) / 16;
            rgba[i] = mix(dr) as u8;
            rgba[i + 1] = mix(dg) as u8;
            rgba[i + 2] = mix(db) as u8;
            rgba[i + 3] = a as u8;
        }
    }
    egui::IconData { rgba, width: N as u32, height: N as u32 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn icon_is_square_with_transparent_corners_and_opaque_centre() {
        let ic = app_icon();
        assert_eq!((ic.width, ic.height), (64, 64));
        assert_eq!(ic.rgba.len(), 64 * 64 * 4);
        assert_eq!(ic.rgba[3], 0, "corner is transparent");
        let centre = (32 * 64 + 32) * 4;
        assert_eq!(ic.rgba[centre + 3], 255);
        assert!(ic.rgba[centre] > 200, "centre bar is white");
        let off_bar = (32 * 64 + 26) * 4;
        assert_eq!(ic.rgba[off_bar + 3], 255);
        assert!(ic.rgba[off_bar] < 120, "between bars it is the blue disc");
    }
}
