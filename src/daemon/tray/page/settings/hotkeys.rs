//! Settings > Hotkeys: the dictation hotkey (captured inside the daemon's own keyboard hook),
//! Tap/Push to talk, Start-menu suppression.
use super::kit::{ToggleDef, apply_toggle, group, note, saved};
use crate::core::config::ActivationMode;
use crate::daemon::hotkey;
use crate::daemon::tray::page::{BtnStyle, Cx, Effect, Event, FieldKind, Item, ItemId, PageModel};

pub const ID_HOTKEY: ItemId = 1;
pub const ID_RESET: ItemId = 2;
pub const ID_TAP: ItemId = 3;
pub const ID_HOLD: ItemId = 4;
pub const ID_START_MENU: ItemId = 5;

pub const DEFAULT_HOTKEY: &str = "Control+Super";

const BEHAVIOUR: &[ToggleDef] = &[ToggleDef {
    id: ID_START_MENU,
    label: "Suppress the Start menu",
    desc: "When the hotkey includes the Windows key, stop Windows from opening the Start menu after you release it.",
    get: |c| c.suppress_start_menu,
    set: |c, v| c.suppress_start_menu = v,
}];

#[derive(Default)]
pub struct HotkeysPage {
    /// Why the last captured combination was refused.
    error: Option<String>,
}

impl HotkeysPage {
    pub fn boxed() -> Box<dyn PageModel> {
        Box::<HotkeysPage>::default()
    }
}

impl PageModel for HotkeysPage {
    fn title(&self) -> String {
        "Hotkeys".into()
    }

    fn items(&self, cx: &Cx) -> Vec<Item> {
        let cfg = &*cx.cfg;
        let mut v = vec![group("Dictation hotkey")];
        v.push(note("Press it anywhere in Windows to start and stop dictation. Modifier-only combinations such as Ctrl + Win work."));
        v.push(Item::Field {
            id: ID_HOTKEY,
            label: "Hotkey".into(),
            hint: "Click, then press your shortcut (Esc cancels)".into(),
            value: cfg.hotkey.clone(),
            kind: FieldKind::Hotkey,
            error: self.error.clone(),
            max_chars: 0,
        });
        v.push(super::kit::button(ID_RESET, "Reset to Ctrl + Win", BtnStyle::Link, cfg.hotkey != DEFAULT_HOTKEY));
        v.push(group("Activation"));
        v.push(super::kit::radio(ID_TAP, "Tap to talk", "Press the hotkey once to start recording and again to stop.", cfg.activation_mode == ActivationMode::Tap));
        v.push(super::kit::radio(
            ID_HOLD,
            "Push to talk",
            "Hold the hotkey while you speak and release to stop. A quick tap cancels.",
            cfg.activation_mode == ActivationMode::Hold,
        ));
        v.push(group("Behaviour"));
        v.extend(BEHAVIOUR.iter().map(|d| d.item(cfg)));
        v.push(note("Press Esc while recording or transcribing to discard the dictation."));
        v
    }

    fn event(&mut self, id: ItemId, ev: Event, cx: &mut Cx) -> Vec<Effect> {
        if apply_toggle(BEHAVIOUR, id, &ev, cx.cfg) {
            return saved();
        }
        match (id, ev) {
            // A click on the box starts capture; the popup then delivers CaptureUpdate/Done here.
            (ID_HOTKEY, Event::Activated | Event::Pressed) => {
                self.error = None;
                vec![Effect::BeginHotkeyCapture(ID_HOTKEY)]
            }
            (ID_HOTKEY, Event::CaptureUpdate(_)) => {
                self.error = None;
                Vec::new()
            }
            (ID_HOTKEY, Event::CaptureDone(accel)) => match hotkey::validate(&accel) {
                None => {
                    self.error = None;
                    let changed = cx.cfg.hotkey != accel;
                    cx.cfg.hotkey = accel;
                    let mut fx = vec![Effect::EndHotkeyCapture];
                    if changed {
                        fx.push(Effect::Saved);
                    }
                    fx
                }
                // Refused: show why and keep listening for another try.
                Some(msg) => {
                    self.error = Some(msg.into());
                    vec![Effect::BeginHotkeyCapture(ID_HOTKEY)]
                }
            },
            (ID_RESET, Event::Pressed) => {
                self.error = None;
                cx.cfg.hotkey = DEFAULT_HOTKEY.into();
                vec![Effect::EndHotkeyCapture, Effect::Saved]
            }
            (ID_TAP, Event::Toggled(true) | Event::Activated) => set_mode(cx, ActivationMode::Tap),
            (ID_HOLD, Event::Toggled(true) | Event::Activated) => set_mode(cx, ActivationMode::Hold),
            _ => Vec::new(),
        }
    }
}

fn set_mode(cx: &mut Cx, m: ActivationMode) -> Vec<Effect> {
    if cx.cfg.activation_mode == m {
        return Vec::new();
    }
    cx.cfg.activation_mode = m;
    saved()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::config::Config;
    use crate::daemon::tray::page::settings::kit::{dump, snapshot};

    #[test]
    fn page_text_snapshot() {
        let mut cfg = Config::default();
        let cx = Cx::new(&mut cfg, false, 0);
        let p = HotkeysPage::default();
        snapshot!("hotkeys", dump(&p.title(), &p.items(&cx)));
    }

    #[test]
    fn click_starts_capture_on_the_hotkey_item() {
        let mut cfg = Config::default();
        let mut p = HotkeysPage { error: Some("old".into()) };
        let mut cx = Cx::new(&mut cfg, false, 0);
        assert_eq!(p.event(ID_HOTKEY, Event::Activated, &mut cx), vec![Effect::BeginHotkeyCapture(ID_HOTKEY)]);
        assert!(p.error.is_none(), "a new attempt clears the old message");
    }

    #[test]
    fn a_valid_capture_is_applied_and_ends_capture() {
        let mut cfg = Config::default();
        let mut p = HotkeysPage::default();
        let mut cx = Cx::new(&mut cfg, false, 0);
        assert!(p.event(ID_HOTKEY, Event::CaptureUpdate("Control".into()), &mut cx).is_empty());
        let fx = p.event(ID_HOTKEY, Event::CaptureDone("Control+Shift+Space".into()), &mut cx);
        assert_eq!(fx, vec![Effect::EndHotkeyCapture, Effect::Saved]);
        assert_eq!(cx.cfg.hotkey, "Control+Shift+Space");
        // Capturing the same combination again changes nothing and saves nothing.
        let fx = p.event(ID_HOTKEY, Event::CaptureDone("Control+Shift+Space".into()), &mut cx);
        assert_eq!(fx, vec![Effect::EndHotkeyCapture]);
    }

    #[test]
    fn function_keys_alone_are_accepted() {
        let mut cfg = Config::default();
        let mut p = HotkeysPage::default();
        let mut cx = Cx::new(&mut cfg, false, 0);
        assert_eq!(p.event(ID_HOTKEY, Event::CaptureDone("F8".into()), &mut cx), vec![Effect::EndHotkeyCapture, Effect::Saved]);
        assert_eq!(cx.cfg.hotkey, "F8");
    }

    #[test]
    fn a_refused_capture_shows_the_reason_and_listens_again() {
        let mut cfg = Config::default();
        let mut p = HotkeysPage::default();
        let mut cx = Cx::new(&mut cfg, false, 0);
        for bad in ["A", "Control", "Space"] {
            let fx = p.event(ID_HOTKEY, Event::CaptureDone(bad.into()), &mut cx);
            assert_eq!(fx, vec![Effect::BeginHotkeyCapture(ID_HOTKEY)], "{bad}");
            assert_eq!(cx.cfg.hotkey, DEFAULT_HOTKEY, "{bad} must not be applied");
            assert!(p.error.is_some());
        }
        let items = p.items(&cx);
        assert!(items.iter().any(|i| matches!(i, Item::Field { id: ID_HOTKEY, error: Some(e), .. } if e.contains("modifier") || e.contains("single"))));
        // The next good attempt clears the message.
        p.event(ID_HOTKEY, Event::CaptureDone("Alt+R".into()), &mut cx);
        assert!(p.error.is_none());
    }

    #[test]
    fn reset_restores_ctrl_win_and_is_disabled_at_the_default() {
        let mut cfg = Config { hotkey: "F8".into(), ..Config::default() };
        let mut p = HotkeysPage::default();
        let mut cx = Cx::new(&mut cfg, false, 0);
        let enabled = |p: &HotkeysPage, cx: &Cx| p.items(cx).iter().find_map(|i| match i {
            Item::Button { id: ID_RESET, enabled, .. } => Some(*enabled),
            _ => None,
        });
        assert_eq!(enabled(&p, &cx), Some(true));
        assert_eq!(p.event(ID_RESET, Event::Pressed, &mut cx), vec![Effect::EndHotkeyCapture, Effect::Saved]);
        assert_eq!(cx.cfg.hotkey, DEFAULT_HOTKEY);
        assert_eq!(enabled(&p, &cx), Some(false));
    }

    #[test]
    fn activation_mode_radios_are_exclusive() {
        let mut cfg = Config::default();
        let mut p = HotkeysPage::default();
        let mut cx = Cx::new(&mut cfg, false, 0);
        assert!(p.event(ID_TAP, Event::Toggled(true), &mut cx).is_empty(), "already tap");
        assert_eq!(p.event(ID_HOLD, Event::Toggled(true), &mut cx), vec![Effect::Saved]);
        assert_eq!(cx.cfg.activation_mode, ActivationMode::Hold);
        let on: Vec<ItemId> = p.items(&cx).iter().filter_map(|i| match i {
            Item::Radio { id, on: true, .. } => Some(*id),
            _ => None,
        }).collect();
        assert_eq!(on, vec![ID_HOLD]);
        assert_eq!(p.event(ID_TAP, Event::Activated, &mut cx), vec![Effect::Saved]);
        assert_eq!(cx.cfg.activation_mode, ActivationMode::Tap);
    }

    #[test]
    fn start_menu_toggle_saves() {
        let mut cfg = Config::default();
        assert!(cfg.suppress_start_menu);
        let mut p = HotkeysPage::default();
        let mut cx = Cx::new(&mut cfg, false, 0);
        assert_eq!(p.event(ID_START_MENU, Event::Toggled(false), &mut cx), vec![Effect::Saved]);
        assert!(!cx.cfg.suppress_start_menu);
    }

    #[test]
    fn the_default_hotkey_is_valid_and_parses() {
        assert!(hotkey::validate(DEFAULT_HOTKEY).is_none());
        assert!(hotkey::parse(DEFAULT_HOTKEY).is_ok());
        assert_eq!(Config::default().hotkey, DEFAULT_HOTKEY);
    }
}
