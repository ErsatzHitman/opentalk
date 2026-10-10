//! Settings > Preferences: theme, dictation behaviour, floating icon, language, startup, microphone.
use super::kit::{ToggleDef, apply_toggle, choice, group, note, saved, segmented};
use crate::core::config::{Config, Theme};
use crate::core::settings_logic::{POSITIONS, WARM_CHOICES, label_of, position_index, warm_index};
use crate::daemon::tray::page::{Cx, Effect, Event, Item, ItemId, PageModel, PickerId, PopupAction, Route};

pub const ID_THEME: ItemId = 1;
pub const ID_POSITION: ItemId = 9;
pub const ID_LANGUAGE: ItemId = 11;
pub const ID_AUTOSTART: ItemId = 12;
pub const ID_MIC: ItemId = 13;
pub const ID_WARM: ItemId = 14;

const DICTATION: &[ToggleDef] = &[
    ToggleDef { id: 2, label: "Dictation sounds", desc: "A short cue on start and stop", get: |c| c.audio_cues, set: |c, v| c.audio_cues = v },
    ToggleDef { id: 3, label: "Pause media while dictating", desc: "Resumes afterwards", get: |c| c.pause_media, set: |c, v| c.pause_media = v },
    ToggleDef {
        id: 4,
        label: "Mute system audio while dictating",
        desc: "Speakers are silent while you record",
        get: |c| c.mute_system_audio,
        set: |c, v| c.mute_system_audio = v,
    },
    ToggleDef { id: 5, label: "Automatic pasting", desc: "Into the app you were using", get: |c| c.auto_paste, set: |c, v| c.auto_paste = v },
    ToggleDef {
        id: 6,
        label: "Keep transcription in clipboard",
        desc: "Instead of restoring what you had copied",
        get: |c| c.keep_in_clipboard,
        set: |c, v| c.keep_in_clipboard = v,
    },
    ToggleDef { id: 7, label: "Smart spacing", desc: "Add a space when continuing a sentence", get: |c| c.smart_spacing, set: |c, v| c.smart_spacing = v },
];

const ICON: &[ToggleDef] = &[
    ToggleDef { id: 8, label: "Auto-hide when idle", desc: "Only while recording or transcribing", get: |c| c.floating_icon_auto_hide, set: |c, v| c.floating_icon_auto_hide = v },
    ToggleDef {
        id: 10,
        label: "Show app bubble",
        desc: "The app you are dictating into",
        get: |c| c.show_app_bubble,
        set: |c, v| c.show_app_bubble = v,
    },
];

const THEMES: [(&str, Theme); 3] = [("Light", Theme::Light), ("Dark", Theme::Dark), ("Auto", Theme::Auto)];

#[derive(Default)]
pub struct PrefsPage {
    /// Launch-at-login state (a registry value, not part of `Config`), read when the page opens.
    autostart: Option<bool>,
}

impl PrefsPage {
    pub fn boxed() -> Box<dyn PageModel> {
        Box::<PrefsPage>::default()
    }
}

/// Name of the selected input device for the Choice value.
pub fn device_label(cfg: &Config) -> String {
    match (&cfg.mic_device_id, &cfg.mic_device_label) {
        (None, _) => "System default".into(),
        (Some(_), Some(l)) if !l.trim().is_empty() => l.clone(),
        (Some(_), _) => "Unavailable device".into(),
    }
}

impl PageModel for PrefsPage {
    fn title(&self) -> String {
        "Preferences".into()
    }

    fn enter(&mut self, _cx: &mut Cx) {
        self.autostart = std::panic::catch_unwind(crate::core::autostart::is_enabled).ok();
    }

    fn items(&self, cx: &Cx) -> Vec<Item> {
        let cfg: &Config = cx.cfg;
        let mut v = vec![group("Appearance")];
        let theme = THEMES.iter().position(|(_, t)| *t == cfg.theme).unwrap_or(2);
        v.push(segmented(ID_THEME, "Theme", &THEMES.map(|(l, _)| l), theme));
        v.push(group("Dictation"));
        v.extend(DICTATION.iter().map(|d| d.item(cfg)));
        v.push(group("Floating icon"));
        v.push(ICON[0].item(cfg));
        v.push(segmented(ID_POSITION, "Start position", &POSITIONS.iter().map(|(l, _)| *l).collect::<Vec<_>>(), position_index(cfg.panel_position)));
        v.push(ICON[1].item(cfg));
        v.push(group("Language"));
        v.push(choice(ID_LANGUAGE, "Transcription language", label_of(&cfg.language)));
        v.push(note("Pick one for better accuracy"));
        v.push(group("Startup"));
        v.push(super::kit::toggle(ID_AUTOSTART, "Launch at login", "Start OpenTalk in the tray when you sign in to Windows.", self.autostart.unwrap_or(false)));
        v.push(group("Microphone"));
        v.push(choice(ID_MIC, "Input device", &device_label(cfg)));
        v.push(choice(ID_WARM, "Keep microphone warm", WARM_CHOICES[warm_index(cfg.mic_warm_hold_secs)].0));
        v.push(note("Instant next start; the Windows mic indicator stays on"));
        v
    }

    fn event(&mut self, id: ItemId, ev: Event, cx: &mut Cx) -> Vec<Effect> {
        if apply_toggle(DICTATION, id, &ev, cx.cfg) || apply_toggle(ICON, id, &ev, cx.cfg) {
            return saved();
        }
        match (id, ev) {
            (ID_THEME, Event::Picked(i)) => {
                cx.cfg.theme = THEMES[i.min(2)].1;
                saved()
            }
            (ID_POSITION, Event::Picked(i)) => {
                cx.cfg.panel_position = POSITIONS[i.min(POSITIONS.len() - 1)].1;
                saved()
            }
            (ID_AUTOSTART, Event::Toggled(v)) => {
                self.autostart = Some(v);
                vec![Effect::Do(PopupAction::Autostart(v))]
            }
            (ID_LANGUAGE, Event::Activated) => vec![Effect::Navigate(Route::Picker(PickerId::Language))],
            (ID_MIC, Event::Activated) => vec![Effect::Navigate(Route::Picker(PickerId::InputDevice))],
            (ID_WARM, Event::Activated) => vec![Effect::Navigate(Route::Picker(PickerId::MicWarm))],
            _ => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::tray::page::settings::kit::{dump, snapshot};

    fn page() -> (PrefsPage, Config) {
        (PrefsPage { autostart: Some(false) }, Config::default())
    }

    fn toggled(p: &mut PrefsPage, cfg: &mut Config, id: ItemId, on: bool) -> Vec<Effect> {
        let mut cx = Cx::new(cfg, 0);
        p.event(id, Event::Toggled(on), &mut cx)
    }

    #[test]
    fn page_text_snapshot() {
        let (p, mut cfg) = page();
        let cx = Cx::new(&mut cfg, 0);
        snapshot!("prefs", dump(&p.title(), &p.items(&cx)));
    }

    #[test]
    fn every_toggle_sets_its_field_and_saves() {
        let (mut p, mut cfg) = page();
        // Start from the opposite of every default, then flip each back.
        for d in DICTATION.iter().chain(ICON) {
            let before = (d.get)(&cfg);
            assert_eq!(toggled(&mut p, &mut cfg, d.id, !before), vec![Effect::Saved], "{}", d.label);
            assert_eq!((d.get)(&cfg), !before, "{}", d.label);
            toggled(&mut p, &mut cfg, d.id, before);
            assert_eq!((d.get)(&cfg), before);
        }
        // They really are different fields.
        let mut c = Config::default();
        for d in DICTATION.iter().chain(ICON) {
            (d.set)(&mut c, !(d.get)(&Config::default()));
        }
        assert!(!c.audio_cues && c.pause_media && !c.mute_system_audio && !c.auto_paste && c.keep_in_clipboard && !c.smart_spacing && c.floating_icon_auto_hide && !c.show_app_bubble);
    }

    #[test]
    fn toggle_ids_are_unique() {
        let mut ids: Vec<ItemId> = DICTATION.iter().chain(ICON).map(|d| d.id).collect();
        ids.extend([ID_THEME, ID_POSITION, ID_LANGUAGE, ID_AUTOSTART, ID_MIC, ID_WARM]);
        let n = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), n);
    }

    #[test]
    fn theme_segmented_maps_both_ways() {
        let (mut p, mut cfg) = page();
        for (i, t) in [(0, Theme::Light), (1, Theme::Dark), (2, Theme::Auto)] {
            let mut cx = Cx::new(&mut cfg, 0);
            assert_eq!(p.event(ID_THEME, Event::Picked(i), &mut cx), vec![Effect::Saved]);
            assert_eq!(cx.cfg.theme, t);
            let sel = match p.items(&cx).iter().find(|it| matches!(it, Item::Segmented { id: ID_THEME, .. })) {
                Some(Item::Segmented { sel, .. }) => *sel,
                _ => panic!("no theme control"),
            };
            assert_eq!(sel, i);
        }
    }

    #[test]
    fn start_position_segmented_maps_every_position() {
        let (mut p, mut cfg) = page();
        for (i, (_, pos)) in POSITIONS.iter().enumerate() {
            let mut cx = Cx::new(&mut cfg, 0);
            assert_eq!(p.event(ID_POSITION, Event::Picked(i), &mut cx), vec![Effect::Saved]);
            assert_eq!(cx.cfg.panel_position, *pos);
        }
        // An out-of-range index from a stale event cannot panic.
        let mut cx = Cx::new(&mut cfg, 0);
        p.event(ID_POSITION, Event::Picked(99), &mut cx);
        p.event(ID_THEME, Event::Picked(99), &mut cx);
    }

    #[test]
    fn launch_at_login_is_a_daemon_action_not_a_config_change() {
        let (mut p, mut cfg) = page();
        let fx = toggled(&mut p, &mut cfg, ID_AUTOSTART, true);
        assert_eq!(fx, vec![Effect::Do(PopupAction::Autostart(true))]);
        assert_eq!(cfg, Config::default(), "config untouched");
        let cx = Cx::new(&mut cfg, 0);
        assert!(p.items(&cx).iter().any(|i| matches!(i, Item::Toggle { id: ID_AUTOSTART, on: true, .. })));
    }

    #[test]
    fn choices_open_their_pickers_and_show_current_values() {
        let (mut p, mut cfg) = page();
        cfg.language = "de".into();
        cfg.mic_device_id = Some("dev1".into());
        cfg.mic_device_label = Some("USB Mic".into());
        cfg.mic_warm_hold_secs = 100; // unlisted: shows the nearest choice
        let mut cx = Cx::new(&mut cfg, 0);
        let items = p.items(&cx);
        let value = |id| items.iter().find_map(|i| match i {
            Item::Choice { id: x, value, .. } if *x == id => Some(value.clone()),
            _ => None,
        });
        assert_eq!(value(ID_LANGUAGE).as_deref(), Some("German"));
        assert_eq!(value(ID_MIC).as_deref(), Some("USB Mic"));
        assert_eq!(value(ID_WARM).as_deref(), Some("1 minute"));
        assert_eq!(p.event(ID_LANGUAGE, Event::Activated, &mut cx), vec![Effect::Navigate(Route::Picker(PickerId::Language))]);
        assert_eq!(p.event(ID_MIC, Event::Activated, &mut cx), vec![Effect::Navigate(Route::Picker(PickerId::InputDevice))]);
        assert_eq!(p.event(ID_WARM, Event::Activated, &mut cx), vec![Effect::Navigate(Route::Picker(PickerId::MicWarm))]);
    }

    #[test]
    fn missing_device_is_reported_not_hidden() {
        let mut cfg = Config { mic_device_id: Some("gone".into()), ..Config::default() };
        assert_eq!(device_label(&cfg), "Unavailable device");
        cfg.mic_device_label = Some("Old headset".into());
        assert_eq!(device_label(&cfg), "Old headset");
        cfg.mic_device_id = None;
        assert_eq!(device_label(&cfg), "System default");
    }

    #[test]
    fn unrelated_events_do_nothing() {
        let (mut p, mut cfg) = page();
        let mut cx = Cx::new(&mut cfg, 0);
        assert!(p.event(999, Event::Pressed, &mut cx).is_empty());
        assert!(p.event(2, Event::Pressed, &mut cx).is_empty(), "a toggle ignores non-toggle events");
        assert_eq!(*cx.cfg, Config::default());
    }
}
