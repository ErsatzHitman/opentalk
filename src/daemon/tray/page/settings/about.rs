//! Settings > About: version, data and log folders, reset app data.
use super::kit::{button, group, note};
use crate::core::config::Config;
use crate::daemon::tray::page::{BtnStyle, ConfirmId, Cx, Effect, Event, Item, ItemId, PageModel, PopupAction, Row, Tone, Trailing};

pub const ID_VERSION: ItemId = 1;
pub const ID_OPEN_DATA: ItemId = 2;
pub const ID_OPEN_LOGS: ItemId = 3;
pub const ID_RESET: ItemId = 4;

#[derive(Default)]
pub struct AboutPage;

impl AboutPage {
    pub fn boxed() -> Box<dyn PageModel> {
        Box::new(AboutPage)
    }
}

impl PageModel for AboutPage {
    fn title(&self) -> String {
        "About".into()
    }

    fn items(&self, _cx: &Cx) -> Vec<Item> {
        vec![
            group("OpenTalk"),
            Item::Row(Row {
                secondary: "Personal dictation for Windows.".into(),
                trailing: Trailing::Badge { text: format!("v{}", env!("CARGO_PKG_VERSION")), tone: Tone::Info },
                ..Row::new(ID_VERSION, "Version", 44.0)
            }),
            group("Folders"),
            note(&format!("Settings, history and logs live in {}", crate::core::paths::app_dir().display())),
            button(ID_OPEN_DATA, "Open data folder", BtnStyle::Outline, true),
            button(ID_OPEN_LOGS, "Open logs folder", BtnStyle::Outline, true),
            note("Diagnostic logs never contain API keys or your text."),
            group("Reset"),
            note("Delete settings, history and saved API keys, and start fresh."),
            button(ID_RESET, "Reset app data...", BtnStyle::Destructive, true),
        ]
    }

    fn event(&mut self, id: ItemId, ev: Event, _cx: &mut Cx) -> Vec<Effect> {
        match (id, ev) {
            (ID_OPEN_DATA, Event::Pressed) => vec![Effect::OpenPath(crate::core::paths::app_dir())],
            (ID_OPEN_LOGS, Event::Pressed) => vec![Effect::OpenPath(crate::core::paths::log_dir())],
            (ID_RESET, Event::Pressed) => vec![Effect::Confirm {
                title: "Reset all app data?".into(),
                body: "Your settings, history, dictionary, snippets and saved API keys will be deleted. This cannot be undone.".into(),
                action: "Reset everything".into(),
                destructive: true,
                on_confirm: ConfirmId::ResetApp,
            }],
            _ => Vec::new(),
        }
    }

    fn confirmed(&mut self, id: ConfirmId, cx: &mut Cx) -> Vec<Effect> {
        match id {
            ConfirmId::ResetApp => {
                // The daemon deletes the files and applies the defaults; keep this copy in step so
                // the pages show them at once (no `Saved`: that would write the config back).
                *cx.cfg = Config::default();
                vec![Effect::Do(PopupAction::Reset), Effect::Toast("App data reset".into())]
            }
            _ => Vec::new(),
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
        let cx = Cx::new(&mut cfg, false, 0);
        let text = dump("About", &AboutPage.items(&cx))
            .lines()
            .map(|l| if l.contains("live in ") { "  note: Settings, history and logs live in <data folder>".to_string() } else { l.replace(env!("CARGO_PKG_VERSION"), "<version>") })
            .collect::<Vec<_>>()
            .join("\n");
        snapshot!("about", text);
    }

    #[test]
    fn folders_open_through_the_shell() {
        let mut cfg = Config::default();
        let mut cx = Cx::new(&mut cfg, false, 0);
        assert_eq!(AboutPage.event(ID_OPEN_DATA, Event::Pressed, &mut cx), vec![Effect::OpenPath(crate::core::paths::app_dir())]);
        assert_eq!(AboutPage.event(ID_OPEN_LOGS, Event::Pressed, &mut cx), vec![Effect::OpenPath(crate::core::paths::log_dir())]);
    }

    #[test]
    fn reset_asks_first_then_resets_the_daemon_and_this_copy() {
        let mut cfg = Config { hotkey: "F8".into(), ..Config::default() };
        let mut p = AboutPage;
        let mut cx = Cx::new(&mut cfg, false, 0);
        let fx = p.event(ID_RESET, Event::Pressed, &mut cx);
        assert!(matches!(&fx[0], Effect::Confirm { destructive: true, on_confirm: ConfirmId::ResetApp, .. }));
        assert_eq!(cx.cfg.hotkey, "F8", "nothing changes before the confirmation");
        let fx = p.confirmed(ConfirmId::ResetApp, &mut cx);
        assert_eq!(fx[0], Effect::Do(PopupAction::Reset));
        assert!(!fx.contains(&Effect::Saved), "saving the old copy would undo the reset");
        assert_eq!(*cx.cfg, Config::default());
        assert!(p.confirmed(ConfirmId::HistoryClear, &mut cx).is_empty());
    }

    #[test]
    fn version_row_shows_the_package_version() {
        let mut cfg = Config::default();
        let cx = Cx::new(&mut cfg, false, 0);
        assert!(AboutPage.items(&cx).iter().any(|i| matches!(i, Item::Row(r) if matches!(&r.trailing, Trailing::Badge { text, .. } if text == &format!("v{}", env!("CARGO_PKG_VERSION"))))));
    }
}
