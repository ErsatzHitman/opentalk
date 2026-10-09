//! Hotkeys: dictation hotkey capture, Tap/Hold, Start-menu suppression.
use super::SettingsState;
use crate::core::config::ActivationMode;
use crate::core::ipc::{self, IpcMsg};
use crate::ui::fonts;
use crate::ui::hotkey_capture::{self as hk, Capture, Captured};
use crate::ui::theme::{Palette, mix};
use crate::ui::widgets::{self as w, Btn, Variant, cr};
use crate::ui::{Cmd, Env, icons};
use eframe::egui::{self, Align2, Sense, Stroke, StrokeKind, Ui, pos2, vec2};

pub const DEFAULT_HOTKEY: &str = "Control+Super";

#[derive(Default)]
pub struct HotkeyState {
    capture: Option<Capture>,
    /// Rejected combo message shown under the box.
    error: Option<String>,
    /// Live label while keys are down.
    preview: String,
}

impl HotkeyState {
    pub fn capturing(&self) -> bool {
        self.capture.is_some()
    }

    /// Stop capturing (if running) and let the daemon's hook resume.
    pub fn cancel(&mut self, _cmds: &mut Vec<Cmd>) {
        if self.capture.take().is_some() {
            resume_daemon();
        }
        self.preview.clear();
    }
}

fn resume_daemon() {
    let _ = crate::ui::safe(|| ipc::notify_daemon(IpcMsg::SuspendHotkeys(false)));
}

pub fn show(ui: &mut Ui, env: &mut Env, st: &mut SettingsState) -> bool {
    let p = *env.p;
    let mut changed = false;

    w::section_title(ui, &p, "Dictation hotkey", "Press it anywhere in Windows to start and stop dictation. Modifier-only combinations such as Ctrl + Win work.");
    w::panel(ui, &p, |ui| {
        w::panel_pad(ui, 16, 16, |ui| {
            ui.spacing_mut().item_spacing = vec2(0.0, 10.0);
            changed |= hotkey_box(ui, env, st);
        });
    });

    crate::ui::settings::group(ui, &p, "Activation", "");
    w::panel(ui, &p, |ui| {
        let mode = env.cfg.activation_mode;
        if mode_card(ui, &p, "Tap to talk", "Press the hotkey once to start recording and again to stop.", mode == ActivationMode::Tap) && mode != ActivationMode::Tap {
            env.cfg.activation_mode = ActivationMode::Tap;
            changed = true;
        }
        w::divider(ui, &p);
        if mode_card(ui, &p, "Push to talk", "Hold the hotkey while you speak and release to stop. A quick tap cancels.", mode == ActivationMode::Hold) && mode != ActivationMode::Hold {
            env.cfg.activation_mode = ActivationMode::Hold;
            changed = true;
        }
    });

    crate::ui::settings::group(ui, &p, "Behaviour", "");
    w::panel(ui, &p, |ui| {
        changed |= super::preferences::toggle_row(
            ui,
            &p,
            "Suppress the Start menu",
            "When the hotkey includes the Windows key, stop Windows from opening the Start menu after you release it.",
            &mut env.cfg.suppress_start_menu,
        );
        w::divider(ui, &p);
        w::row(ui, &p, "Cancel", "Press Esc while recording or transcribing to discard the dictation.", 60.0, |ui| {
            crate::ui::history::keycap(ui, &p, "Esc");
        });
    });
    changed
}

/// Selectable radio card (`ActivationModeSelector`). Returns true when clicked.
fn mode_card(ui: &mut Ui, p: &Palette, title: &str, desc: &str, active: bool) -> bool {
    let w_ = ui.available_width();
    let top = ui.cursor().min;
    let h = 58.0;
    let (rect, resp) = ui.allocate_exact_size(vec2(w_, h), Sense::click());
    w::pointer(&resp);
    let hov = w::hover_anim(ui, resp.id.with("h"), resp.hovered());
    let painter = ui.painter();
    if hov > 0.0 {
        painter.rect_filled(rect, cr(0.0), mix(egui::Color32::TRANSPARENT, p.overlay(3), hov));
    }
    let c = pos2(top.x + 16.0 + 8.0, rect.center().y);
    painter.circle_stroke(c, 8.0, Stroke::new(2.0, if active { p.primary } else { p.border_hover }));
    if active {
        painter.circle_filled(c, 4.0, p.primary);
    }
    painter.text(pos2(top.x + 44.0, rect.center().y - 9.0), Align2::LEFT_CENTER, title, fonts::semibold(12.5), p.foreground);
    painter.text(pos2(top.x + 44.0, rect.center().y + 9.0), Align2::LEFT_CENTER, desc, fonts::regular(12.0), p.muted_a(80));
    resp.clicked()
}

/// The capture box with Change/Reset. Returns true when the hotkey changed.
fn hotkey_box(ui: &mut Ui, env: &mut Env, st: &mut SettingsState) -> bool {
    let p = *env.p;
    let mut changed = false;
    let ctx = ui.ctx().clone();

    // Poll an active capture.
    let mut finished: Option<Captured> = None;
    if let Some(c) = &st.hotkeys.capture {
        let (preview, done) = c.poll();
        st.hotkeys.preview = preview;
        finished = done;
    }
    if let Some(done) = finished {
        match done {
            Captured::Cancelled => {
                st.hotkeys.cancel(env.cmds);
            }
            Captured::Combo(accel) => match hk::validate(&accel) {
                None => {
                    st.hotkeys.cancel(env.cmds);
                    st.hotkeys.error = None;
                    if env.cfg.hotkey != accel {
                        env.cfg.hotkey = accel;
                        changed = true;
                    }
                }
                Some(msg) => {
                    st.hotkeys.error = Some(msg.to_string());
                    if let Some(c) = &st.hotkeys.capture {
                        c.rearm();
                    }
                }
            },
        }
    }

    let capturing = st.hotkeys.capture.is_some();
    let h = 52.0;
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), h), Sense::click());
    if !capturing {
        w::pointer(&resp);
    }
    let hov = w::hover_anim(ui, resp.id.with("h"), resp.hovered() && !capturing);
    let t = ctx.animate_bool_with_time(resp.id.with("cap"), capturing, 0.15);
    {
        let painter = ui.painter();
        painter.rect_filled(rect, cr(8.0), p.input_fill());
        if t > 0.0 {
            painter.rect_stroke(rect.expand(1.5 * t), cr(9.5), Stroke::new(2.0, p.primary_a(15)), StrokeKind::Outside);
        }
        painter.rect_stroke(rect, cr(8.0), Stroke::new(1.0, mix(mix(p.input_border(), p.border_hover, hov), p.primary, t)), StrokeKind::Inside);
    }
    if capturing {
        let label = if st.hotkeys.preview.is_empty() { "Press your shortcut...".to_string() } else { st.hotkeys.preview.clone() };
        let col = if st.hotkeys.preview.is_empty() { p.muted_a(80) } else { p.foreground };
        ui.painter().text(pos2(rect.min.x + 16.0, rect.center().y), Align2::LEFT_CENTER, label, fonts::semibold(13.0), col);
        ui.painter().text(pos2(rect.max.x - 16.0, rect.center().y), Align2::RIGHT_CENTER, "Esc to cancel", fonts::regular(11.5), p.muted_a(70));
    } else {
        // Key caps for the current hotkey.
        let mut x = rect.min.x + 14.0;
        let parts = hk::parts(&env.cfg.hotkey);
        for (i, k) in parts.iter().enumerate() {
            if i > 0 {
                ui.painter().text(pos2(x + 6.0, rect.center().y), Align2::CENTER_CENTER, "+", fonts::regular(12.0), p.muted_a(60));
                x += 18.0;
            }
            let font = fonts::semibold(12.0);
            let tw = w::measure(ui, k, font.clone()).x;
            let cap = egui::Rect::from_min_size(pos2(x, rect.center().y - 14.0), vec2(tw + 20.0, 28.0));
            ui.painter().rect_filled(cap, cr(6.0), if p.dark { p.surface3 } else { egui::Color32::WHITE });
            ui.painter().rect_stroke(cap, cr(6.0), Stroke::new(1.0, p.border_hover), StrokeKind::Inside);
            ui.painter().text(cap.center(), Align2::CENTER_CENTER, k, font, p.foreground);
            x += tw + 20.0;
        }
        ui.painter().text(pos2(rect.max.x - 16.0, rect.center().y), Align2::RIGHT_CENTER, "Click to change", fonts::regular(11.5), p.muted_a(65));
    }
    if resp.clicked() && !capturing {
        start_capture(&ctx, env, st);
    }

    if let Some(e) = &st.hotkeys.error {
        w::text(ui, e.clone(), 12.0, p.destructive);
    }

    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 8.0;
        if capturing {
            if w::button(ui, &p, Btn::new("Cancel", Variant::Outline).small()).clicked() {
                st.hotkeys.cancel(env.cmds);
                st.hotkeys.error = None;
            }
        } else {
            if w::button(ui, &p, Btn::new("Change", Variant::Outline).icon(icons::EDIT).small()).clicked() {
                start_capture(&ctx, env, st);
            }
            let is_default = env.cfg.hotkey == DEFAULT_HOTKEY;
            if w::button(ui, &p, Btn::new("Reset to Ctrl + Win", Variant::Flat).icon(icons::UNDO).small().enabled(!is_default)).clicked() {
                env.cfg.hotkey = DEFAULT_HOTKEY.into();
                st.hotkeys.error = None;
                changed = true;
            }
        }
    });
    changed
}

fn start_capture(ctx: &egui::Context, env: &mut Env, st: &mut SettingsState) {
    st.hotkeys.error = None;
    st.hotkeys.preview.clear();
    // Suspend the daemon's hook first so the same keys are not also treated as a dictation toggle.
    let _ = crate::ui::safe(|| ipc::notify_daemon(IpcMsg::SuspendHotkeys(true)));
    match Capture::start(ctx) {
        Some(c) => st.hotkeys.capture = Some(c),
        None => {
            resume_daemon();
            env.cmds.push(Cmd::Toast("Could not start hotkey capture".into()));
        }
    }
}

impl Drop for HotkeyState {
    /// A capture still running when the window closes must not leave the daemon's hook suspended.
    fn drop(&mut self) {
        if self.capture.take().is_some() {
            resume_daemon();
        }
    }
}
