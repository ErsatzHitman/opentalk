//! Privacy & Data: history, retention, failed audio, discarded dictations, data folder.
use super::{SettingsState, group};
use crate::ui::widgets::{self as w, Btn, DialogResult, Variant};
use crate::ui::{Cmd, Env, icons};
use eframe::egui::Ui;

#[derive(Default)]
pub struct PrivacyState {
    confirm_clear: bool,
}

/// Retention choices: (label, days; 0 = keep forever).
pub const RETENTION: &[(&str, u32)] = &[("Keep forever", 0), ("7 days", 7), ("30 days", 30), ("90 days", 90), ("1 year", 365)];

pub fn retention_index(days: u32) -> usize {
    RETENTION.iter().position(|(_, d)| *d == days).unwrap_or_else(|| {
        // An unlisted value (hand-edited config): show the nearest longer period so nothing is
        // displayed as shorter than what is actually enforced.
        RETENTION.iter().position(|(_, d)| *d != 0 && *d >= days).unwrap_or(0)
    })
}

pub fn show(ui: &mut Ui, env: &mut Env, st: &mut SettingsState) -> bool {
    let p = *env.p;
    let mut changed = false;

    w::section_title(ui, &p, "History", "Your dictations are stored only on this PC.");
    w::panel(ui, &p, |ui| {
        changed |= super::preferences::toggle_row(ui, &p, "Save history", "Keep a list of your dictations in History. Turn off to save nothing.", &mut env.cfg.history_enabled);
        w::divider(ui, &p);
        w::row(ui, &p, "Keep transcripts for", "Older entries are deleted automatically.", 170.0, |ui| {
            let labels: Vec<String> = RETENTION.iter().map(|(l, _)| l.to_string()).collect();
            let cur = retention_index(env.cfg.history_retention_days);
            let r = w::select(ui, &p, "retention", RETENTION[cur].0, &labels, Some(cur), 170.0, false);
            if let Some(i) = r.chosen {
                env.cfg.history_retention_days = RETENTION[i].1;
                changed = true;
                let days = RETENTION[i].1;
                if days > 0 {
                    if let Some(Ok(n)) = crate::ui::safe(|| crate::core::history::prune(days)) {
                        if n > 0 {
                            env.cmds.push(Cmd::Toast(format!("Removed {n} old entries")));
                            env.cmds.push(Cmd::ReloadHistory);
                        }
                    }
                }
            }
        });
    });

    group(ui, &p, "Recordings", "Audio is never kept on disk by default.");
    w::panel(ui, &p, |ui| {
        changed |= super::preferences::toggle_row(ui, &p, "Keep the last failed recording", "Hold it in memory so you can retry after a network or key error. Cleared when OpenTalk exits.", &mut env.cfg.keep_failed_audio);
        w::divider(ui, &p);
        changed |= super::preferences::toggle_row(ui, &p, "Save discarded dictations", "Also list dictations that were cancelled or contained no speech, under \"Show discarded\".", &mut env.cfg.save_discarded);
    });

    group(ui, &p, "Your data", "");
    w::panel(ui, &p, |ui| {
        w::row(ui, &p, "Open data folder", &crate::core::paths::app_dir().display().to_string(), 120.0, |ui| {
            if w::button(ui, &p, Btn::new("Open", Variant::Outline).icon(icons::FOLDER).small()).clicked() {
                w::shell_open(&crate::core::paths::app_dir().display().to_string());
            }
        });
        w::divider(ui, &p);
        w::row(ui, &p, "Clear history", "Delete every saved dictation. This cannot be undone.", 120.0, |ui| {
            if w::button(ui, &p, Btn::new("Clear", Variant::DestructiveOutline).icon(icons::DELETE).small()).clicked() {
                st.privacy.confirm_clear = true;
            }
        });
    });

    if st.privacy.confirm_clear {
        match w::confirm_dialog(ui.ctx(), &p, "confirm-clear-history-settings", "Clear all history?", "Every saved dictation will be deleted. This cannot be undone.", "Clear all", true) {
            DialogResult::Confirm => {
                let _ = crate::ui::safe(crate::core::history::clear);
                st.privacy.confirm_clear = false;
                env.cmds.push(Cmd::ReloadHistory);
                env.cmds.push(Cmd::Toast("History cleared".into()));
            }
            DialogResult::Cancel => st.privacy.confirm_clear = false,
            DialogResult::Open => {}
        }
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retention_lookup() {
        assert_eq!(retention_index(0), 0);
        assert_eq!(retention_index(30), 2);
        assert_eq!(retention_index(365), 4);
        assert_eq!(RETENTION[retention_index(45)].1, 90, "an unlisted value rounds up to a longer period");
        assert_eq!(retention_index(4000), 0);
    }
}
