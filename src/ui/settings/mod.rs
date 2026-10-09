//! Settings modal (896 px x 85vh) with a grouped 208 px sidebar. Every change is applied to the
//! in-memory `Config` by the section code, which reports `changed`; the app then saves
//! `config.json` immediately and notifies the daemon.
pub mod cleanup;
pub mod hotkeys;
pub mod languages;
pub mod preferences;
pub mod privacy;
pub mod stt;
pub mod system;

use super::fonts;
use super::icons;
use super::theme::{Palette, mix, with_alpha};
use super::widgets::{self as w, Btn, Variant, cr};
use super::{Cmd, Env};
use eframe::egui::{self, Align, Align2, Color32, CornerRadius, Layout, Rect, Sense, Stroke, StrokeKind, Ui, UiBuilder, pos2, vec2};
use std::collections::HashMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Section {
    #[default]
    Preferences,
    Hotkeys,
    SpeechToText,
    Cleanup,
    Privacy,
    System,
}

impl Section {
    pub const ALL: [Section; 6] = [Section::Preferences, Section::Hotkeys, Section::SpeechToText, Section::Cleanup, Section::Privacy, Section::System];

    pub fn label(self) -> &'static str {
        match self {
            Section::Preferences => "Preferences",
            Section::Hotkeys => "Hotkeys",
            Section::SpeechToText => "Speech-to-Text",
            Section::Cleanup => "Cleanup",
            Section::Privacy => "Privacy & Data",
            Section::System => "System",
        }
    }

    fn icon(self) -> char {
        match self {
            Section::Preferences => icons::PREFS,
            Section::Hotkeys => icons::KEYBOARD,
            Section::SpeechToText => icons::MIC,
            Section::Cleanup => icons::SPARKLE,
            Section::Privacy => icons::SHIELD,
            Section::System => icons::SETTINGS,
        }
    }

    /// Sidebar groups: (group label, sections).
    pub fn groups() -> [(&'static str, &'static [Section]); 3] {
        [
            ("App", &[Section::Preferences, Section::Hotkeys]),
            ("AI Models", &[Section::SpeechToText, Section::Cleanup]),
            ("System", &[Section::Privacy, Section::System]),
        ]
    }

    /// Parse the `settings:<section>` suffix used by `--settings settings:stt` and deep links.
    pub fn parse(s: &str) -> Option<Section> {
        match s.trim().to_lowercase().as_str() {
            "preferences" | "prefs" | "general" => Some(Section::Preferences),
            "hotkeys" | "hotkey" => Some(Section::Hotkeys),
            "speech-to-text" | "stt" | "speechtotext" | "transcription" => Some(Section::SpeechToText),
            "cleanup" | "intelligence" | "llm" => Some(Section::Cleanup),
            "privacy" | "privacy-data" | "privacydata" => Some(Section::Privacy),
            "system" => Some(Section::System),
            _ => None,
        }
    }
}

/// Decrypted-on-demand secrets, cached masked so DPAPI is not hit every frame.
#[derive(Default)]
pub struct SecretCache {
    masked: HashMap<String, Option<String>>,
}

impl SecretCache {
    pub fn masked(&mut self, name: &str) -> Option<String> {
        self.masked
            .entry(name.to_string())
            .or_insert_with(|| super::safe(|| crate::core::secrets::get(name)).flatten().filter(|v| !v.trim().is_empty()).map(|v| crate::core::secrets::mask(&v)))
            .clone()
    }

    pub fn has(&mut self, name: &str) -> bool {
        self.masked(name).is_some()
    }

    pub fn set(&mut self, name: &str, value: &str) -> Result<(), String> {
        match super::safe(|| crate::core::secrets::set(name, value)) {
            Some(Ok(())) => {
                self.masked.remove(name);
                Ok(())
            }
            Some(Err(e)) => Err(e.to_string()),
            None => Err("secure storage is unavailable".into()),
        }
    }

    pub fn forget(&mut self) {
        self.masked.clear();
    }
}

/// Everything the settings modal remembers between frames.
#[derive(Default)]
pub struct SettingsState {
    pub section: Section,
    pub secrets: SecretCache,
    /// (secret name, text being typed) for the API-key field currently in edit mode.
    pub key_edit: Option<(String, String)>,
    /// Text typed into API-key fields that have no stored value yet, per secret name. Kept apart
    /// from `key_edit` so two empty fields on one page (Corti, Bedrock) do not fight over it.
    pub new_keys: HashMap<String, String>,
    pub prefs: preferences::PrefsState,
    pub hotkeys: hotkeys::HotkeyState,
    pub stt: stt::SttState,
    pub cleanup: cleanup::CleanupState,
    pub privacy: privacy::PrivacyState,
    pub system: system::SystemState,
}

impl SettingsState {
    /// Called when the modal closes: release hooks and transient state.
    pub fn on_close(&mut self, env_cmds: &mut Vec<Cmd>) {
        self.hotkeys.cancel(env_cmds);
        self.key_edit = None;
        self.new_keys.clear();
    }
}

pub struct ModalOut {
    pub changed: bool,
    pub close: bool,
}

/// Draw the modal. Returns whether config changed and whether the user closed it.
pub fn show(ctx: &egui::Context, env: &mut Env, st: &mut SettingsState) -> ModalOut {
    let p = *env.p;
    let screen = ctx.content_rect();
    let width = (screen.width() - 48.0).clamp(560.0, 896.0);
    let height = (screen.height() * 0.85).clamp(420.0, 900.0);
    let mut out = ModalOut { changed: false, close: false };
    let sidebar_w = if width < 700.0 { 56.0 } else { 208.0 };

    let frame = egui::Frame::new()
        .fill(if p.dark { p.surface1 } else { p.background })
        .stroke(Stroke::new(1.0, if p.dark { p.border_subtle } else { p.border }))
        .corner_radius(cr(12.0))
        .shadow(egui::Shadow { offset: [0, 25], blur: 50, spread: 0, color: Color32::from_black_alpha(if p.dark { 130 } else { 64 }) });
    let modal = egui::Modal::new(egui::Id::new("settings-modal"))
        .backdrop_color(Color32::from_black_alpha(if p.dark { 150 } else { 70 }))
        .frame(frame)
        .show(ctx, |ui| {
            ui.spacing_mut().item_spacing = vec2(0.0, 0.0);
            let (rect, _) = ui.allocate_exact_size(vec2(width, height), Sense::hover());
            let side_rect = Rect::from_min_size(rect.min, vec2(sidebar_w, height));
            let main_rect = Rect::from_min_max(pos2(rect.min.x + sidebar_w, rect.min.y), rect.max);
            // Sidebar background + divider.
            let r = CornerRadius { nw: 12, sw: 12, ne: 0, se: 0 };
            ui.painter().rect_filled(side_rect.shrink(0.5), r, if p.dark { p.surface0 } else { p.surface1 });
            ui.painter().line_segment([side_rect.right_top(), side_rect.right_bottom()], Stroke::new(1.0, if p.dark { p.border_subtle } else { p.border_a(70) }));
            let mut side = ui.new_child(UiBuilder::new().max_rect(side_rect).layout(Layout::top_down(Align::Min)));
            sidebar(&mut side, &p, st, sidebar_w < 100.0);

            let mut main = ui.new_child(UiBuilder::new().max_rect(main_rect.shrink2(vec2(0.0, 1.0))).layout(Layout::top_down(Align::Min)));
            main.set_clip_rect(main_rect.shrink(1.0));
            egui::ScrollArea::vertical().id_salt(("settings-scroll", st.section as u8)).auto_shrink([false, false]).show(&mut main, |ui| {
                ui.spacing_mut().item_spacing = vec2(0.0, 0.0);
                egui::Frame::new().inner_margin(egui::Margin { left: 24, right: 24, top: 22, bottom: 28 }).show(ui, |ui| {
                    ui.set_width(main_rect.width() - 48.0 - 2.0);
                    out.changed |= section(ui, env, st);
                });
            });

            // Close button, top right.
            let cr_ = Rect::from_center_size(pos2(rect.max.x - 24.0, rect.min.y + 24.0), vec2(28.0, 28.0));
            let resp = ui.interact(cr_, ui.id().with("settings-close"), Sense::click());
            w::pointer(&resp);
            let hov = w::hover_anim(ui, resp.id.with("h"), resp.hovered());
            ui.painter().rect_filled(cr_, cr(6.0), mix(Color32::TRANSPARENT, if p.dark { p.raised } else { p.muted }, hov));
            w::paint_icon(ui, cr_, icons::CLOSE, 12.0, mix(p.muted_a(50), p.muted_a(100), hov));
            if resp.clicked() {
                out.close = true;
            }
        });
    // Esc / backdrop click closes, unless a capture or a popup owns the keyboard.
    let busy = st.hotkeys.capturing() || egui::Popup::is_any_open(ctx) || st.key_edit.is_some();
    if modal.should_close() && !busy {
        out.close = true;
    }
    out
}

fn sidebar(ui: &mut Ui, p: &Palette, st: &mut SettingsState, compact: bool) {
    ui.add_space(20.0);
    let w_ = ui.available_width();
    if !compact {
        ui.horizontal(|ui| {
            ui.add_space(16.0);
            w::text_semi(ui, "Settings", 14.0, p.foreground);
        });
        ui.add_space(14.0);
    }
    for (gi, (glabel, items)) in Section::groups().into_iter().enumerate() {
        if gi > 0 {
            ui.add_space(14.0);
        }
        if !compact {
            ui.horizontal(|ui| {
                ui.add_space(16.0);
                w::text_semi(ui, glabel, 11.0, p.muted_a(70));
            });
            ui.add_space(4.0);
        }
        for s in items {
            let (rect, resp) = ui.allocate_exact_size(vec2(w_, 30.0), Sense::click());
            let r = rect.shrink2(vec2(8.0, 0.0));
            w::pointer(&resp);
            let active = st.section == *s;
            let hov = w::hover_anim(ui, resp.id.with("h"), resp.hovered() && !active);
            let painter = ui.painter();
            if active {
                painter.rect_filled(r, cr(6.0), if p.dark { p.raised } else { p.muted });
            } else if hov > 0.0 {
                painter.rect_filled(r, cr(6.0), mix(Color32::TRANSPARENT, if p.dark { p.surface2 } else { with_alpha(p.muted, 50) }, hov));
            }
            let ic = if active { p.primary } else { mix(p.muted_a(70), p.fg_a(80), hov) };
            painter.text(pos2(r.min.x + 16.0, r.center().y), Align2::CENTER_CENTER, s.icon().to_string(), fonts::icon(14.0), ic);
            if !compact {
                let font = if active { fonts::semibold(12.5) } else { fonts::regular(12.5) };
                let col = if active { p.foreground } else { mix(p.muted_fg, p.foreground, hov) };
                painter.text(pos2(r.min.x + 32.0, r.center().y), Align2::LEFT_CENTER, s.label(), font, col);
            }
            if resp.clicked() {
                st.section = *s;
            }
        }
    }
    // Footer: version with a status dot.
    let h = ui.available_height();
    if h > 36.0 {
        ui.add_space(h - 34.0);
        let (rect, _) = ui.allocate_exact_size(vec2(w_, 34.0), Sense::hover());
        ui.painter().line_segment([rect.left_top(), rect.right_top()], Stroke::new(1.0, if p.dark { p.border_subtle } else { p.border_a(70) }));
        if !compact {
            ui.painter().circle_filled(pos2(rect.min.x + 18.0, rect.center().y), 2.0, p.success_a(60));
            ui.painter().text(pos2(rect.min.x + 28.0, rect.center().y), Align2::LEFT_CENTER, format!("v{}", env!("CARGO_PKG_VERSION")), fonts::regular(11.0), p.muted_a(70));
        }
    }
}

fn section(ui: &mut Ui, env: &mut Env, st: &mut SettingsState) -> bool {
    let p = *env.p;
    ui.add_space(0.0);
    w::text_semi(ui, st.section.label(), 18.0, p.foreground);
    ui.add_space(18.0);
    ui.spacing_mut().item_spacing = vec2(0.0, 0.0);
    match st.section {
        Section::Preferences => preferences::show(ui, env, st),
        Section::Hotkeys => hotkeys::show(ui, env, st),
        Section::SpeechToText => stt::show(ui, env, st),
        Section::Cleanup => cleanup::show(ui, env, st),
        Section::Privacy => privacy::show(ui, env, st),
        Section::System => system::show(ui, env, st),
    }
}

/// Section title + gap used between panels.
pub fn group(ui: &mut Ui, p: &Palette, title: &str, desc: &str) {
    ui.add_space(22.0);
    w::section_title(ui, p, title, desc);
}

/// API-key field: masked display with Edit, or an input with Save/Cancel. `name` is the secret
/// name in `core::secrets`. Returns true when a key was saved or removed.
pub fn api_key_field(ui: &mut Ui, env: &mut Env, st: &mut SettingsState, name: &str, label: &str, key_url: &str) -> bool {
    let p = *env.p;
    let mut saved = false;
    let stored = st.secrets.masked(name);
    let editing = matches!(&st.key_edit, Some((n, _)) if n == name);
    ui.horizontal(|ui| {
        w::text_semi(ui, label, 12.5, p.foreground);
        if !key_url.is_empty() {
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let r = ui.add(egui::Label::new(egui::RichText::new("Get key \u{2192}").font(fonts::regular(12.0)).color(p.primary)).sense(Sense::click()));
                w::pointer(&r);
                if r.clicked() {
                    w::shell_open(key_url);
                }
            });
        }
    });
    ui.add_space(6.0);
    if editing || stored.is_none() {
        let mut done = false;
        let mut cancel = false;
        // An unset field owns a buffer of its own; an existing key being replaced uses `key_edit`.
        let buf: &mut String = if editing {
            match st.key_edit.as_mut() {
                Some((_, b)) => b,
                None => st.new_keys.entry(name.to_string()).or_default(),
            }
        } else {
            st.new_keys.entry(name.to_string()).or_default()
        };
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            let wd = (ui.available_width() - 150.0).max(120.0);
            let r = w::text_input(ui, &p, buf, "Paste your API key", wd, true);
            if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) && !buf.trim().is_empty() {
                done = true;
            }
            if w::button(ui, &p, Btn::new("Save", Variant::Primary).small().enabled(!buf.trim().is_empty())).clicked() {
                done = true;
            }
            if stored.is_some() && w::button(ui, &p, Btn::new("Cancel", Variant::Flat).small()).clicked() {
                cancel = true;
            }
        });
        let typed = buf.trim().to_string();
        if done {
            match st.secrets.set(name, &typed) {
                Ok(()) => {
                    saved = true;
                    st.key_edit = None;
                    st.new_keys.remove(name);
                    env.cmds.push(Cmd::Toast("API key saved".into()));
                }
                Err(e) => env.cmds.push(Cmd::Toast(format!("Could not save the key: {e}"))),
            }
        } else if cancel {
            st.key_edit = None;
        }
    } else if let Some(m) = stored {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            let wd = (ui.available_width() - 150.0).max(120.0);
            let (rect, _) = ui.allocate_exact_size(vec2(wd, 32.0), Sense::hover());
            ui.painter().rect_filled(rect, cr(6.0), if p.dark { p.surface1 } else { p.muted });
            ui.painter().rect_stroke(rect, cr(6.0), Stroke::new(1.0, p.input_border()), StrokeKind::Inside);
            ui.painter().text(pos2(rect.min.x + 10.0, rect.center().y), Align2::LEFT_CENTER, m, fonts::regular(13.0), p.muted_a(90));
            ui.painter().text(pos2(rect.max.x - 10.0, rect.center().y), Align2::RIGHT_CENTER, "\u{25CF} saved", fonts::regular(11.0), p.success);
            if w::button(ui, &p, Btn::new("Edit", Variant::Outline).small()).clicked() {
                st.key_edit = Some((name.to_string(), String::new()));
            }
            if w::button(ui, &p, Btn::new("Remove", Variant::DestructiveOutline).small()).clicked() {
                if st.secrets.set(name, "").is_ok() {
                    saved = true;
                    env.cmds.push(Cmd::Toast("API key removed".into()));
                }
            }
        });
    }
    saved
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn section_parse_aliases() {
        assert_eq!(Section::parse("stt"), Some(Section::SpeechToText));
        assert_eq!(Section::parse("Hotkeys"), Some(Section::Hotkeys));
        assert_eq!(Section::parse("privacy-data"), Some(Section::Privacy));
        assert_eq!(Section::parse("nope"), None);
    }

    #[test]
    fn groups_cover_every_section_once() {
        let mut seen: Vec<Section> = Section::groups().iter().flat_map(|(_, s)| s.iter().copied()).collect();
        seen.sort_by_key(|s| *s as u8);
        let mut all = Section::ALL.to_vec();
        all.sort_by_key(|s| *s as u8);
        assert_eq!(seen, all);
    }
}
