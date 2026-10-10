//! The Settings index: one compact row per section (icon, name, current value). The rewrite
//! trigger lives on the Cleanup & AI page only.
use super::kit::group;
use crate::core::config::Config;
use crate::daemon::hotkey;
use crate::daemon::tray::page::{Cx, Effect, Event, Icon, Item, ItemId, Lead, PageModel, Route, Row, Section, Trailing};

/// Section rows are `ID_SECTION0 + position in Section::ALL`.
pub const ID_SECTION0: ItemId = 1;
/// Height of a section row.
const ROW_H: f32 = 40.0;

fn section_id(s: Section) -> ItemId {
    ID_SECTION0 + Section::ALL.iter().position(|x| *x == s).unwrap_or(0) as ItemId
}

fn icon(s: Section) -> Icon {
    match s {
        Section::Preferences => Icon::Sliders,
        Section::Hotkeys => Icon::Keyboard,
        Section::SpeechToText => Icon::Mic,
        Section::Cleanup => Icon::Sparkle,
        Section::Privacy => Icon::Shield,
        Section::About => Icon::Info,
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
        let mut v = Vec::new();
        for (label, sections) in Section::groups() {
            v.push(group(label));
            for s in sections {
                v.push(Item::Row(Row { lead: Lead::Icon(icon(*s)), trailing: value(*s, cfg), ..Row::new(section_id(*s), s.label(), ROW_H) }));
            }
        }
        v
    }

    fn event(&mut self, id: ItemId, ev: Event, _cx: &mut Cx) -> Vec<Effect> {
        if ev != Event::Activated {
            return Vec::new();
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
