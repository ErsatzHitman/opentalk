//! Preview pages for the renderer (debug builds and tests only; nothing here ships in a release
//! binary). `gallery` shows every [`Item`] kind with live state, `gallery-list` is a long virtual
//! list (History-like rows) plus head and tail items. `--render-tray-popup --page gallery` renders
//! them; the renderer's tests and goldens use [`all_kinds`].

use super::{BtnStyle, Cx, Effect, Event, FieldKind, Item, ItemId, PageModel, Row, Tone, Trailing, VirtualRows};

/// Every item kind once, in a fixed content (no state): the renderer's reference page.
pub fn all_kinds() -> Vec<Item> {
    let mut v = Vec::new();
    let text = |s: &str| s.to_string();
    v.push(Item::Group(text("Toggles")));
    v.push(Item::Toggle { id: 1, label: text("Dictation sounds"), desc: text("Play a short cue when recording starts and stops."), on: true });
    v.push(Item::Toggle { id: 2, label: text("Pause media while recording"), desc: String::new(), on: false });
    v.push(Item::Toggle {
        id: 3,
        label: text("Automatic pasting"),
        desc: text("Paste the transcript into the app you were using as soon as it is ready, instead of only copying it to the clipboard."),
        on: true,
    });
    v.push(Item::Divider);
    v.push(Item::Group(text("Choices")));
    v.push(Item::Segmented { id: 4, label: text("Theme"), options: vec![text("Light"), text("Dark"), text("Auto")], sel: 2 });
    v.push(Item::Segmented { id: 5, label: String::new(), options: vec![text("Tap to talk"), text("Push to talk")], sel: 1 });
    v.push(Item::Choice { id: 6, label: text("Language"), value: text("English") });
    v.push(Item::Choice { id: 7, label: text("Input device"), value: text("Microphone (USB Audio Device) with a rather long name") });
    v.push(Item::Radio { id: 8, label: text("whisper-large-v3-turbo"), desc: text("Fast and accurate. Streams while you speak."), on: true });
    v.push(Item::Radio { id: 9, label: text("whisper-large-v3"), desc: String::new(), on: false });
    v.push(Item::Group(text("Fields")));
    v.push(Item::Field { id: 10, label: text("Trigger phrase"), hint: text("rewrite this concisely"), value: text("rewrite this concisely"), kind: FieldKind::Text, error: None, max_chars: 80 });
    v.push(Item::Field { id: 11, label: text("API key"), hint: text("Paste your key"), value: text("sk-or-v1-0123456789abcdef"), kind: FieldKind::Secret, error: None, max_chars: 0 });
    v.push(Item::Field { id: 12, label: text("Base URL"), hint: text("https://api.example.com/v1"), value: String::new(), kind: FieldKind::Text, error: Some(text("Enter a URL that starts with https://")), max_chars: 0 });
    v.push(Item::Field {
        id: 13,
        label: text("Custom prompt"),
        hint: text("Leave empty to use the built-in prompt"),
        value: text("You clean up dictated text. Remove filler words, fix punctuation and capitalisation, and keep the meaning.\nNever add commentary."),
        kind: FieldKind::Multiline { rows: 4 },
        error: None,
        max_chars: 0,
    });
    v.push(Item::Field { id: 14, label: text("Dictation hotkey"), hint: String::new(), value: text("Control+Super"), kind: FieldKind::Hotkey, error: None, max_chars: 0 });
    v.push(Item::Group(text("Buttons")));
    v.push(Item::Button { id: 15, label: text("Test transcription"), style: BtnStyle::Primary, enabled: true });
    v.push(Item::Button { id: 16, label: text("Open data folder"), style: BtnStyle::Outline, enabled: true });
    v.push(Item::Button { id: 17, label: text("Clear history"), style: BtnStyle::Destructive, enabled: true });
    v.push(Item::Button { id: 18, label: text("Reset to default"), style: BtnStyle::Link, enabled: true });
    v.push(Item::Button { id: 19, label: text("Not available"), style: BtnStyle::Outline, enabled: false });
    v.push(Item::Result { ok: true, badge: text("Transcribed in 840 ms"), text: text("The quick brown fox jumps over the lazy dog.") });
    v.push(Item::Result { ok: false, badge: text("Error"), text: text("The provider rejected the key (401). Check that it is correct and has not expired.") });
    v.push(Item::Group(text("Rows")));
    v.push(Item::Row({
        let mut r = Row::new(20, "Preferences", 44.0);
        r.trailing = Trailing::Chevron;
        r
    }));
    v.push(Item::Row({
        let mut r = Row::new(21, "Speech-to-Text", 44.0);
        r.trailing = Trailing::Value("OpenRouter".into());
        r
    }));
    v.push(Item::Row({
        let mut r = Row::new(22, "AI cleanup", 44.0);
        r.trailing = Trailing::Badge { text: "On".into(), tone: Tone::Ok };
        r
    }));
    v.push(Item::Row({
        let mut r = Row::new(23, "kubernetes", super::ROW_H_WORD);
        r.trailing = Trailing::Delete;
        r
    }));
    v.push(Item::Row({
        let mut r = Row::new(24, "brb", super::ROW_H_SNIPPET);
        r.secondary = "be right back, give me a minute".into();
        r.trailing = Trailing::Chevron;
        r
    }));
    v.push(Item::Row(history_row(25, 0)));
    v.push(Item::Group(text("Notices")));
    v.push(Item::Banner { tone: Tone::Info, text: text("History is stored on this computer only.") });
    v.push(Item::Banner { tone: Tone::Warn, text: text("No AI key is set, so cleanup and rewrite are off until you add one.") });
    v.push(Item::Banner { tone: Tone::Ok, text: text("Key saved.") });
    v.push(Item::Banner { tone: Tone::Err, text: text("The hotkey is already used by another app. Pick a different combination.") });
    v.push(Item::Note(text("Say the trigger phrase at the end of a dictation. The text before it is rewritten concisely by the AI provider.")));
    v.push(Item::Space(8.0));
    v
}

fn history_row(id: ItemId, i: usize) -> Row {
    let mut r = Row::new(id, format!("{}:{:02} \u{b7} Notepad", 9 + i % 12, (i * 7) % 60), super::ROW_H_HISTORY);
    r.secondary = match i % 4 {
        0 => "Send the revised budget to Priya before the Thursday review and remind her about the vendor contract.".into(),
        1 => "Remind me to renew the domain and check the DNS records.".into(),
        2 => "Okay.".into(),
        _ => "The meeting moved to three o'clock, so please move the deadline for the draft accordingly and tell the team.".into(),
    };
    r.meta = "OpenRouter \u{b7} 640 ms".into();
    r.chips = match i % 4 {
        0 => vec![("AI".into(), Tone::Info)],
        1 => vec![("Failed".into(), Tone::Err)],
        2 => vec![("Discarded".into(), Tone::Warn)],
        _ => vec![("Rewritten".into(), Tone::Ok), ("AI".into(), Tone::Info)],
    };
    r.trailing = Trailing::Copy;
    r
}

/// The interactive gallery.
pub struct Gallery {
    items: Vec<Item>,
}

pub fn gallery_model() -> Box<dyn PageModel> {
    Box::new(Gallery { items: all_kinds() })
}

impl PageModel for Gallery {
    fn title(&self) -> String {
        "Gallery".into()
    }

    fn items(&self, _cx: &Cx) -> Vec<Item> {
        self.items.clone()
    }

    fn event(&mut self, id: ItemId, ev: Event, _cx: &mut Cx) -> Vec<Effect> {
        for it in &mut self.items {
            match (it, &ev) {
                (Item::Toggle { id: i, on, .. }, Event::Toggled(v)) if *i == id => *on = *v,
                (Item::Segmented { id: i, sel, .. }, Event::Picked(s)) if *i == id => *sel = *s,
                (Item::Radio { id: i, on, .. }, Event::Toggled(true)) if *i == id => *on = true,
                (Item::Radio { id: i, on, .. }, Event::Toggled(true)) if *i != id && (8..=9).contains(i) => *on = false,
                (Item::Field { id: i, value, .. }, Event::Committed(t)) if *i == id => *value = t.clone(),
                (Item::Field { id: i, value, .. }, Event::CaptureDone(a)) if *i == id => *value = a.clone(),
                _ => {}
            }
        }
        match ev {
            Event::Activated if id == 14 => vec![Effect::BeginHotkeyCapture(14)],
            Event::Pressed if id == 17 => vec![Effect::Confirm {
                title: "Clear history?".into(),
                body: "This removes every saved dictation from this computer. It cannot be undone.".into(),
                action: "Clear history".into(),
                destructive: true,
                on_confirm: super::ConfirmId::HistoryClear,
            }],
            Event::Pressed => vec![Effect::Toast("Pressed".into())],
            Event::Activated => vec![Effect::Toast("Activated".into())],
            _ => Vec::new(),
        }
    }
}

/// A long list of history-like rows between a header and a "Load older" button.
pub struct LongList {
    count: usize,
}

pub fn long_list_model() -> Box<dyn PageModel> {
    Box::new(LongList { count: 500 })
}

impl PageModel for LongList {
    fn title(&self) -> String {
        "History".into()
    }

    fn items(&self, _cx: &Cx) -> Vec<Item> {
        vec![
            Item::Field { id: 1, label: String::new(), hint: "Search dictations".into(), value: String::new(), kind: FieldKind::Text, error: None, max_chars: 0 },
            Item::Note(format!("{} dictations \u{b7} 12,480 words", self.count)),
        ]
    }

    fn event(&mut self, _id: ItemId, _ev: Event, _cx: &mut Cx) -> Vec<Effect> {
        Vec::new()
    }

    fn virtual_rows(&self) -> Option<VirtualRows<'_>> {
        Some(VirtualRows { count: self.count, row_h: super::ROW_H_HISTORY, row: Box::new(|i| Item::Row(history_row(100 + (i % 1000) as u16, i))) })
    }

    fn row_event(&mut self, index: usize, ev: Event, _cx: &mut Cx) -> Vec<Effect> {
        match ev {
            Event::Copy => vec![Effect::Toast(format!("Copied entry {index}"))],
            Event::Activated => vec![Effect::Toast(format!("Opened entry {index}"))],
            _ => Vec::new(),
        }
    }

    fn tail_items(&self, _cx: &Cx) -> Vec<Item> {
        vec![Item::Button { id: 2, label: "Load older".into(), style: BtnStyle::Outline, enabled: true }]
    }
}

/// A preview page by name (`--page gallery`, `--page gallery-list`).
pub fn model_by_name(name: &str) -> Option<Box<dyn PageModel>> {
    match name {
        "gallery" => Some(gallery_model()),
        "gallery-list" | "list" => Some(long_list_model()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::config::Config;

    #[test]
    fn the_gallery_covers_every_item_kind_and_variant() {
        let items = all_kinds();
        let has = |f: &dyn Fn(&Item) -> bool| items.iter().any(f);
        assert!(has(&|i| matches!(i, Item::Group(_))));
        assert!(has(&|i| matches!(i, Item::Note(_))));
        assert!(has(&|i| matches!(i, Item::Banner { tone: Tone::Info, .. })));
        assert!(has(&|i| matches!(i, Item::Banner { tone: Tone::Warn, .. })));
        assert!(has(&|i| matches!(i, Item::Banner { tone: Tone::Ok, .. })));
        assert!(has(&|i| matches!(i, Item::Banner { tone: Tone::Err, .. })));
        assert!(has(&|i| matches!(i, Item::Toggle { desc, .. } if desc.is_empty())));
        assert!(has(&|i| matches!(i, Item::Toggle { desc, .. } if desc.len() > 100)));
        assert!(has(&|i| matches!(i, Item::Segmented { label, .. } if label.is_empty())));
        assert!(has(&|i| matches!(i, Item::Segmented { options, .. } if options.len() == 3)));
        assert!(has(&|i| matches!(i, Item::Choice { .. })));
        assert!(has(&|i| matches!(i, Item::Radio { on: true, .. })));
        assert!(has(&|i| matches!(i, Item::Radio { on: false, .. })));
        for kind in [FieldKind::Text, FieldKind::Secret, FieldKind::Multiline { rows: 4 }, FieldKind::Hotkey] {
            assert!(has(&|i| matches!(i, Item::Field { kind: k, .. } if *k == kind)), "{kind:?}");
        }
        assert!(has(&|i| matches!(i, Item::Field { error: Some(_), .. })));
        for style in [BtnStyle::Primary, BtnStyle::Outline, BtnStyle::Destructive, BtnStyle::Link] {
            assert!(has(&|i| matches!(i, Item::Button { style: s, enabled: true, .. } if *s == style)), "{style:?}");
        }
        assert!(has(&|i| matches!(i, Item::Button { enabled: false, .. })));
        assert!(has(&|i| matches!(i, Item::Result { ok: true, .. })));
        assert!(has(&|i| matches!(i, Item::Result { ok: false, .. })));
        for t in [Trailing::Chevron, Trailing::Delete, Trailing::Copy] {
            assert!(has(&|i| matches!(i, Item::Row(r) if r.trailing == t)), "{t:?}");
        }
        assert!(has(&|i| matches!(i, Item::Row(Row { trailing: Trailing::Value(_), .. }))));
        assert!(has(&|i| matches!(i, Item::Row(Row { trailing: Trailing::Badge { .. }, .. }))));
        assert!(has(&|i| matches!(i, Item::Divider)));
        assert!(has(&|i| matches!(i, Item::Space(_))));
        // Ids of interactive items are unique.
        let mut ids: Vec<_> = items.iter().filter_map(Item::id).collect();
        let n = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), n);
    }

    #[test]
    fn the_gallery_reacts_to_events() {
        let mut cfg = Config::default();
        let mut cx = Cx::new(&mut cfg, 0);
        let mut g = gallery_model();
        g.event(2, Event::Toggled(true), &mut cx);
        g.event(4, Event::Picked(0), &mut cx);
        g.event(9, Event::Toggled(true), &mut cx);
        let items = g.items(&cx);
        assert!(items.iter().any(|i| matches!(i, Item::Toggle { id: 2, on: true, .. })));
        assert!(items.iter().any(|i| matches!(i, Item::Segmented { id: 4, sel: 0, .. })));
        assert!(items.iter().any(|i| matches!(i, Item::Radio { id: 9, on: true, .. })));
        assert!(items.iter().any(|i| matches!(i, Item::Radio { id: 8, on: false, .. })), "radios are exclusive");
        assert!(matches!(g.event(14, Event::Activated, &mut cx).as_slice(), [Effect::BeginHotkeyCapture(14)]));
        assert!(matches!(g.event(17, Event::Pressed, &mut cx).as_slice(), [Effect::Confirm { .. }]));
        assert!(model_by_name("gallery").is_some() && model_by_name("gallery-list").is_some() && model_by_name("nope").is_none());
    }

    #[test]
    fn the_long_list_is_virtual() {
        let mut cfg = Config::default();
        let cx = Cx::new(&mut cfg, 0);
        let m = long_list_model();
        let v = m.virtual_rows().unwrap();
        assert_eq!((v.count, v.row_h), (500, super::super::ROW_H_HISTORY));
        assert!(matches!((v.row)(499), Item::Row(_)));
        assert_eq!(m.tail_items(&cx).len(), 1);
    }
}
