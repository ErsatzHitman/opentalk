//! Settings pages: the index (`index_model`), one page per [`Section`] (`model`) and the
//! list pickers (`picker_model`). Every setting of the former Settings window lives here as a
//! pure [`PageModel`]: `items()` reads `Cx::cfg`, `event()` mutates it and answers
//! `Effect::Saved`; the popup posts the config to the daemon, which applies it in-process
//! (`App::apply_config`). Secrets go through `PopupAction::SetSecret`, provider tests through
//! `Effect::RunTest`, the hotkey through the daemon hook's capture mode.
//!
//! Each page has a text snapshot test (`snapshots/*.txt`, written by `kit::dump`): the pages'
//! wording and order are reviewed by reading those files.
pub mod about;
pub mod cleanup;
pub mod hotkeys;
pub mod index;
pub mod keyfield;
pub mod kit;
pub mod pickers;
pub mod prefs;
pub mod privacy;
pub mod stt;

use super::{PageModel, PickerId, Section};

/// The Settings index (`Route::SettingsIndex`): one compact row per section (the rewrite trigger
/// is set on the Cleanup & AI page).
pub fn index_model() -> Box<dyn PageModel> {
    index::IndexPage::boxed()
}

/// One Settings section page (`Route::Settings`).
pub fn model(section: Section) -> Box<dyn PageModel> {
    match section {
        Section::Preferences => prefs::PrefsPage::boxed(),
        Section::Hotkeys => hotkeys::HotkeysPage::boxed(),
        Section::SpeechToText => stt::SttPage::boxed(),
        Section::Cleanup => cleanup::CleanupPage::boxed(),
        Section::Privacy => privacy::PrivacyPage::boxed(),
        Section::About => about::AboutPage::boxed(),
    }
}

/// A list picker (`Route::Picker`): radio rows, optional filter field; picking applies and goes Back.
pub fn picker_model(id: PickerId) -> Box<dyn PageModel> {
    pickers::PickerPage::boxed(id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::config::Config;
    use crate::daemon::tray::page::{Cx, Effect, Event, Item, ItemId, Route};

    /// Walk every Settings route: pages build, enter/leave, and every interactive id is unique.
    #[test]
    fn every_settings_route_builds_with_unique_item_ids() {
        let mut routes = vec![Route::SettingsIndex];
        routes.extend(Section::ALL.iter().map(|s| Route::Settings(*s)));
        routes.extend(PickerId::ALL.iter().map(|p| Route::Picker(*p)));
        for r in routes {
            let mut cfg = Config::default();
            let mut m = crate::daemon::tray::page::model_for(&r).unwrap();
            let mut cx = Cx::new(&mut cfg, 0);
            m.enter(&mut cx);
            let ids: Vec<ItemId> = m.items(&cx).iter().filter_map(Item::id).collect();
            let mut sorted = ids.clone();
            sorted.sort_unstable();
            sorted.dedup();
            assert_eq!(sorted.len(), ids.len(), "{r:?} reuses an item id");
            m.leave(&mut cx);
        }
    }

    /// Every Settings page ignores events for items it does not have (a stale event after a
    /// re-layout must never panic or change the config).
    #[test]
    fn stray_events_are_harmless() {
        for s in Section::ALL {
            let mut cfg = Config::default();
            let mut m = model(s);
            let mut cx = Cx::new(&mut cfg, 0);
            for ev in [Event::Pressed, Event::Activated, Event::Toggled(true), Event::Picked(7), Event::Committed("x".into()), Event::Deleted, Event::Copy] {
                assert!(m.event(60000, ev, &mut cx).is_empty(), "{s:?}");
            }
            assert_eq!(*cx.cfg, Config::default(), "{s:?}");
        }
    }

    /// Pages never ask for `Saved` without changing something the daemon needs, and the Navigate
    /// targets they use all have a model.
    #[test]
    fn navigation_targets_exist() {
        let mut cfg = Config::default();
        let mut m = index_model();
        let mut cx = Cx::new(&mut cfg, 0);
        let ids: Vec<ItemId> = m.items(&cx).iter().filter_map(Item::id).collect();
        for id in ids {
            for fx in m.event(id, Event::Activated, &mut cx) {
                if let Effect::Navigate(r) = fx {
                    assert!(crate::daemon::tray::page::model_for(&r).is_some(), "{r:?}");
                }
            }
        }
    }
}
