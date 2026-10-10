//! Tray popup pages: the contract between the page models (pure, unit-tested) and the renderer.
//!
//! The popup (`popup_win`) is one layered window. `Home` is the existing menu; every other
//! [`Route`] is a 380 px page whose content is a list of [`Item`]s produced by a [`PageModel`].
//! The renderer lays the items out (`layout`), draws them (`draw`, `controls`), scrolls them
//! (`scroll`), edits text fields (`edit`) and feeds user input back as [`Event`]s; the model
//! mutates `Cx::cfg` and answers with [`Effect`]s. Models never touch Win32, files they do not
//! own, or the daemon: everything with a side effect is an `Effect`, executed by the popup, which
//! turns it into a [`PopupAction`] in the [`mailbox`] for the daemon thread.
//!
//! Layout:
//! - `mod.rs` (this file): the contract between models and renderer.
//! - `layout`, `draw`, `edit`, `scroll`, `controls`, `session`, `pager`: the renderer.
//! - `settings/**`: the Settings index, its pages and the list pickers.
//! - `history`, `dictionary`: History, Dictionary and Snippets.
//!
//! Data flow of one interaction: input -> renderer finds the item -> `model.event(id, ev, cx)`
//! -> `Vec<Effect>` -> renderer re-asks `model.items(cx)` and re-lays-out.

pub mod controls;
pub mod dictionary;
pub mod draw;
pub mod edit;
#[cfg(any(test, debug_assertions))]
pub mod gallery;
pub mod history;
pub mod layout;
pub mod metrics;
pub mod pager;
pub mod scroll;
pub mod session;
pub mod settings;

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use crate::core::config::Config;
pub use crate::core::nav::{PickerId, Route, Section};

// ---------------------------------------------------------------------------------------------
// Metrics shared by models and the renderer (logical px at 1x)
// ---------------------------------------------------------------------------------------------

/// Card width of every page (Home stays at its own 288).
pub const CARD_W_PAGE: f32 = 380.0;
/// Page card height: `min(PAGE_MAX_H, work_h - 2 * EDGE_GAP)`; fixed per page, never content-sized.
pub const PAGE_MAX_H: f32 = 600.0;
/// Page header (back button + title).
pub const PAGE_HEADER_H: f32 = 48.0;
/// Fixed `Row` heights of the virtualised lists (the model puts the right one in `Row::height`
/// and `VirtualRows::row_h`; the renderer turns scroll offset into a row index by division).
pub const ROW_H_HISTORY: f32 = 64.0;
pub const ROW_H_WORD: f32 = 32.0;
pub const ROW_H_SNIPPET: f32 = 44.0;

// ---------------------------------------------------------------------------------------------
// Items
// ---------------------------------------------------------------------------------------------

/// Identifies an interactive item within ONE page model (stable across `items()` calls so focus,
/// scroll and a half-typed field survive re-layout). Models pick their own numbering.
pub type ItemId = u16;

/// Colour role of a banner, result, chip or dot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    Info,
    Warn,
    Ok,
    Err,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FieldKind {
    /// Single-line text.
    Text,
    /// Single-line, drawn as bullets, copy/cut refused (API keys).
    Secret,
    /// Multi-line text area with `rows` visible lines (prompt, snippet replacement, JSON secrets).
    Multiline { rows: u8 },
    /// Key-combination capture box: no typing; click starts capture (`Effect::BeginHotkeyCapture`),
    /// the value shows the accelerator or the live preview.
    Hotkey,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BtnStyle {
    Primary,
    Outline,
    Destructive,
    /// Text-only (e.g. "Get key", "Reset to default").
    Link,
}

/// Right edge of a [`Row`].
#[derive(Clone, Debug, PartialEq)]
pub enum Trailing {
    None,
    /// Chevron: the row opens another page (tap = `Event::Activated`).
    Chevron,
    /// Muted value text (e.g. the current setting) followed by a chevron.
    Value(String),
    /// Small coloured badge.
    Badge { text: String, tone: Tone },
    /// "x" delete button (`Event::Deleted`), shown on hover and keyboard focus.
    Delete,
    /// Copy button (`Event::Copy`), shown on hover and keyboard focus.
    Copy,
}

/// A fixed-height list row: settings index entries, history entries, dictionary words, snippets.
#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    pub id: ItemId,
    /// First line (e.g. app + clock, the word, the snippet trigger).
    pub primary: String,
    /// Second line, cut to the row width (e.g. the transcript text); empty = single-line row.
    pub secondary: String,
    /// Small muted meta line (provider, latency); empty = none.
    pub meta: String,
    /// Chips next to the primary line (Failed / Discarded / AI cleaned / Rewritten).
    pub chips: Vec<(String, Tone)>,
    pub trailing: Trailing,
    /// Fixed height in logical px: one of `ROW_H_*`, or 44 for settings rows.
    pub height: f32,
}

impl Row {
    pub fn new(id: ItemId, primary: impl Into<String>, height: f32) -> Row {
        Row { id, primary: primary.into(), secondary: String::new(), meta: String::new(), chips: Vec::new(), trailing: Trailing::None, height }
    }
}

/// One element of a page. Pure data: the renderer owns measuring, drawing and focus.
#[derive(Clone, Debug, PartialEq)]
pub enum Item {
    /// Section heading inside a page.
    Group(String),
    /// Muted explanatory text (wrapped).
    Note(String),
    /// Coloured notice (wrapped).
    Banner { tone: Tone, text: String },
    /// Label + description + on/off switch. Emits `Toggled(new)`.
    Toggle { id: ItemId, label: String, desc: String, on: bool },
    /// 2-4 option segmented control. Emits `Picked(index)`.
    Segmented { id: ItemId, label: String, options: Vec<String>, sel: usize },
    /// Label + current value + chevron; Activated opens a picker: reply `Effect::Navigate(Route::Picker(..))`.
    Choice { id: ItemId, label: String, value: String },
    /// Radio card (model cards, activation mode). Activated/`Toggled(true)` selects.
    Radio { id: ItemId, label: String, desc: String, on: bool },
    /// Labelled text field. `value` is the committed value; while the field is focused the
    /// renderer keeps its own edit buffer and does NOT overwrite it from `items()`. Emits
    /// `Edited(text)` on every change and `Committed(text)` on Enter, blur, page leave and popup
    /// close. `max_chars == 0` = unlimited. `error` is drawn under the field in the error colour.
    Field { id: ItemId, label: String, hint: String, value: String, kind: FieldKind, error: Option<String>, max_chars: usize },
    /// Button. Emits `Pressed` when enabled.
    Button { id: ItemId, label: String, style: BtnStyle, enabled: bool },
    /// Outcome line of a test (badge such as "OK 840 ms" + the text).
    Result { ok: bool, badge: String, text: String },
    /// Fixed-height list row (see [`Row`]); `Activated` on tap/Enter, `Deleted`/`Copy` from its trailing buttons.
    Row(Row),
    Divider,
    /// Vertical gap in logical px.
    Space(f32),
}

impl Item {
    /// The id of an interactive item (None for Group, Note, Banner, Result, Divider, Space).
    pub fn id(&self) -> Option<ItemId> {
        match self {
            Item::Toggle { id, .. }
            | Item::Segmented { id, .. }
            | Item::Choice { id, .. }
            | Item::Radio { id, .. }
            | Item::Field { id, .. }
            | Item::Button { id, .. } => Some(*id),
            Item::Row(r) => Some(r.id),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Events (renderer -> model) and effects (model -> popup)
// ---------------------------------------------------------------------------------------------

/// What the user did to an item.
#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    /// Toggle / Radio changed to this value.
    Toggled(bool),
    /// Segmented option index chosen.
    Picked(usize),
    /// Field text changed (every keystroke; ignore it unless the page filters live).
    Edited(String),
    /// Field text committed (Enter, blur, page leave, popup close). Always delivered for a dirty
    /// field before the page is left or the popup closes, so no edit is lost.
    Committed(String),
    /// Button pressed.
    Pressed,
    /// Row / Choice tapped or activated with Enter.
    Activated,
    /// A row's "x" pressed.
    Deleted,
    /// A row's copy button pressed.
    Copy,
    /// Hotkey capture in progress: the accelerator typed so far ("Control+Super"). Delivered to
    /// the `Field { kind: Hotkey }` that sent `Effect::BeginHotkeyCapture(id)`.
    CaptureUpdate(String),
    /// Hotkey capture finished with this accelerator (unvalidated; the model validates and
    /// re-arms with `BeginHotkeyCapture` or applies it). `Esc` alone ends capture without this event.
    CaptureDone(String),
}

/// Which provider test to run on a worker thread.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TestKind {
    /// Embedded sample WAV through the active STT provider.
    Stt,
    /// Sample text through the active cleanup LLM.
    Llm,
}

/// Outcome of a test, delivered to the page that asked via [`PageModel::test_done`].
#[derive(Clone, Debug, PartialEq)]
pub struct TestResult {
    pub kind: TestKind,
    pub ok: bool,
    /// Short badge ("Transcribed in 840 ms", "Error").
    pub badge: String,
    pub text: String,
}

/// Which destructive action a confirmation guards; handed back to the page in
/// [`PageModel::confirmed`] so it knows what to do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfirmId {
    HistoryClear,
    /// Delete this history entry (`Entry::id`).
    HistoryDelete(i64),
    DictionaryClear,
    SnippetsClear,
    /// Delete the snippet at this index in `Config.snippets`.
    SnippetDelete(usize),
    /// Reset app data (About page).
    ResetApp,
    /// Remove a stored API key (page-private numbering).
    RemoveKey(u16),
    /// Page-private confirmation.
    Custom(u16),
}

/// What a model asks the popup to do. Applied in order after each event.
#[derive(Clone, Debug, PartialEq)]
pub enum Effect {
    /// Push a route on the popup's back stack (the page is built with [`model_for`]).
    Navigate(Route),
    /// Pop the back stack (Home when empty).
    Back,
    /// Show text in the footer status strip for ~2.6 s ("Copied to clipboard", "Saved").
    Toast(String),
    /// Put text on the clipboard.
    CopyText(String),
    /// Open a folder or file in the shell (data folder, logs folder).
    OpenPath(PathBuf),
    /// Start in-daemon hotkey capture; `CaptureUpdate`/`CaptureDone` go to this item.
    BeginHotkeyCapture(ItemId),
    /// Stop capture (also done automatically on leave, close and deactivation).
    EndHotkeyCapture,
    /// Run a provider test on a worker; the result comes back through `PageModel::test_done`.
    RunTest(TestKind),
    /// Ask first. On accept the popup calls `PageModel::confirmed(on_confirm)`.
    Confirm { title: String, body: String, action: String, destructive: bool, on_confirm: ConfirmId },
    /// `cx.cfg` was changed: the popup posts `PopupAction::SetConfig(Box::new(cx.cfg.clone()))`
    /// (the daemon saves it and applies the diff, like a reload). Toggles, segments and pickers
    /// answer `Saved` on every change; text fields on `Committed`.
    Saved,
    /// Any other daemon-side action, posted through the mailbox as is.
    Do(PopupAction),
    /// Move keyboard focus to this item of the current page (after earlier effects ran).
    Focus(ItemId),
}

/// Actions for the daemon thread, queued by the popup in the [`mailbox`] (the popup then posts
/// `WM_TRAY_POPUP_ACT`; the host drains the mailbox, applies each action, saves, and refreshes
/// the popup's state). The popup never touches `App` directly.
#[derive(Clone, Debug, PartialEq)]
pub enum PopupAction {
    /// Replace the config: save it (atomic) and apply the same diffs as a reload.
    SetConfig(Box<Config>),
    /// Store (`Some`) or remove (`None`) a secret in the DPAPI store; applied immediately.
    SetSecret { name: String, value: Option<String> },
    /// Delete one history entry by id.
    HistoryDelete(i64),
    /// Delete all history.
    HistoryClear,
    /// Prune history older than N days (0 = keep forever means no-op); runs on a worker.
    HistoryPrune(u32),
    /// Launch at login on/off (registry Run key; not part of `Config`).
    Autostart(bool),
    /// Reset app data: config, history, secrets, autostart.
    Reset,
    /// Run a provider test on a worker thread and report back through the popup.
    RunTest(TestKind),
    /// Retry the last failed dictation.
    Retry,
}

/// The queue between the popup and the daemon thread (same pattern as `WM_NOTE`: payload in a
/// mutexed queue, a small message wakes the receiver).
pub mod mailbox {
    use super::*;

    static QUEUE: Mutex<VecDeque<PopupAction>> = Mutex::new(VecDeque::new());

    /// Queue an action. The caller then posts the wake-up message.
    pub fn push(a: PopupAction) {
        QUEUE.lock().unwrap_or_else(|e| e.into_inner()).push_back(a);
    }

    /// Take every queued action in order.
    pub fn drain() -> Vec<PopupAction> {
        QUEUE.lock().unwrap_or_else(|e| e.into_inner()).drain(..).collect()
    }

    /// Drop everything queued (tests, shutdown).
    pub fn clear() {
        QUEUE.lock().unwrap_or_else(|e| e.into_inner()).clear();
    }
}

// ---------------------------------------------------------------------------------------------
// Model
// ---------------------------------------------------------------------------------------------

/// Wakes the popup from a worker thread: posts a message that makes the popup call
/// [`PageModel::wake`] on the current page. Cheap to clone, safe to call after the popup closed
/// (then it does nothing), and the page may receive spurious wakes.
pub type Waker = Arc<dyn Fn() + Send + Sync>;

/// A waker that does nothing (tests).
pub fn noop_waker() -> Waker {
    Arc::new(|| {})
}

/// Context handed to every model call.
pub struct Cx<'a> {
    /// The popup's working copy of the config. Mutate it and answer `Effect::Saved`; the daemon's
    /// copy is replaced on apply, and a fresh one arrives with each state sync.
    pub cfg: &'a mut Config,
    /// Dark theme in effect (resolved from `cfg.theme` at open).
    pub dark: bool,
    /// Wall clock, unix ms, sampled when the popup handles the input (pure models use it for
    /// "Today / Yesterday" and relative times).
    pub now_ms: i64,
    /// For workers started by the model (search scans, device lists).
    pub waker: Waker,
}

impl<'a> Cx<'a> {
    pub fn new(cfg: &'a mut Config, dark: bool, now_ms: i64) -> Cx<'a> {
        Cx { cfg, dark, now_ms, waker: noop_waker() }
    }
}

/// A virtualised list shown after the page's `items()`: `count` rows of fixed `row_h`; the
/// renderer asks `row(i)` only for visible indexes (O(1) per row: `i = offset / row_h`). Row
/// events arrive through `PageModel::row_event(i, ..)`.
pub struct VirtualRows<'a> {
    pub count: usize,
    pub row_h: f32,
    pub row: Box<dyn Fn(usize) -> Item + 'a>,
}

/// A page's behaviour. Everything is synchronous and pure with respect to the outside world
/// (clock and config come through [`Cx`]).
pub trait PageModel {
    /// Header title.
    fn title(&self) -> String;
    /// The page content, top to bottom. Called after every event and wake; must be cheap.
    fn items(&self, cx: &Cx) -> Vec<Item>;
    /// A user action on item `id`. Mutate `cx.cfg` and return effects (usually `Effect::Saved`).
    fn event(&mut self, id: ItemId, ev: Event, cx: &mut Cx) -> Vec<Effect>;
    /// The page was pushed or re-shown (load data, e.g. the history tail).
    fn enter(&mut self, _cx: &mut Cx) {}
    /// The page is being left (popped or popup closing), after dirty fields were committed.
    fn leave(&mut self, _cx: &mut Cx) {}
    /// Optional virtualised list laid out after `items()`.
    fn virtual_rows(&self) -> Option<VirtualRows<'_>> {
        None
    }
    /// Event on row `index` of the virtual list.
    fn row_event(&mut self, _index: usize, _ev: Event, _cx: &mut Cx) -> Vec<Effect> {
        Vec::new()
    }
    /// Items laid out after the virtual rows (e.g. "Load older").
    fn tail_items(&self, _cx: &Cx) -> Vec<Item> {
        Vec::new()
    }
    /// The user accepted a confirmation this page requested.
    fn confirmed(&mut self, _id: ConfirmId, _cx: &mut Cx) -> Vec<Effect> {
        Vec::new()
    }
    /// A requested test finished.
    fn test_done(&mut self, _result: TestResult, _cx: &mut Cx) {}
    /// A worker called the [`Waker`]: read its shared slot and update state.
    fn wake(&mut self, _cx: &mut Cx) {}
}

/// Build the page for a route; `None` for `Home` (the menu is not a page model). The only place
/// that maps routes to constructors; the constructors' signatures are fixed by the contract.
pub fn model_for(route: &Route) -> Option<Box<dyn PageModel>> {
    Some(match *route {
        Route::Home => return None,
        Route::History => history::model(),
        Route::HistoryEntry(id) => history::entry_model(id),
        Route::Dictionary => dictionary::model(),
        Route::Snippets => dictionary::snippets_model(),
        Route::SnippetEdit(i) => dictionary::snippet_edit_model(i),
        Route::SettingsIndex => settings::index_model(),
        Route::Settings(s) => settings::model(s),
        Route::Picker(p) => settings::picker_model(p),
    })
}

/// Placeholder page used by the stub constructors until a page is implemented.
pub struct Placeholder {
    title: String,
}

impl Placeholder {
    pub fn boxed(title: impl Into<String>) -> Box<dyn PageModel> {
        Box::new(Placeholder { title: title.into() })
    }
}

impl PageModel for Placeholder {
    fn title(&self) -> String {
        self.title.clone()
    }
    fn items(&self, _cx: &Cx) -> Vec<Item> {
        vec![Item::Note("This page is not available yet.".into())]
    }
    fn event(&mut self, _id: ItemId, _ev: Event, _cx: &mut Cx) -> Vec<Effect> {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all_routes() -> Vec<Route> {
        let mut v = vec![
            Route::History,
            Route::HistoryEntry(1),
            Route::Dictionary,
            Route::Snippets,
            Route::SnippetEdit(None),
            Route::SnippetEdit(Some(0)),
            Route::SettingsIndex,
        ];
        v.extend(Section::ALL.iter().map(|s| Route::Settings(*s)));
        v.extend(PickerId::ALL.iter().map(|p| Route::Picker(*p)));
        v
    }

    #[test]
    fn every_route_has_a_model_except_home() {
        assert!(model_for(&Route::Home).is_none());
        let mut cfg = Config::default();
        for r in all_routes() {
            let mut m = model_for(&r).unwrap_or_else(|| panic!("{r:?}"));
            let mut cx = Cx::new(&mut cfg, false, 0);
            m.enter(&mut cx);
            assert!(!m.title().is_empty(), "{r:?}");
            assert!(!m.items(&cx).is_empty(), "{r:?}");
            m.leave(&mut cx);
        }
    }

    #[test]
    fn item_ids_are_reported_for_interactive_items_only() {
        assert_eq!(Item::Toggle { id: 3, label: "a".into(), desc: String::new(), on: true }.id(), Some(3));
        assert_eq!(Item::Row(Row::new(9, "w", ROW_H_WORD)).id(), Some(9));
        assert_eq!(Item::Note("n".into()).id(), None);
        assert_eq!(Item::Divider.id(), None);
        assert_eq!(Item::Result { ok: true, badge: String::new(), text: String::new() }.id(), None);
    }

    #[test]
    fn mailbox_is_fifo_and_drains() {
        mailbox::clear();
        mailbox::push(PopupAction::HistoryDelete(5));
        mailbox::push(PopupAction::Autostart(true));
        mailbox::push(PopupAction::SetConfig(Box::default()));
        let got = mailbox::drain();
        assert_eq!(got.len(), 3);
        assert_eq!(got[0], PopupAction::HistoryDelete(5));
        assert_eq!(got[1], PopupAction::Autostart(true));
        assert!(matches!(got[2], PopupAction::SetConfig(_)));
        assert!(mailbox::drain().is_empty());
    }

    #[test]
    fn default_model_hooks_are_inert() {
        let mut cfg = Config::default();
        let mut cx = Cx::new(&mut cfg, true, 7);
        let mut m = Placeholder::boxed("X");
        assert!(m.virtual_rows().is_none());
        assert!(m.tail_items(&cx).is_empty());
        assert!(m.confirmed(ConfirmId::HistoryClear, &mut cx).is_empty());
        assert!(m.row_event(0, Event::Activated, &mut cx).is_empty());
        assert!(m.event(0, Event::Pressed, &mut cx).is_empty());
        m.wake(&mut cx);
        (cx.waker)();
    }
}
