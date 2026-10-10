//! The Settings index: one row per section, plus the "Rewrite trigger" shortcut that opens the
//! Cleanup & AI page with the trigger field focused (two taps from the tray).
use super::cleanup::{ID_RW_TOGGLE, ID_RW_TRIGGER};
use super::kit::group;
use crate::core::config::Config;
use crate::daemon::hotkey;
use crate::daemon::tray::page::{Cx, Effect, Event, Item, ItemId, PageModel, Route, Row, Section, Trailing};

/// Section rows are `ID_SECTION0 + position in Section::ALL`.
pub const ID_SECTION0: ItemId = 1;
pub const ID_REWRITE: ItemId = 20;

fn section_id(s: Section) -> ItemId {
    ID_SECTION0 + Section::ALL.iter().position(|x| *x == s).unwrap_or(0) as ItemId
}

fn blurb(s: Section) -> &'static str {
    match s {
        Section::Preferences => "Theme, sounds, language, microphone",
        Section::Hotkeys => "Dictation hotkey and activation",
        Section::SpeechToText => "Provider, API key, model",
        Section::Cleanup => "AI rewrite, filler words, AI cleanup",
        Section::Privacy => "History, retention, your data",
        Section::About => "Version, folders, reset",
    }
}

/// Right-hand value of a section row (the current setting where there is one).
fn value(s: Section, cfg: &Config) -> Trailing {
    match s {
        Section::Hotkeys => Trailing::Value(hotkey::label(&cfg.hotkey)),
        Section::SpeechToText => Trailing::Value(crate::stt::registry::provider(cfg.stt.provider).name.into()),
        _ => Trailing::Chevron,
    }
}

#[derive(Default)]
pub struct IndexPage;

impl IndexPage {
    pub fn boxed() -> Box<dyn PageModel> {
        Box::new(IndexPage)
    }
}

impl PageModel for IndexPage {
    fn title(&self) -> String {
        "Settings".into()
    }

    fn items(&self, cx: &Cx) -> Vec<Item> {
        let cfg = &*cx.cfg;
        let mut v = vec![group("AI rewrite")];
        let (phrase, trailing) = match cfg.rewrite_trigger_effective() {
            Some(p) => (format!("\"{p}\""), Trailing::Chevron),
            None if !cfg.ai_cleanup.rewrite_enabled => ("Off".to_string(), Trailing::Chevron),
            None => ("No phrase set".to_string(), Trailing::Chevron),
        };
        v.push(Item::Row(Row { secondary: phrase, trailing, ..Row::new(ID_REWRITE, "Rewrite trigger", 44.0) }));
        for (label, sections) in Section::groups() {
            v.push(group(label));
            for s in sections {
                v.push(Item::Row(Row { secondary: blurb(*s).into(), trailing: value(*s, cfg), ..Row::new(section_id(*s), s.label(), 44.0) }));
            }
        }
        v
    }

    fn event(&mut self, id: ItemId, ev: Event, cx: &mut Cx) -> Vec<Effect> {
        if ev != Event::Activated {
            return Vec::new();
        }
        if id == ID_REWRITE {
            // Land on the phrase field; with the toggle off it is hidden, so land on the toggle.
            let focus = if cx.cfg.ai_cleanup.rewrite_enabled { ID_RW_TRIGGER } else { ID_RW_TOGGLE };
            return vec![Effect::Navigate(Route::Settings(Section::Cleanup)), Effect::Focus(focus)];
        }
        match Section::ALL.iter().find(|s| section_id(**s) == id) {
            Some(s) => vec![Effect::Navigate(Route::Settings(*s))],
            None => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::tray::page::settings::kit::{dump, snapshot};

    #[test]
    fn page_text_snapshot() {
        let mut cfg = Config::default();
        let cx = Cx::new(&mut cfg, 0);
        snapshot!("index", dump("Settings", &IndexPage.items(&cx)));
        cfg.ai_cleanup.rewrite_enabled = false;
        cfg.hotkey = "F8".into();
        cfg.stt.provider = crate::core::config::SttProviderId::Groq;
        let cx = Cx::new(&mut cfg, 0);
        snapshot!("index_rewrite_off", dump("Settings", &IndexPage.items(&cx)));
    }

    #[test]
    fn every_section_has_exactly_one_row_that_opens_it() {
        let mut cfg = Config::default();
        let mut cx = Cx::new(&mut cfg, 0);
        let mut p = IndexPage;
        for s in Section::ALL {
            let rows = p.items(&cx).into_iter().filter(|i| matches!(i, Item::Row(r) if r.primary == s.label())).count();
            assert_eq!(rows, 1, "{s:?}");
            assert_eq!(p.event(section_id(s), Event::Activated, &mut cx), vec![Effect::Navigate(Route::Settings(s))]);
        }
    }

    #[test]
    fn rewrite_shortcut_jumps_to_the_trigger_field() {
        let mut cfg = Config::default();
        let mut cx = Cx::new(&mut cfg, 0);
        let mut p = IndexPage;
        assert_eq!(p.event(ID_REWRITE, Event::Activated, &mut cx), vec![Effect::Navigate(Route::Settings(Section::Cleanup)), Effect::Focus(ID_RW_TRIGGER)]);
        cx.cfg.ai_cleanup.rewrite_enabled = false;
        assert_eq!(p.event(ID_REWRITE, Event::Activated, &mut cx), vec![Effect::Navigate(Route::Settings(Section::Cleanup)), Effect::Focus(ID_RW_TOGGLE)], "the field is hidden while the rewrite is off");
    }

    #[test]
    fn rewrite_row_shows_the_phrase_or_why_it_is_off() {
        let mut cfg = Config::default();
        let secondary = |cfg: &mut Config| {
            let cx = Cx::new(cfg, 0);
            IndexPage.items(&cx).into_iter().find_map(|i| match i {
                Item::Row(r) if r.id == ID_REWRITE => Some(r.secondary),
                _ => None,
            })
        };
        assert_eq!(secondary(&mut cfg).as_deref(), Some("\"rewrite this concisely\""));
        cfg.rewrite_trigger = "  ".into();
        assert_eq!(secondary(&mut cfg).as_deref(), Some("No phrase set"));
        cfg.ai_cleanup.rewrite_enabled = false;
        assert_eq!(secondary(&mut cfg).as_deref(), Some("Off"));
    }

    #[test]
    fn rows_that_show_a_setting_follow_the_config() {
        let mut cfg = Config { hotkey: "Control+Shift+Space".into(), ..Config::default() };
        let cx = Cx::new(&mut cfg, 0);
        let items = IndexPage.items(&cx);
        let trailing = |label: &str| items.iter().find_map(|i| match i {
            Item::Row(r) if r.primary == label => Some(r.trailing.clone()),
            _ => None,
        });
        assert_eq!(trailing("Hotkeys"), Some(Trailing::Value("Ctrl + Shift + Space".into())));
        assert_eq!(trailing("Speech-to-Text"), Some(Trailing::Value("OpenAI".into())));
        assert_eq!(trailing("About"), Some(Trailing::Chevron));
    }

    #[test]
    fn unknown_ids_and_other_events_do_nothing() {
        let mut cfg = Config::default();
        let mut cx = Cx::new(&mut cfg, 0);
        assert!(IndexPage.event(99, Event::Activated, &mut cx).is_empty());
        assert!(IndexPage.event(ID_SECTION0, Event::Pressed, &mut cx).is_empty());
    }
}
