//! Settings > Privacy & Data: history, retention, failed recordings, discarded dictations,
//! the data folder and clearing history.
use super::kit::{ToggleDef, apply_toggle, button, choice, group, note, saved};
use crate::core::settings_logic::{RETENTION, retention_index};
use crate::daemon::tray::page::{BtnStyle, ConfirmId, Cx, Effect, Event, Item, ItemId, PageModel, PickerId, PopupAction, Route};

pub const ID_RETENTION: ItemId = 2;
pub const ID_OPEN_DATA: ItemId = 10;
pub const ID_CLEAR: ItemId = 11;

const TOGGLES: &[ToggleDef] = &[
    ToggleDef {
        id: 1,
        label: "Save history",
        desc: "Keep a list of your dictations in History. Turn off to save nothing.",
        get: |c| c.history_enabled,
        set: |c, v| c.history_enabled = v,
    },
    ToggleDef {
        id: 3,
        label: "Keep the last failed recording",
        desc: "Hold it in memory so you can retry after a network or key error. Cleared when OpenTalk exits.",
        get: |c| c.keep_failed_audio,
        set: |c, v| c.keep_failed_audio = v,
    },
    ToggleDef {
        id: 4,
        label: "Save discarded dictations",
        desc: "Also list dictations that were cancelled or contained no speech, under \"Show discarded\" in History.",
        get: |c| c.save_discarded,
        set: |c, v| c.save_discarded = v,
    },
];

#[derive(Default)]
pub struct PrivacyPage;

impl PrivacyPage {
    pub fn boxed() -> Box<dyn PageModel> {
        Box::new(PrivacyPage)
    }
}

impl PageModel for PrivacyPage {
    fn title(&self) -> String {
        "Privacy & Data".into()
    }

    fn items(&self, cx: &Cx) -> Vec<Item> {
        let cfg = &*cx.cfg;
        vec![
            group("History"),
            note("Your dictations are stored only on this PC."),
            TOGGLES[0].item(cfg),
            choice(ID_RETENTION, "Keep transcripts for", RETENTION[retention_index(cfg.history_retention_days)].0),
            note("Older entries are deleted automatically."),
            group("Recordings"),
            note("Audio is never kept on disk."),
            TOGGLES[1].item(cfg),
            TOGGLES[2].item(cfg),
            group("Your data"),
            note(&format!("Everything lives in {}", crate::core::paths::app_dir().display())),
            button(ID_OPEN_DATA, "Open data folder", BtnStyle::Outline, true),
            button(ID_CLEAR, "Clear history...", BtnStyle::Destructive, true),
        ]
    }

    fn event(&mut self, id: ItemId, ev: Event, cx: &mut Cx) -> Vec<Effect> {
        if apply_toggle(TOGGLES, id, &ev, cx.cfg) {
            return saved();
        }
        match (id, ev) {
            (ID_RETENTION, Event::Activated) => vec![Effect::Navigate(Route::Picker(PickerId::Retention))],
            (ID_OPEN_DATA, Event::Pressed) => vec![Effect::OpenPath(crate::core::paths::app_dir())],
            (ID_CLEAR, Event::Pressed) => vec![Effect::Confirm {
                title: "Clear all history?".into(),
                body: "Every saved dictation will be deleted. This cannot be undone.".into(),
                action: "Clear all".into(),
                destructive: true,
                on_confirm: ConfirmId::HistoryClear,
            }],
            _ => Vec::new(),
        }
    }

    fn confirmed(&mut self, id: ConfirmId, _cx: &mut Cx) -> Vec<Effect> {
        match id {
            ConfirmId::HistoryClear => vec![Effect::Do(PopupAction::HistoryClear), Effect::Toast("History cleared".into())],
            _ => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::config::Config;
    use crate::daemon::tray::page::settings::kit::{dump, snapshot};

    #[test]
    fn page_text_snapshot() {
        // The data folder differs per machine: dump the page with the path line neutralised.
        let mut cfg = Config::default();
        let cx = Cx::new(&mut cfg, false, 0);
        let text = dump("Privacy & Data", &PrivacyPage.items(&cx)).lines().map(|l| if l.contains("Everything lives in") { "  note: Everything lives in <data folder>".to_string() } else { l.to_string() }).collect::<Vec<_>>().join("\n");
        snapshot!("privacy", text);
    }

    #[test]
    fn toggles_save_their_fields() {
        let mut cfg = Config::default();
        let mut p = PrivacyPage;
        let mut cx = Cx::new(&mut cfg, false, 0);
        assert_eq!(p.event(1, Event::Toggled(false), &mut cx), vec![Effect::Saved]);
        assert_eq!(p.event(3, Event::Toggled(false), &mut cx), vec![Effect::Saved]);
        assert_eq!(p.event(4, Event::Toggled(true), &mut cx), vec![Effect::Saved]);
        assert!(!cx.cfg.history_enabled && !cx.cfg.keep_failed_audio && cx.cfg.save_discarded);
    }

    #[test]
    fn retention_shows_the_rounded_up_period_and_opens_its_picker() {
        let mut cfg = Config { history_retention_days: 45, ..Config::default() };
        let mut p = PrivacyPage;
        let mut cx = Cx::new(&mut cfg, false, 0);
        assert!(p.items(&cx).iter().any(|i| matches!(i, Item::Choice { id: ID_RETENTION, value, .. } if value == "90 days")));
        assert_eq!(p.event(ID_RETENTION, Event::Activated, &mut cx), vec![Effect::Navigate(Route::Picker(PickerId::Retention))]);
    }

    #[test]
    fn clear_history_is_confirmed_before_anything_is_deleted() {
        let mut cfg = Config::default();
        let mut p = PrivacyPage;
        let mut cx = Cx::new(&mut cfg, false, 0);
        let fx = p.event(ID_CLEAR, Event::Pressed, &mut cx);
        assert_eq!(fx.len(), 1);
        assert!(matches!(&fx[0], Effect::Confirm { destructive: true, on_confirm: ConfirmId::HistoryClear, .. }));
        let fx = p.confirmed(ConfirmId::HistoryClear, &mut cx);
        assert_eq!(fx[0], Effect::Do(PopupAction::HistoryClear));
        assert!(p.confirmed(ConfirmId::ResetApp, &mut cx).is_empty(), "not ours");
    }

    #[test]
    fn open_data_folder_opens_the_app_dir() {
        let mut cfg = Config::default();
        let mut cx = Cx::new(&mut cfg, false, 0);
        assert_eq!(PrivacyPage.event(ID_OPEN_DATA, Event::Pressed, &mut cx), vec![Effect::OpenPath(crate::core::paths::app_dir())]);
    }
}
