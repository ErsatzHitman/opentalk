//! Preferences: theme, behaviour toggles, floating icon, language, startup and microphone.
use super::languages::{LANGUAGES, label_of};
use super::{SettingsState, group};
use crate::core::config::{Config, PanelPosition, Theme};
use crate::ui::widgets::{self as w};
use crate::ui::{Cmd, Env};
use eframe::egui::Ui;

#[derive(Default)]
pub struct PrefsState {
    /// Capture endpoints (id, name); loaded lazily, refreshed on demand.
    devices: Option<Vec<(String, String)>>,
    autostart: Option<bool>,
}

impl PrefsState {
    /// Re-read the launch-at-login state next time the page is drawn.
    pub fn forget_autostart(&mut self) {
        self.autostart = None;
    }
}

/// Keep-warm choices: (label, seconds).
pub const WARM_CHOICES: &[(&str, u32)] = &[("Off", 0), ("30 seconds", 30), ("1 minute", 60), ("5 minutes", 300), ("15 minutes", 900)];

pub fn warm_index(secs: u32) -> usize {
    WARM_CHOICES.iter().position(|(_, s)| *s == secs).unwrap_or_else(|| {
        // Unknown value in config.json: show the closest choice rather than "Off".
        WARM_CHOICES.iter().enumerate().min_by_key(|(_, (_, s))| (*s as i64 - secs as i64).abs()).map(|(i, _)| i).unwrap_or(0)
    })
}

pub const POSITIONS: &[(&str, PanelPosition)] =
    &[("Bottom right", PanelPosition::BottomRight), ("Bottom center", PanelPosition::BottomCenter), ("Bottom left", PanelPosition::BottomLeft)];

pub fn show(ui: &mut Ui, env: &mut Env, st: &mut SettingsState) -> bool {
    let p = *env.p;
    let mut changed = false;
    let cfg: &mut Config = env.cfg;

    // ---- Appearance
    w::section_title(ui, &p, "Appearance", "");
    w::panel(ui, &p, |ui| {
        w::row(ui, &p, "Theme", "Light, dark, or follow Windows.", 200.0, |ui| {
            let mut sel = match cfg.theme {
                Theme::Light => 0,
                Theme::Dark => 1,
                Theme::Auto => 2,
            };
            if w::segmented(ui, &p, &mut sel, &["Light", "Dark", "Auto"]) {
                cfg.theme = [Theme::Light, Theme::Dark, Theme::Auto][sel];
                changed = true;
            }
        });
        w::divider(ui, &p);
        changed |= toggle_row(ui, &p, "Reduce transparency", "Use a solid window background instead of the translucent Windows backdrop.", &mut cfg.reduce_transparency);
    });

    // ---- Dictation
    group(ui, &p, "Dictation", "How a finished dictation reaches your app.");
    w::panel(ui, &p, |ui| {
        changed |= toggle_row(ui, &p, "Dictation sounds", "Play a short cue when recording starts and stops.", &mut cfg.audio_cues);
        w::divider(ui, &p);
        changed |= toggle_row(ui, &p, "Pause media while dictating", "Pause music and video when you start, resume afterwards.", &mut cfg.pause_media);
        w::divider(ui, &p);
        changed |= toggle_row(ui, &p, "Automatic pasting", "Paste the text into the app you were using. When off, it is only copied.", &mut cfg.auto_paste);
        w::divider(ui, &p);
        changed |= toggle_row(ui, &p, "Keep transcription in clipboard", "Leave the text on the clipboard instead of restoring what you had copied.", &mut cfg.keep_in_clipboard);
        w::divider(ui, &p);
        changed |= toggle_row(ui, &p, "Smart spacing", "Add a space before the text when it continues a sentence.", &mut cfg.smart_spacing);
    });

    // ---- Floating icon
    group(ui, &p, "Floating icon", "The small indicator that shows recording state.");
    w::panel(ui, &p, |ui| {
        changed |= toggle_row(ui, &p, "Auto-hide when idle", "Show the icon only while recording or transcribing.", &mut cfg.floating_icon_auto_hide);
        w::divider(ui, &p);
        w::row(ui, &p, "Start position", "Where the icon sits on your screen.", 170.0, |ui| {
            let labels: Vec<String> = POSITIONS.iter().map(|(l, _)| l.to_string()).collect();
            let cur = POSITIONS.iter().position(|(_, v)| *v == cfg.panel_position);
            let r = w::select(ui, &p, "pos", cur.map(|i| POSITIONS[i].0).unwrap_or("Bottom right"), &labels, cur, 170.0, false);
            if let Some(i) = r.chosen {
                cfg.panel_position = POSITIONS[i].1;
                changed = true;
            }
        });
        w::divider(ui, &p);
        changed |= toggle_row(ui, &p, "Show app bubble", "A round icon of the app you are dictating into, beside the indicator.", &mut cfg.show_app_bubble);
    });

    // ---- Language
    group(ui, &p, "Language", "");
    w::panel(ui, &p, |ui| {
        w::row(ui, &p, "Transcription language", "Auto-detect works for most speech. Pick a language for better accuracy.", 200.0, |ui| {
            let labels: Vec<String> = LANGUAGES.iter().map(|(_, l)| l.to_string()).collect();
            let cur = LANGUAGES.iter().position(|(c, _)| *c == cfg.language);
            let r = w::select(ui, &p, "lang", label_of(&cfg.language), &labels, cur, 200.0, true);
            if let Some(i) = r.chosen {
                cfg.language = LANGUAGES[i].0.to_string();
                changed = true;
            }
        });
    });

    // ---- Startup
    group(ui, &p, "Startup", "");
    let auto_now = *st.prefs.autostart.get_or_insert_with(|| crate::ui::safe(crate::core::autostart::is_enabled).unwrap_or(false));
    w::panel(ui, &p, |ui| {
        let mut v = auto_now;
        w::row(ui, &p, "Launch at login", "Start OpenTalk in the tray when you sign in to Windows.", 44.0, |ui| {
            if w::toggle(ui, &p, &mut v) {
                match crate::ui::safe(|| crate::core::autostart::set_enabled(v)) {
                    Some(Ok(())) => st.prefs.autostart = Some(v),
                    Some(Err(e)) => env.cmds.push(Cmd::Toast(format!("Could not change startup: {e}"))),
                    None => env.cmds.push(Cmd::Toast("Could not change startup setting".into())),
                }
            }
        });
        w::divider(ui, &p);
        changed |= toggle_row(ui, &p, "Start minimized", "Do not open this window when OpenTalk launches.", &mut cfg.start_minimized);
    });

    // ---- Microphone
    group(ui, &p, "Microphone", "");
    let devices = st.prefs.devices.get_or_insert_with(load_devices).clone();
    w::panel(ui, &p, |ui| {
        w::row(ui, &p, "Input device", "The microphone used for dictation.", 240.0, |ui| {
            let mut labels = vec!["System default".to_string()];
            labels.extend(devices.iter().map(|(_, n)| n.clone()));
            let cur = match &cfg.mic_device_id {
                None => Some(0),
                Some(id) => devices.iter().position(|(d, _)| d == id).map(|i| i + 1),
            };
            let shown = match (&cfg.mic_device_id, cur) {
                (_, Some(i)) => labels[i].clone(),
                (Some(_), None) => cfg.mic_device_label.clone().unwrap_or_else(|| "Unavailable device".into()),
                _ => "System default".into(),
            };
            let r = w::select(ui, &p, "mic", &shown, &labels, cur, 240.0, false);
            if let Some(i) = r.chosen {
                if i == 0 {
                    cfg.mic_device_id = None;
                    cfg.mic_device_label = None;
                } else {
                    cfg.mic_device_id = Some(devices[i - 1].0.clone());
                    cfg.mic_device_label = Some(devices[i - 1].1.clone());
                }
                changed = true;
            }
        });
        w::divider(ui, &p);
        w::row(ui, &p, "Keep microphone warm", "Keeping the stream open after a dictation makes the next start instant, but the Windows mic indicator stays on.", 170.0, |ui| {
            let labels: Vec<String> = WARM_CHOICES.iter().map(|(l, _)| l.to_string()).collect();
            let cur = warm_index(cfg.mic_warm_hold_secs);
            let r = w::select(ui, &p, "warm", WARM_CHOICES[cur].0, &labels, Some(cur), 170.0, false);
            if let Some(i) = r.chosen {
                cfg.mic_warm_hold_secs = WARM_CHOICES[i].1;
                changed = true;
            }
        });
    });
    ui.add_space(8.0);
    if w::button(ui, &p, w::Btn::new("Refresh devices", w::Variant::Flat).icon(crate::ui::icons::REFRESH).small()).clicked() {
        st.prefs.devices = None;
    }
    changed
}

/// Capture endpoints; a missing/failed audio backend yields only "System default".
fn load_devices() -> Vec<(String, String)> {
    crate::ui::safe(crate::daemon::audio::list_devices).unwrap_or_default().into_iter().map(|d| (d.id, d.name)).collect()
}

pub fn toggle_row(ui: &mut Ui, p: &crate::ui::theme::Palette, label: &str, desc: &str, value: &mut bool) -> bool {
    let mut changed = false;
    w::row(ui, p, label, desc, 44.0, |ui| {
        changed = w::toggle(ui, p, value);
    });
    changed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn warm_index_exact_and_nearest() {
        assert_eq!(warm_index(0), 0);
        assert_eq!(warm_index(60), 2);
        assert_eq!(warm_index(900), 4);
        assert_eq!(warm_index(100), 2, "100 s is closest to 1 minute");
    }

    #[test]
    fn positions_cover_every_variant() {
        for v in [PanelPosition::BottomRight, PanelPosition::BottomCenter, PanelPosition::BottomLeft] {
            assert!(POSITIONS.iter().any(|(_, p)| *p == v));
        }
    }
}
