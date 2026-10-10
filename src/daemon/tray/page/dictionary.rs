//! Dictionary and snippet pages (agent H).
//!
//! * [`model`] (`Route::Dictionary`): an add field (Enter or the button adds; commas and new lines
//!   split several words), a link to the snippets, the speech-prompt budget banner, an optional
//!   filter, the words as fixed-height virtual rows (tap to edit in the add field, "x" to delete)
//!   and Import from clipboard / Copy as JSON / Clear all.
//! * [`snippets_model`] (`Route::Snippets`): the snippets as rows (tap to edit, "x" to delete after
//!   a confirmation), "New snippet", import / export / clear.
//! * [`snippet_edit_model`] (`Route::SnippetEdit`): Trigger (single line, duplicate check) and
//!   Replacement (`Multiline { rows: 5 }`), Save and Delete.
//!
//! Everything works on `cx.cfg` (the popup's working copy) and answers `Effect::Saved`; parsing,
//! import/export, the budget and the trigger check are `core::dictview`.
//!
//! The add field is reset by the model after an add, a cancel, or when a tapped word is loaded into
//! it. A focused field keeps its own edit buffer and ignores a new `value`, so the model hands the
//! renderer a field with a NEW item id each time (`DictionaryPage::input_id`, ids 10..90): the
//! renderer drops the old edit state and shows the new value. The `Effect::Focus` that follows a
//! tapped word names the new id; it takes effect when the renderer lays the page out again before
//! applying it (otherwise the user clicks into the field).
use std::cell::RefCell;

use super::{BtnStyle, ConfirmId, Cx, Effect, Event, FieldKind, Item, ItemId, PageModel, ROW_H_SNIPPET, ROW_H_WORD, Route, Row, Tone, Trailing, VirtualRows};
use crate::core::config::{Config, Snippet};
use crate::core::dictview::{
    MAX_IMPORT_BYTES, add_words, budget_warning, export_snippets_json, export_words_json, filter_words, hint_words, merge_snippets, one_line, parse_import, parse_words, prompt_chars,
    trigger_taken, truncate,
};
use crate::core::histview::thousands;

/// Virtual row `i` has item id `ROW_BASE + i`.
const ROW_BASE: ItemId = 100;
/// The filter field only appears once the list is long enough to need it.
const FILTER_MIN_WORDS: usize = 8;
const MAX_FIELD_CHARS: usize = 4000;
const MAX_TRIGGER_CHARS: usize = 120;
const SNIPPET_PREVIEW_CHARS: usize = 160;

/// Reads the clipboard text (tests replace it).
type Clipboard = fn() -> Option<String>;

fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{} {}", thousands(n), if n == 1 { one } else { many })
}

fn row_id(i: usize) -> ItemId {
    ROW_BASE + i.min(usize::from(ItemId::MAX - ROW_BASE)) as ItemId
}

fn toast(s: impl Into<String>) -> Effect {
    Effect::Toast(s.into())
}

/// Merge the clipboard (JSON or plain text, words and snippets) into the config.
fn import_clipboard(clip: Clipboard, cfg: &mut Config) -> Vec<Effect> {
    let Some(text) = clip().filter(|t| !t.trim().is_empty()) else {
        return vec![toast("The clipboard has no text to import.")];
    };
    if text.len() as u64 > MAX_IMPORT_BYTES {
        return vec![toast("The clipboard text is larger than 4 MB.")];
    }
    let (words, snippets) = parse_import(&text);
    if words.is_empty() && snippets.is_empty() {
        return vec![toast("No words or snippets found in the clipboard.")];
    }
    let w = add_words(&mut cfg.dictionary, &words);
    let s = merge_snippets(&mut cfg.snippets, snippets);
    if w + s == 0 {
        return vec![toast("Nothing new: those are already here.")];
    }
    let mut parts = Vec::new();
    if w > 0 {
        parts.push(plural(w, "word", "words"));
    }
    if s > 0 {
        parts.push(plural(s, "snippet", "snippets"));
    }
    vec![Effect::Saved, toast(format!("Imported {}", parts.join(" and ")))]
}

// ---------------------------------------------------------------------------------------------
// Dictionary
// ---------------------------------------------------------------------------------------------

const C_SNIPPETS: ItemId = 1;
/// The add field has one of the ids `F_INPUT..F_INPUT + INPUT_IDS` (see the module docs).
const F_INPUT: ItemId = 10;
const INPUT_IDS: ItemId = 80;
const B_ADD: ItemId = 3;
const F_FILTER: ItemId = 5;
const B_IMPORT: ItemId = 6;
const B_COPY: ItemId = 7;
const B_CLEAR: ItemId = 8;

/// What `virtual_rows` (which has no `Cx`) draws: refreshed by `items()`, which the renderer
/// always calls before it lays out the virtual rows.
#[derive(Default)]
struct WordCache {
    words: Vec<String>,
    filter: String,
    /// Indices into `words` that match `filter`.
    view: Vec<usize>,
}

struct DictionaryPage {
    /// The add field's text (also the text of the word being edited).
    input: String,
    error: Option<String>,
    /// Index into `cfg.dictionary` of the word loaded into the field for editing.
    editing: Option<usize>,
    /// The filter field's text.
    filter: String,
    /// Bumped whenever the model replaces the add field's text itself.
    epoch: ItemId,
    clipboard: Clipboard,
    cache: RefCell<WordCache>,
}

/// Dictionary words page (`Route::Dictionary`).
pub fn model() -> Box<dyn PageModel> {
    Box::new(DictionaryPage::new(crate::daemon::paste::read_clipboard_text))
}

impl DictionaryPage {
    fn new(clipboard: Clipboard) -> DictionaryPage {
        DictionaryPage { input: String::new(), error: None, editing: None, filter: String::new(), epoch: 0, clipboard, cache: RefCell::default() }
    }

    fn refresh(&self, words: &[String]) {
        let mut c = self.cache.borrow_mut();
        if c.words != words || c.filter != self.filter {
            c.words = words.to_vec();
            c.filter = self.filter.clone();
            c.view = filter_words(words, &self.filter);
        }
    }

    /// Id of the add field right now.
    fn input_id(&self) -> ItemId {
        F_INPUT + self.epoch % INPUT_IDS
    }

    fn is_input(&self, id: ItemId) -> bool {
        id == self.input_id()
    }

    /// Replace the add field's text (a new field, see the module docs).
    fn set_input(&mut self, text: String) {
        self.input = text;
        self.epoch = self.epoch.wrapping_add(1);
    }

    fn cancel_edit(&mut self) {
        self.editing = None;
        self.set_input(String::new());
        self.error = None;
    }

    /// Enter / blur / the Add button: add the typed words, or replace the word being edited.
    fn apply_input(&mut self, cx: &mut Cx) -> Vec<Effect> {
        let words = parse_words(&self.input);
        if let Some(i) = self.editing {
            if i < cx.cfg.dictionary.len() {
                let Some(first) = words.first() else {
                    self.cancel_edit();
                    return Vec::new();
                };
                let lower = first.to_lowercase();
                if cx.cfg.dictionary.iter().enumerate().any(|(j, w)| j != i && w.to_lowercase() == lower) {
                    self.error = Some("Already in the dictionary.".into());
                    return Vec::new();
                }
                cx.cfg.dictionary[i] = first.clone();
                let extra = add_words(&mut cx.cfg.dictionary, &words[1..]);
                self.cancel_edit();
                let mut fx = vec![Effect::Saved];
                if extra > 0 {
                    fx.push(toast(format!("Updated and added {}", plural(extra, "more word", "more words"))));
                }
                return fx;
            }
            self.editing = None;
        }
        if words.is_empty() {
            self.set_input(String::new());
            return Vec::new();
        }
        let added = add_words(&mut cx.cfg.dictionary, &words);
        if added == 0 {
            self.error = Some(if words.len() == 1 { "Already in the dictionary." } else { "These words are already in the dictionary." }.into());
            return Vec::new();
        }
        self.set_input(String::new());
        self.error = None;
        let mut fx = vec![Effect::Saved];
        if added > 1 {
            fx.push(toast(format!("Added {}", plural(added, "word", "words"))));
        }
        fx
    }

    fn count_note(&self, cfg: &Config, shown: usize) -> String {
        let n = cfg.dictionary.len();
        if n == 0 {
            return "Names, products and jargon your speech provider gets wrong go here. They are sent along as hints with every dictation.".into();
        }
        let chars = prompt_chars(&hint_words(&cfg.dictionary, &cfg.snippets));
        let base = format!("{} \u{b7} {} in the speech prompt", plural(n, "word", "words"), plural(chars, "character", "characters"));
        if self.filter.trim().is_empty() { base } else { format!("{shown} of {base}") }
    }
}

impl PageModel for DictionaryPage {
    fn title(&self) -> String {
        "Dictionary".into()
    }

    fn items(&self, cx: &Cx) -> Vec<Item> {
        self.refresh(&cx.cfg.dictionary);
        let shown = self.cache.borrow().view.len();
        let n = cx.cfg.dictionary.len();
        let mut v = Vec::new();
        v.push(Item::Choice {
            id: C_SNIPPETS,
            label: "Snippets".into(),
            value: if cx.cfg.snippets.is_empty() { "None".into() } else { plural(cx.cfg.snippets.len(), "snippet", "snippets") },
        });
        if let Some(w) = budget_warning(cx.cfg) {
            v.push(Item::Banner { tone: Tone::Warn, text: w });
        }
        let editing = self.editing.is_some();
        v.push(Item::Field {
            id: self.input_id(),
            label: if editing { "Edit word".into() } else { "Add words".into() },
            hint: if editing { "Enter saves. Clear the field to cancel.".into() } else { "Type a word and press Enter".into() },
            value: self.input.clone(),
            kind: FieldKind::Text,
            error: self.error.clone(),
            max_chars: MAX_FIELD_CHARS,
        });
        v.push(Item::Button { id: B_ADD, label: if editing { "Save word".into() } else { "Add".into() }, style: BtnStyle::Primary, enabled: !parse_words(&self.input).is_empty() });
        v.push(Item::Note(self.count_note(cx.cfg, shown)));
        if n > FILTER_MIN_WORDS || !self.filter.is_empty() {
            v.push(Item::Field {
                id: F_FILTER,
                label: "Filter".into(),
                hint: "Show only words containing\u{2026}".into(),
                value: self.filter.clone(),
                kind: FieldKind::Text,
                error: None,
                max_chars: 200,
            });
            if shown == 0 && n > 0 {
                v.push(Item::Note("No words match.".into()));
            }
        }
        v
    }

    fn virtual_rows(&self) -> Option<VirtualRows<'_>> {
        let count = self.cache.borrow().view.len();
        (count > 0).then(|| VirtualRows {
            count,
            row_h: ROW_H_WORD,
            row: Box::new(move |i| {
                let c = self.cache.borrow();
                let word = c.view.get(i).and_then(|&j| c.words.get(j)).cloned().unwrap_or_default();
                let mut r = Row::new(row_id(i), word, ROW_H_WORD);
                r.trailing = Trailing::Delete;
                Item::Row(r)
            }),
        })
    }

    fn tail_items(&self, cx: &Cx) -> Vec<Item> {
        let has = !cx.cfg.dictionary.is_empty();
        vec![
            Item::Divider,
            Item::Button { id: B_IMPORT, label: "Import from clipboard".into(), style: BtnStyle::Outline, enabled: true },
            Item::Button { id: B_COPY, label: "Copy as JSON".into(), style: BtnStyle::Outline, enabled: has },
            Item::Button { id: B_CLEAR, label: "Clear all".into(), style: BtnStyle::Destructive, enabled: has },
        ]
    }

    fn enter(&mut self, _cx: &mut Cx) {
        self.error = None;
    }

    fn event(&mut self, id: ItemId, ev: Event, cx: &mut Cx) -> Vec<Effect> {
        match (id, ev) {
            (C_SNIPPETS, Event::Activated) => vec![Effect::Navigate(Route::Snippets)],
            (i, Event::Edited(t)) if self.is_input(i) => {
                self.input = t;
                self.error = None;
                Vec::new()
            }
            (i, Event::Committed(t)) if self.is_input(i) => {
                self.input = t;
                self.apply_input(cx)
            }
            (B_ADD, Event::Pressed) => self.apply_input(cx),
            (F_FILTER, Event::Edited(t) | Event::Committed(t)) => {
                self.filter = t;
                Vec::new()
            }
            (B_IMPORT, Event::Pressed) => import_clipboard(self.clipboard, cx.cfg),
            (B_COPY, Event::Pressed) => {
                let n = cx.cfg.dictionary.len();
                vec![Effect::CopyText(export_words_json(&cx.cfg.dictionary)), toast(format!("Copied {} as JSON", plural(n, "word", "words")))]
            }
            (B_CLEAR, Event::Pressed) => {
                let n = cx.cfg.dictionary.len();
                vec![Effect::Confirm {
                    title: "Clear the dictionary?".into(),
                    body: format!("All {} are removed. Snippets are kept.", plural(n, "word", "words")),
                    action: "Clear all".into(),
                    destructive: true,
                    on_confirm: ConfirmId::DictionaryClear,
                }]
            }
            _ => Vec::new(),
        }
    }

    fn row_event(&mut self, index: usize, ev: Event, cx: &mut Cx) -> Vec<Effect> {
        let Some(&real) = filter_words(&cx.cfg.dictionary, &self.filter).get(index) else { return Vec::new() };
        match ev {
            Event::Activated => {
                self.editing = Some(real);
                self.set_input(cx.cfg.dictionary[real].clone());
                self.error = None;
                vec![Effect::Focus(self.input_id())]
            }
            Event::Deleted => {
                cx.cfg.dictionary.remove(real);
                match self.editing {
                    Some(e) if e == real => self.cancel_edit(),
                    Some(e) if e > real => self.editing = Some(e - 1),
                    _ => {}
                }
                vec![Effect::Saved]
            }
            _ => Vec::new(),
        }
    }

    fn confirmed(&mut self, id: ConfirmId, cx: &mut Cx) -> Vec<Effect> {
        if id != ConfirmId::DictionaryClear {
            return Vec::new();
        }
        cx.cfg.dictionary.clear();
        self.cancel_edit();
        self.filter.clear();
        vec![Effect::Saved, toast("Dictionary cleared")]
    }
}

// ---------------------------------------------------------------------------------------------
// Snippets
// ---------------------------------------------------------------------------------------------

const S_NEW: ItemId = 1;
const S_IMPORT: ItemId = 2;
const S_COPY: ItemId = 3;
const S_CLEAR: ItemId = 4;

struct SnippetsPage {
    clipboard: Clipboard,
    /// Refreshed by `items()` for `virtual_rows`.
    cache: RefCell<Vec<Snippet>>,
}

/// Snippets list (`Route::Snippets`).
pub fn snippets_model() -> Box<dyn PageModel> {
    Box::new(SnippetsPage { clipboard: crate::daemon::paste::read_clipboard_text, cache: RefCell::default() })
}

impl PageModel for SnippetsPage {
    fn title(&self) -> String {
        "Snippets".into()
    }

    fn items(&self, cx: &Cx) -> Vec<Item> {
        if *self.cache.borrow() != cx.cfg.snippets {
            *self.cache.borrow_mut() = cx.cfg.snippets.clone();
        }
        let mut v = Vec::new();
        if let Some(w) = budget_warning(cx.cfg) {
            v.push(Item::Banner { tone: Tone::Warn, text: w });
        }
        v.push(Item::Button { id: S_NEW, label: "New snippet".into(), style: BtnStyle::Primary, enabled: true });
        v.push(Item::Note(if cx.cfg.snippets.is_empty() {
            "Say a short trigger while dictating and OpenTalk pastes the longer text you saved for it, like an email address or a sign-off.".into()
        } else {
            format!("{}. Say a trigger while dictating to paste its text.", plural(cx.cfg.snippets.len(), "snippet", "snippets"))
        }));
        v
    }

    fn virtual_rows(&self) -> Option<VirtualRows<'_>> {
        let count = self.cache.borrow().len();
        (count > 0).then(|| VirtualRows {
            count,
            row_h: ROW_H_SNIPPET,
            row: Box::new(move |i| {
                let c = self.cache.borrow();
                let s = c.get(i);
                let mut r = Row::new(row_id(i), s.map(|s| one_line(&s.trigger)).unwrap_or_default(), ROW_H_SNIPPET);
                r.secondary = s.map(|s| truncate(&one_line(&s.replacement), SNIPPET_PREVIEW_CHARS)).unwrap_or_default();
                r.trailing = Trailing::Delete;
                Item::Row(r)
            }),
        })
    }

    fn tail_items(&self, cx: &Cx) -> Vec<Item> {
        let has = !cx.cfg.snippets.is_empty();
        vec![
            Item::Divider,
            Item::Button { id: S_IMPORT, label: "Import from clipboard".into(), style: BtnStyle::Outline, enabled: true },
            Item::Button { id: S_COPY, label: "Copy as JSON".into(), style: BtnStyle::Outline, enabled: has },
            Item::Button { id: S_CLEAR, label: "Clear all".into(), style: BtnStyle::Destructive, enabled: has },
        ]
    }

    fn event(&mut self, id: ItemId, ev: Event, cx: &mut Cx) -> Vec<Effect> {
        if ev != Event::Pressed {
            return Vec::new();
        }
        match id {
            S_NEW => vec![Effect::Navigate(Route::SnippetEdit(None))],
            S_IMPORT => import_clipboard(self.clipboard, cx.cfg),
            S_COPY => {
                let n = cx.cfg.snippets.len();
                vec![Effect::CopyText(export_snippets_json(&cx.cfg.snippets)), toast(format!("Copied {} as JSON", plural(n, "snippet", "snippets")))]
            }
            S_CLEAR => vec![Effect::Confirm {
                title: "Delete all snippets?".into(),
                body: format!("All {} are removed. The dictionary is kept.", plural(cx.cfg.snippets.len(), "snippet", "snippets")),
                action: "Delete all".into(),
                destructive: true,
                on_confirm: ConfirmId::SnippetsClear,
            }],
            _ => Vec::new(),
        }
    }

    fn row_event(&mut self, index: usize, ev: Event, cx: &mut Cx) -> Vec<Effect> {
        let Some(s) = cx.cfg.snippets.get(index) else { return Vec::new() };
        match ev {
            Event::Activated => vec![Effect::Navigate(Route::SnippetEdit(Some(index)))],
            Event::Deleted => vec![Effect::Confirm {
                title: "Delete this snippet?".into(),
                body: format!("\u{201c}{}\u{201d} will no longer be expanded.", truncate(&one_line(&s.trigger), 60)),
                action: "Delete".into(),
                destructive: true,
                on_confirm: ConfirmId::SnippetDelete(index),
            }],
            _ => Vec::new(),
        }
    }

    fn confirmed(&mut self, id: ConfirmId, cx: &mut Cx) -> Vec<Effect> {
        match id {
            ConfirmId::SnippetsClear => {
                cx.cfg.snippets.clear();
                vec![Effect::Saved, toast("Snippets deleted")]
            }
            ConfirmId::SnippetDelete(i) if i < cx.cfg.snippets.len() => {
                cx.cfg.snippets.remove(i);
                vec![Effect::Saved, toast("Snippet deleted")]
            }
            _ => Vec::new(),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Snippet editor
// ---------------------------------------------------------------------------------------------

const E_TRIGGER: ItemId = 1;
const E_REPLACEMENT: ItemId = 2;
const E_SAVE: ItemId = 3;
const E_DELETE: ItemId = 4;

struct SnippetEditPage {
    /// The snippet being edited (`None`: a new one).
    index: Option<usize>,
    trigger: String,
    replacement: String,
}

/// Snippet editor (`Route::SnippetEdit`): `Some(i)` edits `Config.snippets[i]`, `None` adds one.
pub fn snippet_edit_model(index: Option<usize>) -> Box<dyn PageModel> {
    Box::new(SnippetEditPage { index, trigger: String::new(), replacement: String::new() })
}

impl SnippetEditPage {
    fn skip(&self) -> usize {
        self.index.unwrap_or(usize::MAX)
    }

    fn clash(&self, cfg: &Config) -> bool {
        trigger_taken(&cfg.snippets, self.skip(), &self.trigger)
    }

    fn valid(&self, cfg: &Config) -> bool {
        !self.trigger.trim().is_empty() && !self.replacement.trim().is_empty() && !self.clash(cfg)
    }
}

impl PageModel for SnippetEditPage {
    fn title(&self) -> String {
        if self.index.is_some() { "Edit snippet" } else { "New snippet" }.into()
    }

    fn items(&self, cx: &Cx) -> Vec<Item> {
        vec![
            Item::Field {
                id: E_TRIGGER,
                label: "Trigger".into(),
                hint: "What you say, like \u{201c}my email\u{201d}".into(),
                value: self.trigger.clone(),
                kind: FieldKind::Text,
                error: self.clash(cx.cfg).then(|| "Another snippet already uses this trigger.".into()),
                max_chars: MAX_TRIGGER_CHARS,
            },
            Item::Field {
                id: E_REPLACEMENT,
                label: "Replacement".into(),
                hint: "The text to paste instead".into(),
                value: self.replacement.clone(),
                kind: FieldKind::Multiline { rows: 5 },
                error: None,
                max_chars: MAX_FIELD_CHARS,
            },
            Item::Note("The trigger is matched as whole words and ignores capitals. A trigger inside a longer phrase is replaced there too.".into()),
            Item::Button { id: E_SAVE, label: "Save".into(), style: BtnStyle::Primary, enabled: self.valid(cx.cfg) },
        ]
        .into_iter()
        .chain(self.index.map(|_| Item::Button { id: E_DELETE, label: "Delete snippet".into(), style: BtnStyle::Destructive, enabled: true }))
        .collect()
    }

    fn enter(&mut self, cx: &mut Cx) {
        match self.index.and_then(|i| cx.cfg.snippets.get(i)) {
            Some(s) => {
                self.trigger = s.trigger.clone();
                self.replacement = s.replacement.clone();
            }
            None => self.index = None,
        }
    }

    fn event(&mut self, id: ItemId, ev: Event, cx: &mut Cx) -> Vec<Effect> {
        match (id, ev) {
            (E_TRIGGER, Event::Edited(t) | Event::Committed(t)) => self.trigger = t,
            (E_REPLACEMENT, Event::Edited(t) | Event::Committed(t)) => self.replacement = t,
            (E_SAVE, Event::Pressed) if self.valid(cx.cfg) => {
                let s = Snippet { trigger: self.trigger.trim().to_string(), replacement: self.replacement.clone() };
                match self.index.filter(|&i| i < cx.cfg.snippets.len()) {
                    Some(i) => cx.cfg.snippets[i] = s,
                    None => cx.cfg.snippets.push(s),
                }
                return vec![Effect::Saved, toast("Snippet saved"), Effect::Back];
            }
            (E_DELETE, Event::Pressed) => {
                let Some(i) = self.index else { return Vec::new() };
                return vec![Effect::Confirm {
                    title: "Delete this snippet?".into(),
                    body: format!("\u{201c}{}\u{201d} will no longer be expanded.", truncate(&one_line(&self.trigger), 60)),
                    action: "Delete".into(),
                    destructive: true,
                    on_confirm: ConfirmId::SnippetDelete(i),
                }];
            }
            _ => {}
        }
        Vec::new()
    }

    fn confirmed(&mut self, id: ConfirmId, cx: &mut Cx) -> Vec<Effect> {
        match (id, self.index) {
            (ConfirmId::SnippetDelete(i), Some(mine)) if i == mine => {
                if i < cx.cfg.snippets.len() {
                    cx.cfg.snippets.remove(i);
                }
                vec![Effect::Saved, toast("Snippet deleted"), Effect::Back]
            }
            _ => Vec::new(),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::super::history::testkit::{dump, snapshot};
    use super::*;
    use crate::core::config::SttProviderId;

    fn sn(t: &str, r: &str) -> Snippet {
        Snippet { trigger: t.into(), replacement: r.into() }
    }

    fn cfg_with(words: &[&str]) -> Config {
        Config { dictionary: words.iter().map(|s| s.to_string()).collect(), ..Config::default() }
    }

    fn no_clip() -> Option<String> {
        None
    }

    fn page() -> DictionaryPage {
        DictionaryPage::new(no_clip)
    }

    fn cx(cfg: &mut Config) -> Cx<'_> {
        Cx::new(cfg, false, 0)
    }

    fn input_field(p: &DictionaryPage, cfg: &mut Config) -> Item {
        p.items(&cx(cfg)).into_iter().find(|i| i.id() == Some(p.input_id())).expect("the add field")
    }

    fn saved(fx: &[Effect]) -> bool {
        fx.contains(&Effect::Saved)
    }

    fn toasts(fx: &[Effect]) -> Vec<String> {
        fx.iter().filter_map(|e| if let Effect::Toast(t) = e { Some(t.clone()) } else { None }).collect()
    }

    fn words_in_view(p: &DictionaryPage, cfg: &mut Config) -> Vec<String> {
        let c = cx(cfg);
        let _ = p.items(&c);
        let rows = p.virtual_rows();
        rows.map(|v| (0..v.count).map(|i| if let Item::Row(r) = (v.row)(i) { r.primary } else { String::new() }).collect()).unwrap_or_default()
    }

    // ----- dictionary

    #[test]
    fn empty_dictionary_snapshot() {
        let mut cfg = Config::default();
        let p = page();
        let snap = snapshot(&p, &cx(&mut cfg));
        println!("{snap}");
        let want = [
            "# Dictionary",
            "#1 choice Snippets = None",
            "#10 field Add words [text] value=\"\" hint=\"Type a word and press Enter\" max=4000",
            "#3 button Add [Primary] (disabled)",
            "note: Names, products and jargon your speech provider gets wrong go here. They are sent along as hints with every dictation.",
            "---",
            "#6 button Import from clipboard [Outline]",
            "#7 button Copy as JSON [Outline] (disabled)",
            "#8 button Clear all [Destructive] (disabled)",
        ]
        .join("\n");
        assert_eq!(snap, want);
    }

    #[test]
    fn typing_words_and_pressing_enter_adds_them() {
        let mut cfg = cfg_with(&["Kubernetes"]);
        let mut p = page();
        p.event(p.input_id(), Event::Edited("Terraform, kubernetes; Grafana\nOpenTalk".into()), &mut cx(&mut cfg));
        let items = p.items(&cx(&mut cfg));
        assert!(items.iter().any(|i| matches!(i, Item::Button { id: B_ADD, enabled: true, .. })));
        let fx = p.event(p.input_id(), Event::Committed("Terraform, kubernetes; Grafana\nOpenTalk".into()), &mut cx(&mut cfg));
        assert_eq!(cfg.dictionary, vec!["Kubernetes", "Terraform", "Grafana", "OpenTalk"], "duplicates (any case) are skipped");
        assert!(saved(&fx));
        assert_eq!(toasts(&fx), vec!["Added 3 words"]);
        let field = input_field(&p, &mut cfg);
        assert!(matches!(&field, Item::Field { value, error: None, .. } if value.is_empty()), "the field is cleared for the next word");
        assert_ne!(field.id(), Some(F_INPUT), "...as a new field, because a focused one would keep its text");
    }

    #[test]
    fn the_add_button_adds_what_was_typed() {
        let mut cfg = Config::default();
        let mut p = page();
        p.event(p.input_id(), Event::Edited("Whisper".into()), &mut cx(&mut cfg));
        let fx = p.event(B_ADD, Event::Pressed, &mut cx(&mut cfg));
        assert_eq!(cfg.dictionary, vec!["Whisper"]);
        assert_eq!(fx, vec![Effect::Saved], "a single word needs no toast");
        // Pressing it again with an empty field changes nothing.
        assert!(p.event(B_ADD, Event::Pressed, &mut cx(&mut cfg)).is_empty());
        assert_eq!(cfg.dictionary.len(), 1);
    }

    #[test]
    fn adding_a_duplicate_shows_an_error_until_the_next_keystroke() {
        let mut cfg = cfg_with(&["Whisper"]);
        let mut p = page();
        let fx = p.event(p.input_id(), Event::Committed("whisper".into()), &mut cx(&mut cfg));
        assert!(fx.is_empty() && cfg.dictionary == vec!["Whisper"]);
        let err = |p: &DictionaryPage, cfg: &mut Config| match input_field(p, cfg) {
            Item::Field { error, value, .. } => (error, value),
            _ => panic!(),
        };
        assert_eq!(err(&p, &mut cfg), (Some("Already in the dictionary.".to_string()), "whisper".to_string()));
        p.event(p.input_id(), Event::Edited("whisperx".into()), &mut cx(&mut cfg));
        assert_eq!(err(&p, &mut cfg).0, None);
    }

    #[test]
    fn a_blank_commit_adds_nothing_and_clears_the_field() {
        let mut cfg = Config::default();
        let mut p = page();
        assert!(p.event(p.input_id(), Event::Committed("  , ;".into()), &mut cx(&mut cfg)).is_empty());
        assert!(cfg.dictionary.is_empty() && p.input.is_empty());
    }

    #[test]
    fn tapping_a_word_loads_it_for_editing_and_enter_replaces_it() {
        let mut cfg = cfg_with(&["alpha", "beta", "gamma"]);
        let mut p = page();
        let fx = p.row_event(1, Event::Activated, &mut cx(&mut cfg));
        assert_eq!(fx, vec![Effect::Focus(p.input_id())]);
        let snap = snapshot(&p, &cx(&mut cfg));
        println!("{snap}");
        assert!(snap.contains("#11 field Edit word [text] value=\"beta\" hint=\"Enter saves. Clear the field to cancel.\""), "{snap}");
        assert!(snap.contains("#3 button Save word [Primary]") && !snap.contains("Cancel"), "{snap}");
        let fx = p.event(p.input_id(), Event::Committed("Beta Test".into()), &mut cx(&mut cfg));
        assert_eq!(cfg.dictionary, vec!["alpha", "Beta Test", "gamma"]);
        assert_eq!(fx, vec![Effect::Saved]);
        assert!(p.editing.is_none() && p.input.is_empty());
        assert!(!snapshot(&p, &cx(&mut cfg)).contains("Cancel"));
    }

    #[test]
    fn editing_into_another_existing_word_is_refused_but_a_case_change_is_fine() {
        let mut cfg = cfg_with(&["alpha", "beta"]);
        let mut p = page();
        p.row_event(0, Event::Activated, &mut cx(&mut cfg));
        let fx = p.event(p.input_id(), Event::Committed("BETA".into()), &mut cx(&mut cfg));
        assert!(fx.is_empty() && cfg.dictionary == vec!["alpha", "beta"] && p.editing == Some(0));
        assert_eq!(p.error.as_deref(), Some("Already in the dictionary."));
        p.event(p.input_id(), Event::Edited("Alpha".into()), &mut cx(&mut cfg));
        let fx = p.event(B_ADD, Event::Pressed, &mut cx(&mut cfg));
        assert!(saved(&fx));
        assert_eq!(cfg.dictionary, vec!["Alpha", "beta"]);
    }

    #[test]
    fn emptying_the_field_cancels_an_edit_and_leaves_the_word_alone() {
        // No Cancel button: the renderer commits a changed field when the pointer goes down on
        // any other control, so a Cancel press would arrive after the edit had already been saved.
        let mut cfg = cfg_with(&["alpha"]);
        let mut p = page();
        p.row_event(0, Event::Activated, &mut cx(&mut cfg));
        assert!(p.event(p.input_id(), Event::Committed(String::new()), &mut cx(&mut cfg)).is_empty());
        assert!(p.editing.is_none() && cfg.dictionary == vec!["alpha"]);
    }

    #[test]
    fn deleting_a_row_removes_that_word_and_keeps_the_edit_pointing_at_its_word() {
        let mut cfg = cfg_with(&["alpha", "beta", "gamma"]);
        let mut p = page();
        p.row_event(2, Event::Activated, &mut cx(&mut cfg)); // editing gamma
        let fx = p.row_event(0, Event::Deleted, &mut cx(&mut cfg));
        assert_eq!(fx, vec![Effect::Saved]);
        assert_eq!(cfg.dictionary, vec!["beta", "gamma"]);
        assert_eq!(p.editing, Some(1), "still gamma");
        p.row_event(1, Event::Deleted, &mut cx(&mut cfg));
        assert!(p.editing.is_none() && p.input.is_empty(), "deleting the word being edited ends the edit");
        assert!(p.row_event(7, Event::Deleted, &mut cx(&mut cfg)).is_empty(), "stale index");
    }

    #[test]
    fn long_lists_get_a_filter_that_maps_rows_back_to_the_right_words() {
        let names: Vec<String> = (1..=12).map(|i| format!("word{i:02}")).collect();
        let mut cfg = cfg_with(&names.iter().map(String::as_str).collect::<Vec<_>>());
        let mut p = page();
        let snap = snapshot(&p, &cx(&mut cfg));
        println!("{snap}");
        assert!(snap.contains("#5 field Filter") && snap.contains("note: 12 words \u{b7} 94 characters in the speech prompt"), "{snap}");
        assert!(snap.contains("[virtual rows: 12 x 32px]") && snap.contains("#100 row h=32 | word01 |  |  | chips=[] | x"), "{snap}");
        p.event(F_FILTER, Event::Edited("WORD1".into()), &mut cx(&mut cfg));
        assert_eq!(words_in_view(&p, &mut cfg), vec!["word10", "word11", "word12"]);
        assert!(snapshot(&p, &cx(&mut cfg)).contains("note: 3 of 12 words \u{b7} 94 characters in the speech prompt"));
        // Row 1 of the filtered list is "word11", whatever its place in the whole list.
        p.row_event(1, Event::Activated, &mut cx(&mut cfg));
        assert_eq!(p.input, "word11");
        p.row_event(0, Event::Deleted, &mut cx(&mut cfg));
        assert!(!cfg.dictionary.contains(&"word10".to_string()) && cfg.dictionary.len() == 11);
        p.event(F_FILTER, Event::Edited("nothing".into()), &mut cx(&mut cfg));
        assert!(words_in_view(&p, &mut cfg).is_empty());
        assert!(p.items(&cx(&mut cfg)).iter().any(|i| matches!(i, Item::Note(n) if n == "No words match.")));
    }

    #[test]
    fn the_budget_banner_appears_when_the_provider_cannot_take_every_word() {
        let long: Vec<String> = (0..120).map(|i| format!("term{i:03}")).collect();
        let mut cfg = Config { dictionary: long, ..Config::default() };
        cfg.stt.provider = SttProviderId::Groq; // 890 characters
        let p = page();
        let items = p.items(&cx(&mut cfg));
        let banner = items.iter().find_map(|i| if let Item::Banner { tone: Tone::Warn, text } = i { Some(text.clone()) } else { None });
        assert!(banner.as_deref().is_some_and(|b| b.contains("Groq") && b.contains("only the first")), "{banner:?}");
        cfg.dictionary.truncate(5);
        assert!(!p.items(&cx(&mut cfg)).iter().any(|i| matches!(i, Item::Banner { .. })));
    }

    fn clip_words() -> Option<String> {
        Some(r#"{"dictionary":["Terraform","alpha"],"snippets":[{"trigger":"my email","replacement":"me@example.com"}]}"#.into())
    }

    fn clip_text() -> Option<String> {
        Some("one, two\nthree".into())
    }

    fn clip_blank() -> Option<String> {
        Some("  \n ".into())
    }

    fn clip_junk() -> Option<String> {
        Some("[{\"x\":1}]".into())
    }

    #[test]
    fn import_from_clipboard_merges_words_and_snippets() {
        let mut cfg = cfg_with(&["Alpha"]);
        let mut p = DictionaryPage::new(clip_words);
        let fx = p.event(B_IMPORT, Event::Pressed, &mut cx(&mut cfg));
        assert_eq!(cfg.dictionary, vec!["Alpha", "Terraform"]);
        assert_eq!(cfg.snippets, vec![sn("my email", "me@example.com")]);
        assert_eq!(fx, vec![Effect::Saved, toast("Imported 1 word and 1 snippet")]);
        // Importing the same thing again adds nothing.
        let fx = p.event(B_IMPORT, Event::Pressed, &mut cx(&mut cfg));
        assert_eq!(fx, vec![toast("Nothing new: those are already here.")]);
        // Plain text works too.
        p.clipboard = clip_text;
        let fx = p.event(B_IMPORT, Event::Pressed, &mut cx(&mut cfg));
        assert_eq!(fx, vec![Effect::Saved, toast("Imported 3 words")]);
        // Nothing usable on the clipboard.
        p.clipboard = no_clip;
        assert_eq!(p.event(B_IMPORT, Event::Pressed, &mut cx(&mut cfg)), vec![toast("The clipboard has no text to import.")]);
        p.clipboard = clip_blank;
        assert_eq!(p.event(B_IMPORT, Event::Pressed, &mut cx(&mut cfg)), vec![toast("The clipboard has no text to import.")]);
        p.clipboard = clip_junk;
        assert_eq!(p.event(B_IMPORT, Event::Pressed, &mut cx(&mut cfg)), vec![toast("No words or snippets found in the clipboard.")]);
    }

    #[test]
    fn copy_as_json_exports_what_import_reads_back() {
        let mut cfg = cfg_with(&["Terraform", "OpenTalk"]);
        let mut p = page();
        let fx = p.event(B_COPY, Event::Pressed, &mut cx(&mut cfg));
        let [Effect::CopyText(json), Effect::Toast(t)] = &fx[..] else { panic!("{fx:?}") };
        assert_eq!(t, "Copied 2 words as JSON");
        let (words, snippets) = parse_import(json);
        assert_eq!(words, vec!["Terraform", "OpenTalk"]);
        assert!(snippets.is_empty());
    }

    #[test]
    fn clear_all_confirms_first_and_keeps_snippets() {
        let mut cfg = cfg_with(&["a", "b"]);
        cfg.snippets.push(sn("x", "y"));
        let mut p = page();
        p.row_event(0, Event::Activated, &mut cx(&mut cfg));
        let fx = p.event(B_CLEAR, Event::Pressed, &mut cx(&mut cfg));
        assert!(matches!(&fx[..], [Effect::Confirm { destructive: true, on_confirm: ConfirmId::DictionaryClear, body, .. }] if body == "All 2 words are removed. Snippets are kept."));
        assert_eq!(cfg.dictionary.len(), 2);
        assert!(p.confirmed(ConfirmId::SnippetsClear, &mut cx(&mut cfg)).is_empty());
        let fx = p.confirmed(ConfirmId::DictionaryClear, &mut cx(&mut cfg));
        assert_eq!(fx, vec![Effect::Saved, toast("Dictionary cleared")]);
        assert!(cfg.dictionary.is_empty() && cfg.snippets.len() == 1 && p.editing.is_none());
    }

    #[test]
    fn the_snippets_link_shows_the_count_and_navigates() {
        let mut cfg = Config { snippets: vec![sn("a", "b"), sn("c", "d")], ..Config::default() };
        let mut p = page();
        assert!(snapshot(&p, &cx(&mut cfg)).contains("#1 choice Snippets = 2 snippets"));
        assert_eq!(p.event(C_SNIPPETS, Event::Activated, &mut cx(&mut cfg)), vec![Effect::Navigate(Route::Snippets)]);
    }

    // ----- snippets

    #[test]
    fn snippets_snapshot_lists_rows_with_previews() {
        let mut cfg = Config { snippets: vec![sn("my email", "me@example.com"), sn("sign off", "Best regards,\n  Pat\n\n(sent by voice)")], ..Config::default() };
        let p = SnippetsPage { clipboard: no_clip, cache: RefCell::default() };
        let snap = snapshot(&p, &cx(&mut cfg));
        println!("{snap}");
        let want = [
            "# Snippets",
            "#1 button New snippet [Primary]",
            "note: 2 snippets. Say a trigger while dictating to paste its text.",
            "[virtual rows: 2 x 44px]",
            "#100 row h=44 | my email | me@example.com |  | chips=[] | x",
            "#101 row h=44 | sign off | Best regards, Pat (sent by voice) |  | chips=[] | x",
            "---",
            "#2 button Import from clipboard [Outline]",
            "#3 button Copy as JSON [Outline]",
            "#4 button Clear all [Destructive]",
        ]
        .join("\n");
        assert_eq!(snap, want);
    }

    #[test]
    fn empty_snippets_page_explains_itself_and_has_no_rows() {
        let mut cfg = Config::default();
        let p = SnippetsPage { clipboard: no_clip, cache: RefCell::default() };
        let snap = snapshot(&p, &cx(&mut cfg));
        assert!(snap.contains("note: Say a short trigger while dictating") && !snap.contains("virtual rows"), "{snap}");
        assert!(snap.contains("#3 button Copy as JSON [Outline] (disabled)") && snap.contains("#4 button Clear all [Destructive] (disabled)"), "{snap}");
    }

    #[test]
    fn snippet_rows_open_the_editor_and_delete_asks_first() {
        let mut cfg = Config { snippets: vec![sn("a", "1"), sn("b", "2"), sn("c", "3")], ..Config::default() };
        let mut p = SnippetsPage { clipboard: no_clip, cache: RefCell::default() };
        assert_eq!(p.event(S_NEW, Event::Pressed, &mut cx(&mut cfg)), vec![Effect::Navigate(Route::SnippetEdit(None))]);
        assert_eq!(p.row_event(1, Event::Activated, &mut cx(&mut cfg)), vec![Effect::Navigate(Route::SnippetEdit(Some(1)))]);
        let fx = p.row_event(1, Event::Deleted, &mut cx(&mut cfg));
        assert!(matches!(&fx[..], [Effect::Confirm { on_confirm: ConfirmId::SnippetDelete(1), body, .. }] if body == "\u{201c}b\u{201d} will no longer be expanded."));
        assert_eq!(cfg.snippets.len(), 3);
        let fx = p.confirmed(ConfirmId::SnippetDelete(1), &mut cx(&mut cfg));
        assert_eq!(fx, vec![Effect::Saved, toast("Snippet deleted")]);
        assert_eq!(cfg.snippets, vec![sn("a", "1"), sn("c", "3")]);
        assert!(p.confirmed(ConfirmId::SnippetDelete(9), &mut cx(&mut cfg)).is_empty(), "stale index");
        assert!(p.row_event(5, Event::Activated, &mut cx(&mut cfg)).is_empty());
    }

    #[test]
    fn snippets_import_export_and_clear() {
        let mut cfg = Config::default();
        let mut p = SnippetsPage { clipboard: clip_words, cache: RefCell::default() };
        let fx = p.event(S_IMPORT, Event::Pressed, &mut cx(&mut cfg));
        assert_eq!(fx, vec![Effect::Saved, toast("Imported 2 words and 1 snippet")]);
        assert_eq!(cfg.snippets.len(), 1);
        let fx = p.event(S_COPY, Event::Pressed, &mut cx(&mut cfg));
        let [Effect::CopyText(json), Effect::Toast(t)] = &fx[..] else { panic!("{fx:?}") };
        assert_eq!(t, "Copied 1 snippet as JSON");
        assert_eq!(parse_import(json).1, vec![sn("my email", "me@example.com")]);
        let fx = p.event(S_CLEAR, Event::Pressed, &mut cx(&mut cfg));
        assert!(matches!(&fx[..], [Effect::Confirm { on_confirm: ConfirmId::SnippetsClear, .. }]));
        assert_eq!(p.confirmed(ConfirmId::SnippetsClear, &mut cx(&mut cfg)), vec![Effect::Saved, toast("Snippets deleted")]);
        assert!(cfg.snippets.is_empty() && cfg.dictionary.len() == 2, "words imported by the same paste stay");
    }

    // ----- snippet editor

    fn editor(index: Option<usize>, cfg: &mut Config) -> SnippetEditPage {
        let mut p = SnippetEditPage { index, trigger: String::new(), replacement: String::new() };
        p.enter(&mut cx(cfg));
        p
    }

    #[test]
    fn new_snippet_snapshot_and_save() {
        let mut cfg = Config::default();
        let mut p = editor(None, &mut cfg);
        let snap = snapshot(&p, &cx(&mut cfg));
        println!("{snap}");
        let want = [
            "# New snippet",
            "#1 field Trigger [text] value=\"\" hint=\"What you say, like \u{201c}my email\u{201d}\" max=120",
            "#2 field Replacement [multiline5] value=\"\" hint=\"The text to paste instead\" max=4000",
            "note: The trigger is matched as whole words and ignores capitals. A trigger inside a longer phrase is replaced there too.",
            "#3 button Save [Primary] (disabled)",
        ]
        .join("\n");
        assert_eq!(snap, want);
        assert!(p.event(E_SAVE, Event::Pressed, &mut cx(&mut cfg)).is_empty(), "disabled button does nothing");
        p.event(E_TRIGGER, Event::Edited("  my email ".into()), &mut cx(&mut cfg));
        p.event(E_REPLACEMENT, Event::Edited("me@example.com\nsecond line".into()), &mut cx(&mut cfg));
        assert!(!snapshot(&p, &cx(&mut cfg)).contains("(disabled)"), "Save is enabled once both fields are filled");
        let fx = p.event(E_SAVE, Event::Pressed, &mut cx(&mut cfg));
        assert_eq!(fx, vec![Effect::Saved, toast("Snippet saved"), Effect::Back]);
        assert_eq!(cfg.snippets, vec![sn("my email", "me@example.com\nsecond line")], "trigger trimmed, replacement verbatim");
    }

    #[test]
    fn editing_an_existing_snippet_loads_and_replaces_it() {
        let mut cfg = Config { snippets: vec![sn("a", "1"), sn("b", "2")], ..Config::default() };
        let mut p = editor(Some(1), &mut cfg);
        assert_eq!((p.trigger.as_str(), p.replacement.as_str()), ("b", "2"));
        assert_eq!(p.title(), "Edit snippet");
        let snap = snapshot(&p, &cx(&mut cfg));
        println!("{snap}");
        assert!(snap.contains("value=\"b\"") && snap.ends_with("#4 button Delete snippet [Destructive]"), "{snap}");
        p.event(E_REPLACEMENT, Event::Committed("two".into()), &mut cx(&mut cfg));
        p.event(E_SAVE, Event::Pressed, &mut cx(&mut cfg));
        assert_eq!(cfg.snippets, vec![sn("a", "1"), sn("b", "two")], "its own trigger is not a clash");
    }

    #[test]
    fn a_duplicate_trigger_is_an_error_and_blocks_saving() {
        let mut cfg = Config { snippets: vec![sn("my email", "x"), sn("other", "y")], ..Config::default() };
        let mut p = editor(Some(1), &mut cfg);
        p.event(E_TRIGGER, Event::Edited("My Email".into()), &mut cx(&mut cfg));
        let snap = snapshot(&p, &cx(&mut cfg));
        println!("{snap}");
        assert!(snap.contains("ERROR=Another snippet already uses this trigger.") && snap.contains("#3 button Save [Primary] (disabled)"), "{snap}");
        assert!(p.event(E_SAVE, Event::Pressed, &mut cx(&mut cfg)).is_empty());
        assert_eq!(cfg.snippets[1], sn("other", "y"));
        p.event(E_TRIGGER, Event::Edited("My Email 2".into()), &mut cx(&mut cfg));
        assert!(!snapshot(&p, &cx(&mut cfg)).contains("ERROR"));
        // A new snippet checks against every existing one.
        let mut n = editor(None, &mut cfg);
        n.event(E_TRIGGER, Event::Edited("OTHER".into()), &mut cx(&mut cfg));
        assert!(snapshot(&n, &cx(&mut cfg)).contains("ERROR=Another snippet"));
    }

    #[test]
    fn an_empty_replacement_cannot_be_saved() {
        let mut cfg = Config::default();
        let mut p = editor(None, &mut cfg);
        p.event(E_TRIGGER, Event::Edited("sig".into()), &mut cx(&mut cfg));
        p.event(E_REPLACEMENT, Event::Edited("  \n ".into()), &mut cx(&mut cfg));
        assert!(p.event(E_SAVE, Event::Pressed, &mut cx(&mut cfg)).is_empty() && cfg.snippets.is_empty());
    }

    #[test]
    fn deleting_from_the_editor_confirms_and_goes_back() {
        let mut cfg = Config { snippets: vec![sn("a", "1"), sn("b", "2")], ..Config::default() };
        let mut p = editor(Some(0), &mut cfg);
        let fx = p.event(E_DELETE, Event::Pressed, &mut cx(&mut cfg));
        assert!(matches!(&fx[..], [Effect::Confirm { on_confirm: ConfirmId::SnippetDelete(0), destructive: true, .. }]));
        assert!(p.confirmed(ConfirmId::SnippetDelete(1), &mut cx(&mut cfg)).is_empty());
        let fx = p.confirmed(ConfirmId::SnippetDelete(0), &mut cx(&mut cfg));
        assert_eq!(fx, vec![Effect::Saved, toast("Snippet deleted"), Effect::Back]);
        assert_eq!(cfg.snippets, vec![sn("b", "2")]);
        // A new snippet has no Delete button.
        let n = editor(None, &mut cfg);
        assert!(!snapshot(&n, &cx(&mut cfg)).contains("Delete"));
    }

    #[test]
    fn an_index_that_no_longer_exists_becomes_a_new_snippet() {
        let mut cfg = Config::default();
        let mut p = editor(Some(4), &mut cfg);
        assert_eq!(p.index, None);
        assert_eq!(p.title(), "New snippet");
        p.event(E_TRIGGER, Event::Edited("a".into()), &mut cx(&mut cfg));
        p.event(E_REPLACEMENT, Event::Edited("b".into()), &mut cx(&mut cfg));
        p.event(E_SAVE, Event::Pressed, &mut cx(&mut cfg));
        assert_eq!(cfg.snippets, vec![sn("a", "b")]);
        // The same with the list shrinking while the editor is open.
        let mut cfg = Config { snippets: vec![sn("x", "1")], ..Config::default() };
        let mut p = editor(Some(0), &mut cfg);
        cfg.snippets.clear();
        p.event(E_REPLACEMENT, Event::Edited("2".into()), &mut cx(&mut cfg));
        p.event(E_SAVE, Event::Pressed, &mut cx(&mut cfg));
        assert_eq!(cfg.snippets, vec![sn("x", "2")]);
    }

    #[test]
    fn item_ids_are_unique_within_each_page() {
        let mut cfg = Config { dictionary: (0..20).map(|i| format!("w{i}")).collect(), snippets: vec![sn("a", "b")], ..Config::default() };
        let check = |m: &dyn PageModel, cfg: &mut Config| {
            let c = cx(cfg);
            let mut ids: Vec<ItemId> = m.items(&c).iter().chain(m.tail_items(&c).iter()).filter_map(Item::id).collect();
            if let Some(v) = m.virtual_rows() {
                ids.extend((0..v.count).filter_map(|i| (v.row)(i).id()));
            }
            let n = ids.len();
            ids.sort_unstable();
            ids.dedup();
            assert_eq!(ids.len(), n, "{}", m.title());
        };
        let mut d = page();
        d.row_event(0, Event::Activated, &mut cx(&mut cfg));
        check(&d, &mut cfg);
        check(&SnippetsPage { clipboard: no_clip, cache: RefCell::default() }, &mut cfg);
        check(&editor(Some(0), &mut cfg), &mut cfg);
    }

    #[test]
    fn real_constructors_build_pages() {
        let mut cfg = Config::default();
        for mut m in [model(), snippets_model(), snippet_edit_model(None), snippet_edit_model(Some(3))] {
            let mut c = cx(&mut cfg);
            m.enter(&mut c);
            assert!(!m.title().is_empty());
            assert!(!dump(&m.items(&c)).is_empty());
            m.leave(&mut c);
        }
    }
}
