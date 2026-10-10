//! Control panel (egui/eframe, glow) — a separate on-demand process (`opentalk.exe --settings`).
//!
//! Shell (sidebar History / Dictionary / Settings / Support, 48 px top bar, Ctrl+K palette),
//! History page, Dictionary + Snippets page and the Settings modal, styled with OpenWhispr's
//! tokens (docs/SPEC.md). Fonts come from `C:\Windows\Fonts`. Every setting is saved to
//! `config.json` immediately and followed by `core::ipc::notify_daemon(IpcMsg::Reload)`.
//!
//! The process repaints only on input or while an animation runs; it never polls.
mod app;
mod dictionary;
mod filedialog;
mod fonts;
mod glass;
mod history;
mod hotkey_capture;
mod icons;
mod palette;
mod settings;
mod theme;
mod timefmt;
mod widgets;

use crate::core::config::Config;
pub use settings::Section;
use theme::Palette;

/// Top-level pages of the shell.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Page {
    #[default]
    History,
    Dictionary,
}

/// Requests raised by pages and the settings modal, handled by the app after drawing.
#[derive(Clone, Debug, PartialEq)]
pub enum Cmd {
    Toast(String),
    Copy(String),
    OpenSettings(Section),
    RetryLast,
    ReloadHistory,
}

/// What a page needs while drawing.
pub struct Env<'a> {
    pub p: &'a Palette,
    pub cfg: &'a mut Config,
    pub cmds: &'a mut Vec<Cmd>,
    /// Wall-clock now (unix ms), sampled once per frame.
    pub now_ms: i64,
}

/// Run `f`, turning a panic into `None`. The shared `core` modules are called through this so a
/// failing backend (missing audio device, unreadable secrets store) degrades one control instead
/// of closing the whole window.
pub fn safe<T>(f: impl FnOnce() -> T) -> Option<T> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).ok()
}

/// What `--settings <page>` asked for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Launch {
    pub page: Page,
    pub settings: Option<Section>,
}

/// `page`: None | "history" | "dictionary" | "settings[:section]" (a bare `--page`-style value
/// such as `stt` also opens that settings section).
pub fn parse_page(page: Option<&str>) -> Launch {
    let raw = page.unwrap_or("").trim().trim_start_matches('-').to_lowercase();
    let raw = raw.strip_prefix("page=").unwrap_or(&raw).to_string();
    match raw.as_str() {
        "" | "history" | "home" => Launch { page: Page::History, settings: None },
        "dictionary" | "snippets" | "dict" => Launch { page: Page::Dictionary, settings: None },
        "settings" => Launch { page: Page::History, settings: Some(Section::Preferences) },
        s => {
            if let Some(sec) = s.strip_prefix("settings:").or_else(|| s.strip_prefix("settings/")) {
                Launch { page: Page::History, settings: Some(Section::parse(sec).unwrap_or_default()) }
            } else if let Some(sec) = Section::parse(s) {
                Launch { page: Page::History, settings: Some(sec) }
            } else {
                Launch { page: Page::History, settings: None }
            }
        }
    }
}

/// Entry point for `opentalk.exe --settings [page]`. Blocks until the window closes.
pub fn run(page: Option<&str>) {
    std::panic::set_hook(Box::new(|info| {
        crate::log_err!("ui panic: {info}");
    }));
    let launch = parse_page(page);
    let options = app::native_options();
    let result = eframe::run_native("OpenTalk", options, Box::new(move |cc| Ok(Box::new(app::App::new(cc, launch)))));
    if let Err(e) = result {
        crate::log_err!("settings window failed: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_arguments() {
        assert_eq!(parse_page(None), Launch { page: Page::History, settings: None });
        assert_eq!(parse_page(Some("history")).page, Page::History);
        assert_eq!(parse_page(Some("Dictionary")).page, Page::Dictionary);
        assert_eq!(parse_page(Some("settings")), Launch { page: Page::History, settings: Some(Section::Preferences) });
        assert_eq!(parse_page(Some("settings:stt")).settings, Some(Section::SpeechToText));
        assert_eq!(parse_page(Some("settings:hotkeys")).settings, Some(Section::Hotkeys));
        assert_eq!(parse_page(Some("settings:bogus")).settings, Some(Section::Preferences));
        assert_eq!(parse_page(Some("cleanup")).settings, Some(Section::Cleanup));
        assert_eq!(parse_page(Some("--page=dictionary")).page, Page::Dictionary);
        assert_eq!(parse_page(Some("garbage")), Launch { page: Page::History, settings: None });
    }

    #[test]
    fn safe_catches_panics() {
        assert_eq!(safe(|| 5), Some(5));
        assert_eq!(safe(|| -> u8 { panic!("boom") }), None);
    }
}
