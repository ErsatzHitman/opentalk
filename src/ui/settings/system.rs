//! System: version, logs/data folders, reset app data.
use super::{SettingsState, group};
use crate::core::config::Config;
use crate::ui::widgets::{self as w, Btn, DialogResult, Variant};
use crate::ui::{Cmd, Env, icons};
use eframe::egui::Ui;

#[derive(Default)]
pub struct SystemState {
    confirm_reset: bool,
}

/// Files removed by "Reset app data" (relative to the app directory).
pub const RESET_FILES: &[&str] = &["config.json", "config.json.bad", "history.jsonl", "secrets.json"];

/// Delete the user's files under `dir`. Returns the first error message, if any; missing files are fine.
pub fn reset_files(dir: &std::path::Path) -> Result<(), String> {
    let mut first_err = None;
    for f in RESET_FILES {
        match std::fs::remove_file(dir.join(f)) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                first_err.get_or_insert(format!("{f}: {e}"));
            }
        }
    }
    match first_err {
        None => Ok(()),
        Some(e) => Err(e),
    }
}

pub fn show(ui: &mut Ui, env: &mut Env, st: &mut SettingsState) -> bool {
    let p = *env.p;
    let mut changed = false;

    w::section_title(ui, &p, "About", "");
    w::panel(ui, &p, |ui| {
        w::row(ui, &p, "Version", "OpenTalk, personal dictation for Windows.", 90.0, |ui| {
            w::chip(ui, &p, &format!("v{}", env!("CARGO_PKG_VERSION")), w::ChipKind::Neutral);
        });
        w::divider(ui, &p);
        w::row(ui, &p, "Data folder", &crate::core::paths::app_dir().display().to_string(), 110.0, |ui| {
            if w::button(ui, &p, Btn::new("Open", Variant::Outline).icon(icons::FOLDER).small()).clicked() {
                w::shell_open(&crate::core::paths::app_dir().display().to_string());
            }
        });
        w::divider(ui, &p);
        w::row(ui, &p, "Logs", "Diagnostic logs never contain API keys or your text.", 110.0, |ui| {
            if w::button(ui, &p, Btn::new("Open", Variant::Outline).icon(icons::FOLDER).small()).clicked() {
                w::shell_open(&crate::core::paths::log_dir().display().to_string());
            }
        });
    });

    group(ui, &p, "Reset", "");
    w::panel(ui, &p, |ui| {
        w::row(ui, &p, "Reset app data", "Delete settings, history and saved API keys, and start fresh.", 110.0, |ui| {
            if w::button(ui, &p, Btn::new("Reset...", Variant::DestructiveOutline).icon(icons::REFRESH).small()).clicked() {
                st.system.confirm_reset = true;
            }
        });
    });

    if st.system.confirm_reset {
        match w::confirm_dialog(
            ui.ctx(),
            &p,
            "confirm-reset-app",
            "Reset all app data?",
            "Your settings, history, dictionary, snippets and saved API keys will be deleted. This cannot be undone.",
            "Reset everything",
            true,
        ) {
            DialogResult::Confirm => {
                st.system.confirm_reset = false;
                match reset_files(&crate::core::paths::app_dir()) {
                    Ok(()) => {
                        *env.cfg = Config::default();
                        st.secrets.forget();
                        st.key_edit = None;
                        changed = true;
                        env.cmds.push(Cmd::ReloadHistory);
                        env.cmds.push(Cmd::Toast("App data reset".into()));
                    }
                    Err(e) => env.cmds.push(Cmd::Toast(format!("Reset failed: {e}"))),
                }
            }
            DialogResult::Cancel => st.system.confirm_reset = false,
            DialogResult::Open => {}
        }
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reset_removes_user_files_and_keeps_others() {
        let dir = std::env::temp_dir().join(format!("opentalk-ui-reset-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for f in ["config.json", "history.jsonl", "secrets.json", "keep.txt"] {
            std::fs::write(dir.join(f), "x").unwrap();
        }
        reset_files(&dir).unwrap();
        assert!(!dir.join("config.json").exists());
        assert!(!dir.join("history.jsonl").exists());
        assert!(!dir.join("secrets.json").exists());
        assert!(dir.join("keep.txt").exists());
        // Idempotent when files are already gone.
        reset_files(&dir).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
