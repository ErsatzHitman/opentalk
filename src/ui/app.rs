//! The eframe application: a frameless, translucent window (Windows acrylic behind our ambient mesh)
//! with a glass top bar, a glass sidebar, page routing and toasts. See docs/GLASS.md.
use super::dictionary::{self, DictState};
use super::history::{self, HistoryState};
use super::palette::{self, Item};
use super::settings::{self, Section, SettingsState};
use super::theme::{self, Palette, mix};
use super::widgets::{self as w, cr};
use super::glass::{self, Gl};
use super::{Cmd, Env, Launch, Page, fonts, icons};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};
use windows::Win32::UI::WindowsAndMessaging::{ChangeWindowMessageFilter, MSG, MSGFLT_ADD};
use crate::core::config::Config;
use crate::core::ipc::{self, IpcMsg};
use eframe::egui::{
    self, Align2, Color32, CursorIcon, Id, Key, Modifiers, Rect, ResizeDirection, Sense, Stroke, Ui, UiBuilder, ViewportCommand, pos2, vec2,
};

pub const SIDEBAR_W: f32 = 192.0;
pub const TOPBAR_H: f32 = 48.0;
/// Gap between the window edge and the glass surfaces, and between the surfaces.
const GAP: f32 = 8.0;
const RESIZE_BORDER: f32 = 6.0;

pub fn native_options() -> eframe::NativeOptions {
    let viewport = egui::ViewportBuilder::default()
        .with_title("OpenTalk")
        .with_app_id("OpenTalk")
        .with_inner_size([1200.0, 800.0])
        .with_min_inner_size([760.0, 520.0])
        .with_decorations(false)
        // Per-pixel alpha: the Windows acrylic backdrop shows through where our mesh is thin.
        .with_transparent(true)
        .with_icon(std::sync::Arc::new(app_icon()));
    // egui needs no depth/stencil buffer or MSAA; asking for none keeps the pixel format minimal.
    // These are eframe's defaults, spelled out so the GL config is explicit and cannot drift.
    // Measured on an NVIDIA driver: none of them (nor software GL, which egui_glow rejects as
    // GL 1.1) changes the ~265 MB of private commit the driver makes; see docs/BENCHMARK.md.
    let glow_options = eframe::egui_glow::GlowConfiguration {
        hardware_acceleration: eframe::egui_glow::HardwareAcceleration::Preferred,
        vsync: true,
        ..Default::default()
    };
    eframe::NativeOptions {
        viewport,
        centered: true,
        depth_buffer: 0,
        stencil_buffer: 0,
        multisampling: 0,
        glow_options,
        event_loop_builder: Some(Box::new(install_page_hook)),
        ..Default::default()
    }
}

// ------------------------------------------------------------ page requests from the daemon

/// Page request received from the daemon (`ipc::encode_settings_page` code); 0 = none.
static PENDING_PAGE: AtomicUsize = AtomicUsize::new(0);
/// Context to wake when a request arrives (set by `App::new`).
static REPAINT_CTX: OnceLock<egui::Context> = OnceLock::new();

/// Hook the event loop's message pump so the "OpenTalk.SettingsPage" message the daemon posts
/// when Settings is already open is seen with no polling: it stores the code and wakes egui.
fn install_page_hook(builder: &mut eframe::EventLoopBuilder<eframe::UserEvent>) {
    use winit::platform::windows::EventLoopBuilderExtWindows;
    let id = ipc::settings_page_message();
    if id == 0 {
        return;
    }
    allow_message_across_integrity_levels(id);
    builder.with_msg_hook(move |raw| {
        // SAFETY: winit passes a pointer to the MSG it is about to dispatch, valid for this call.
        let msg = unsafe { &*(raw as *const MSG) };
        if !accept_page_message(id, msg.message, msg.wParam.0) {
            return false;
        }
        if let Some(ctx) = REPAINT_CTX.get() {
            ctx.request_repaint();
        }
        true
    });
}

/// Let the message through UIPI, so a daemon running at a lower integrity level can still
/// deliver it (process-wide, which is fine: this process only has the one window).
fn allow_message_across_integrity_levels(id: u32) {
    // SAFETY: plain call with a registered message id.
    if let Err(e) = unsafe { ChangeWindowMessageFilter(id, MSGFLT_ADD) } {
        crate::log_warn!("settings: cannot lift the message filter: {e}");
    }
}

/// Record the page code of message `message` if it is the page request `id` and carries a valid
/// code; true when it was consumed.
fn accept_page_message(id: u32, message: u32, wparam: usize) -> bool {
    if message != id {
        return false;
    }
    if ipc::decode_settings_page(wparam).is_some() {
        PENDING_PAGE.store(wparam, Ordering::Release);
    }
    true
}

/// The page request waiting for the next frame, if any.
fn take_page_request() -> Option<Launch> {
    ipc::decode_settings_page(PENDING_PAGE.swap(0, Ordering::AcqRel))
}

/// Show `launch` exactly as a fresh launch with that argument would: the page, and the settings
/// modal only when a section is asked for (otherwise it is closed). Returns true when an open
/// modal was closed, so the caller can run its close hook.
fn apply_launch(page: &mut Page, settings_open: &mut bool, section: &mut Section, launch: Launch) -> bool {
    *page = launch.page;
    let was_open = *settings_open;
    *settings_open = launch.settings.is_some();
    if let Some(s) = launch.settings {
        *section = s;
    }
    was_open && !*settings_open
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
    /// Time (egui seconds) of the next idle working-set trim, if one is pending.
    trim_at: Option<f64>,
    support_open_id: Id,
    /// Raw HWND (0 when unavailable), for the DWM backdrop calls.
    hwnd: isize,
    /// DWM accepted the acrylic system backdrop.
    acrylic_ok: bool,
    /// Windows' "Transparency effects" setting.
    effects_on: bool,
    /// Theme last pushed to DWM's immersive-dark-mode attribute.
    backdrop_dark: Option<bool>,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>, launch: Launch) -> Self {
        fonts::install(&cc.egui_ctx);
        let _ = REPAINT_CTX.set(cc.egui_ctx.clone());
        let cfg = Config::load();
        let hwnd = window_handle(cc);
        let dark0 = match cfg.theme {
            crate::core::config::Theme::Auto => !theme::system_apps_use_light_theme(),
            t => theme::resolve_dark(t),
        };
        let acrylic_ok = glass::sys::enable_acrylic(hwnd, dark0);
        if !acrylic_ok {
            crate::log_info!("settings: no acrylic backdrop (build {}), using the opaque page", glass::sys::os_build());
        }
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
            trim_at: Some(1.5),
            support_open_id: Id::new("support-menu"),
            hwnd,
            acrylic_ok,
            effects_on: glass::sys::transparency_effects_enabled(),
            backdrop_dark: Some(dark0),
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
    /// Fully transparent: our own backdrop layer paints the page colour (partial alpha over the
    /// acrylic, or opaque in the fallback), so the clear must not.
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        [0.0, 0.0, 0.0, 0.0]
    }

    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();

        // Follow Windows when Auto: re-read the registry whenever the window regains focus.
        let focused = ctx.input(|i| i.viewport().focused).unwrap_or(true);
        if focused && !self.was_focused {
            self.sys_dark = !theme::system_apps_use_light_theme();
            self.effects_on = glass::sys::transparency_effects_enabled();
            self.hist.reload();
        }
        if !focused && self.was_focused {
            self.trim_at = Some(ctx.input(|i| i.time) + 0.5);
            // The capture hook is global: once the user is in another window it would swallow
            // their typing there, so a capture only lives while this window has focus.
            if self.settings.hotkeys.capturing() {
                self.settings.hotkeys.cancel(&mut self.cmds);
            }
        }
        self.was_focused = focused;
        if let Some(launch) = take_page_request() {
            crate::log_info!("settings: switching to {launch:?}");
            // A capture pauses the daemon's hotkeys; it must not outlive the section it belongs to.
            if self.settings.hotkeys.capturing() && launch.settings != Some(Section::Hotkeys) {
                self.settings.hotkeys.cancel(&mut self.cmds);
            }
            if apply_launch(&mut self.page, &mut self.settings_open, &mut self.settings.section, launch) {
                let mut cmds = std::mem::take(&mut self.cmds);
                self.settings.on_close(&mut cmds);
                self.cmds = cmds;
            }
        }
        if let Some(t) = self.trim_at {
            let now = ctx.input(|i| i.time);
            if now >= t {
                trim_working_set();
                self.trim_at = None;
            } else {
                ctx.request_repaint_after(std::time::Duration::from_secs_f64(t - now));
            }
        }

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

        // Backdrop first (skill rule 1): opaque page colour, or a partial-alpha page over the Windows
        // acrylic when that is available and not switched off.
        glass::set_opaque(self.cfg.reduce_transparency || !self.acrylic_ok || !self.effects_on);
        if self.backdrop_dark != Some(dark) {
            glass::sys::set_dark(self.hwnd, dark);
            self.backdrop_dark = Some(dark);
        }
        let screen = ui.max_rect();
        let g = Gl::new(&ctx, dark);
        ui.painter().extend(glass::backdrop_shapes(&g, screen));
        let maximized = ctx.input(|i| i.viewport().maximized).unwrap_or(false);
        self.window_resize_handles(&ctx, screen, maximized);

        let top = Rect::from_min_size(screen.min + vec2(GAP, GAP), vec2(screen.width() - 2.0 * GAP, TOPBAR_H));
        let side = Rect::from_min_max(pos2(screen.min.x + GAP, top.max.y + GAP), pos2(screen.min.x + SIDEBAR_W, screen.max.y - GAP));
        let main = Rect::from_min_max(pos2(side.max.x + GAP, top.max.y + GAP), screen.max - vec2(GAP, GAP));
        self.topbar(ui, &ctx, &p, &g, top, maximized);
        self.sidebar(ui, &p, &g, side);
        self.page_area(ui, &p, main, screen);

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

    /// One glass panel holding the page links (and Settings / Support / identity at the bottom).
    fn sidebar(&mut self, ui: &mut Ui, p: &Palette, g: &Gl, rect: Rect) {
        glass::panel(ui.painter(), g, rect, glass::RADIUS, 0.0);
        let prev = w::set_in_glass(ui.ctx(), true);
        let mut y = rect.min.y + 12.0;
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
        ui.painter().line_segment([pos2(rect.min.x + 16.0, div_y), pos2(rect.max.x - 16.0, div_y)], Stroke::new(1.0, p.divider()));
        // Identity.
        draw_logo(ui, pos2(ident.min.x + 22.0, ident.center().y), 11.0, p);
        ui.painter().text(pos2(ident.min.x + 42.0, ident.center().y - 7.0), Align2::LEFT_CENTER, "OpenTalk", fonts::semibold(12.5), p.fg_a(85));
        ui.painter().text(pos2(ident.min.x + 42.0, ident.center().y + 8.0), Align2::LEFT_CENTER, format!("v{}", env!("CARGO_PKG_VERSION")), fonts::regular(11.0), p.fg_a(50));
        w::set_in_glass(ui.ctx(), prev);
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
            .frame(egui::Frame::NONE)
            .show(|ui| {
                w::menu_surface(ui, p, |ui| {
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
            });
        if let Some(s) = open_settings {
            self.cmds.push(Cmd::OpenSettings(s));
        }
        if let Some(f) = folder {
            w::shell_open(&f.display().to_string());
        }
    }

    // ------------------------------------------------------------ top bar and page area

    /// The glass top bar: logo and page title on the left, the search pill in the middle, the
    /// window controls on the right. It is also the window's drag region.
    fn topbar(&mut self, ui: &mut Ui, ctx: &egui::Context, p: &Palette, g: &Gl, top: Rect, maximized: bool) {
        glass::panel(ui.painter(), g, top, 14.0, 0.0);
        let prev = w::set_in_glass(ui.ctx(), true);
        // Drag region first so the controls on top win the hit test.
        self.drag_region(ui, top, "topbar-drag");
        draw_logo(ui, pos2(top.min.x + 26.0, top.center().y), 10.0, p);
        let title = match self.page {
            Page::History => "History",
            Page::Dictionary => "Dictionary",
        };
        ui.painter().text(pos2(top.min.x + 46.0, top.center().y), Align2::LEFT_CENTER, title, fonts::semibold(14.0), p.foreground);
        // Search pill, centred in the bar: a wash on the bar's glass, no border of its own.
        let pill_w = 340.0f32.min((top.width() - 2.0 * 190.0).max(180.0));
        let pill = Rect::from_center_size(pos2(top.center().x, top.center().y), vec2(pill_w, 32.0));
        let sresp = ui.interact(pill, Id::new("topbar-search"), Sense::click());
        w::pointer(&sresp);
        let hov = w::hover_anim(ui, sresp.id.with("h"), sresp.hovered());
        ui.painter().rect_filled(pill, cr(16.0), mix(p.wash(0), p.wash(1), hov));
        w::paint_icon(ui, Rect::from_center_size(pos2(pill.min.x + 20.0, pill.center().y), vec2(16.0, 16.0)), icons::SEARCH, 13.0, p.muted_a(75));
        ui.painter().text(pos2(pill.min.x + 36.0, pill.center().y), Align2::LEFT_CENTER, "Search", fonts::regular(13.0), p.muted_a(75));
        {
            let font = fonts::semibold(10.0);
            let label = "Ctrl + K";
            let tw = w::measure(ui, label, font.clone()).x;
            let kr = Rect::from_center_size(pos2(pill.max.x - 14.0 - (tw + 12.0) / 2.0, pill.center().y), vec2(tw + 12.0, 18.0));
            ui.painter().rect_filled(kr, cr(9.0), p.wash(1));
            ui.painter().text(kr.center(), Align2::CENTER_CENTER, label, font, p.muted_a(80));
        }
        if sresp.clicked() {
            self.palette.toggle();
        }
        self.window_controls(ui, ctx, p, top, maximized);
        w::set_in_glass(ui.ctx(), prev);
    }

    /// The page body. It has no surface of its own: the pages put their glass panels straight on
    /// the backdrop.
    fn page_area(&mut self, ui: &mut Ui, p: &Palette, main: Rect, screen: Rect) {
        let mut body_ui = ui.new_child(UiBuilder::new().max_rect(main).layout(egui::Layout::top_down(egui::Align::Min)));
        // Let panel shadows spill into the gaps around the page, but not under the other surfaces.
        let clip = Rect::from_min_max(pos2(main.min.x - GAP, main.min.y - GAP + 2.0), screen.max);
        body_ui.set_clip_rect(clip.intersect(ui.clip_rect()));
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
            let bg = if close { mix(Color32::TRANSPARENT, Color32::from_rgb(0xe8, 0x11, 0x23), hov) } else { mix(Color32::TRANSPARENT, p.wash(1), hov) };
            ui.painter().rect_filled(r, cr(if close { 10.0 } else { 8.0 }), bg);
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
                    let g = Gl::new(ctx, p.dark);
                    let slot = ui.painter().add(egui::Shape::Noop);
                    let out = egui::Frame::new().inner_margin(egui::Margin::symmetric(18, 10)).show(ui, |ui| {
                        ui.label(egui::RichText::new(msg).font(fonts::regular(13.0)).color(p.foreground));
                    });
                    ui.painter().set(slot, egui::Shape::Vec(glass::menu_shapes(&g, out.response.rect, 20.0)));
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
    painter.rect_filled(rect, cr(8.0), mix(mix(Color32::TRANSPARENT, p.wash(1), hov), p.primary_wash(1), t));
    let ic = mix(mix(p.fg_a(70), p.fg_a(90), hov), p.primary, t);
    painter.text(pos2(rect.min.x + 10.0 + 8.0, rect.center().y), Align2::CENTER_CENTER, glyph.to_string(), fonts::icon(15.0), ic);
    let font = if active { fonts::semibold(13.0) } else { fonts::regular(13.0) };
    let col = mix(mix(p.fg_a(88), p.foreground, hov), p.foreground, t);
    painter.text(pos2(rect.min.x + 10.0 + 16.0 + 10.0, rect.center().y), Align2::LEFT_CENTER, label, font, col);
    resp
}

/// The raw `HWND` of the window being created (0 if the platform handle is not a Win32 one).
fn window_handle(cc: &eframe::CreationContext<'_>) -> isize {
    use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
    match cc.window_handle().map(|h| h.as_raw()) {
        Ok(RawWindowHandle::Win32(h)) => h.hwnd.get(),
        _ => 0,
    }
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

    /// The pending-page slot is process-global: tests that use it run one at a time.
    static SLOT: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn launch(page: Page, settings: Option<Section>) -> Launch {
        Launch { page, settings }
    }

    #[test]
    fn apply_launch_matches_a_fresh_launch() {
        for l in [
            launch(Page::History, None),
            launch(Page::Dictionary, None),
            launch(Page::History, Some(Section::Preferences)),
            launch(Page::History, Some(Section::SpeechToText)),
        ] {
            // Whatever state the window was in, the result equals what App::new sets for `l`.
            for (page, open, sec) in [(Page::History, false, Section::Preferences), (Page::Dictionary, true, Section::Hotkeys)] {
                let (mut p, mut o, mut s) = (page, open, sec);
                let closed = apply_launch(&mut p, &mut o, &mut s, l);
                assert_eq!(p, l.page);
                assert_eq!(o, l.settings.is_some());
                if let Some(want) = l.settings {
                    assert_eq!(s, want);
                } else {
                    assert_eq!(s, sec, "section is left alone when the modal is not asked for");
                }
                assert_eq!(closed, open && l.settings.is_none());
            }
        }
    }

    #[test]
    fn apply_launch_dictionary_while_history_is_open() {
        let (mut p, mut o, mut s) = (Page::History, false, Section::Preferences);
        assert!(!apply_launch(&mut p, &mut o, &mut s, crate::ui::parse_page(Some("dictionary"))));
        assert_eq!(p, Page::Dictionary);
        assert!(!o);
    }

    #[test]
    fn page_message_is_consumed_and_stored_once() {
        let _g = SLOT.lock().unwrap_or_else(|e| e.into_inner());
        PENDING_PAGE.store(0, Ordering::SeqCst);
        let id = 0xC123;
        let l = launch(Page::Dictionary, Some(Section::Cleanup));
        assert!(!accept_page_message(id, id + 1, ipc::encode_settings_page(l)), "other messages pass through");
        assert_eq!(take_page_request(), None);
        assert!(accept_page_message(id, id, ipc::encode_settings_page(l)));
        assert_eq!(take_page_request(), Some(l));
        assert_eq!(take_page_request(), None, "a request is applied once");
    }

    #[test]
    fn invalid_page_code_is_swallowed_without_effect() {
        let _g = SLOT.lock().unwrap_or_else(|e| e.into_inner());
        PENDING_PAGE.store(0, Ordering::SeqCst);
        assert!(accept_page_message(7, 7, 0));
        assert_eq!(take_page_request(), None);
    }

    #[test]
    fn newest_request_wins() {
        let _g = SLOT.lock().unwrap_or_else(|e| e.into_inner());
        PENDING_PAGE.store(0, Ordering::SeqCst);
        accept_page_message(7, 7, ipc::encode_settings_page(launch(Page::History, None)));
        accept_page_message(7, 7, ipc::encode_settings_page(launch(Page::Dictionary, None)));
        assert_eq!(take_page_request(), Some(launch(Page::Dictionary, None)));
    }
}

/// Hand the pages the GL driver and font atlas touched at start-up back to Windows. The panel is
/// idle almost all the time, so this keeps its resident size small; pages fault back in on demand.
fn trim_working_set() {
    use windows::Win32::System::ProcessStatus::EmptyWorkingSet;
    use windows::Win32::System::Threading::GetCurrentProcess;
    // SAFETY: the pseudo handle from GetCurrentProcess is always valid for this process.
    unsafe {
        let _ = EmptyWorkingSet(GetCurrentProcess());
    }
}
