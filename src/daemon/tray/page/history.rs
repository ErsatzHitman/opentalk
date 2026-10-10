//! History pages.
//!
//! * [`model`] (`Route::History`): stats line, search field, "Show discarded", a virtualised list
//!   of fixed-height rows (`ROW_H_HISTORY`, newest first, the day and time in each row's first
//!   line) and, after the list, "Load older" and "Clear all".
//! * [`entry_model`] (`Route::HistoryEntry(id)`): the full text, the rewrite's "before" text, the
//!   raw transcript, Copy / Retry / Delete.
//!
//! Data comes from a [`Store`] (the real one reads `core::history`; tests use a fake one):
//! the newest [`PAGE`] entries are read from the tail of the file when the page is shown, "Load
//! older" reads further back, and a committed search over a partly loaded history, as well as the
//! whole-file totals, run on a worker thread and come back through the popup's [`Waker`] and
//! [`PageModel::wake`]. Deleting and clearing go through the daemon (`PopupAction`); until it has
//! done so, a page re-read from disk must not show the entry again, which the shared
//! [`Tombstones`] take care of.
//!
//! Contract notes for the renderer: the search field is "live" (the list filters on every
//! `Edited`), and the list is rebuilt from `virtual_rows()` after each event.
use std::collections::HashSet;
use std::sync::{Arc, Mutex, OnceLock};

use super::{BtnStyle, ConfirmId, action_line, picked_action, Cx, Effect, Event, FieldKind, Icon, Item, ItemId, Lead, PageModel, PopupAction, ROW_H_HISTORY, Route, Row, Tone, Tool, ToolTone, Trailing, VirtualRows};
use crate::core::dictview::{one_line, truncate};
use crate::core::histview::{self, Stats};
use crate::core::history::{self, Entry, Status};
use crate::core::timefmt;

/// Entries read when the page opens and per "Load older" step.
const PAGE: usize = 200;
/// Most entries held in memory (older ones stay reachable through search).
const MAX_LOADED: usize = 2000;
/// Longest text kept in a list row (the renderer cuts it to the row width anyway).
const ROW_TEXT_CHARS: usize = 240;

const F_SEARCH: ItemId = 1;
const T_DISCARDED: ItemId = 2;
const B_MORE: ItemId = 3;
const B_CLEAR: ItemId = 4;
/// Virtual row `i` has item id `ROW_BASE + i`.
const ROW_BASE: ItemId = 100;

// ---------------------------------------------------------------------------------------------
// Data access
// ---------------------------------------------------------------------------------------------

/// Where the pages read history from.
trait Store: Send + Sync {
    /// The newest `n` entries (any status) and whether older ones exist.
    fn load_recent(&self, n: usize) -> history::Recent;
    fn find(&self, id: i64) -> Option<Entry>;
    /// The newest `cap` matches of a whole-file search (worker thread).
    fn search(&self, query: &str, cap: usize) -> history::Recent;
    /// Whole-file totals (worker thread).
    fn summary(&self) -> Stats;
}

struct DiskStore;

impl Store for DiskStore {
    fn load_recent(&self, n: usize) -> history::Recent {
        history::load_recent(n)
    }
    fn find(&self, id: i64) -> Option<Entry> {
        history::find(id)
    }
    fn search(&self, query: &str, cap: usize) -> history::Recent {
        history::search_view(query, cap)
    }
    fn summary(&self) -> Stats {
        history::summary()
    }
}

/// Runs a job off the UI thread.
type Spawn = fn(Box<dyn FnOnce() + Send>);

fn spawn_thread(job: Box<dyn FnOnce() + Send>) {
    let _ = std::thread::Builder::new().name("history-scan".into()).spawn(job);
}

/// Deletions the daemon has been asked for but may not have applied yet. Shared by every history
/// page of the process: the detail page records a delete, then the list is re-shown and re-reads
/// the file before the daemon (which handles the popup's mailbox after the click) has rewritten it.
#[derive(Default)]
struct Tombstones {
    ids: HashSet<i64>,
    /// "Clear all" was confirmed at this time: entries stamped up to then are gone.
    cleared_at: Option<i64>,
}

impl Tombstones {
    fn hides(&self, e: &Entry) -> bool {
        self.ids.contains(&e.id) || self.cleared_at.is_some_and(|t| e.ts <= t)
    }
}

fn global_tombstones() -> Arc<Mutex<Tombstones>> {
    static T: OnceLock<Arc<Mutex<Tombstones>>> = OnceLock::new();
    T.get_or_init(Default::default).clone()
}

/// The entry the list is opening in the detail page. The list already holds it (it may have found
/// it through a whole-file search, far from the end of the file), so the detail page takes it from
/// here instead of reading the file again on the UI thread.
type Handoff = Arc<Mutex<Option<Entry>>>;

fn global_handoff() -> Handoff {
    static H: OnceLock<Handoff> = OnceLock::new();
    H.get_or_init(Default::default).clone()
}

#[derive(Clone)]
struct Env {
    store: Arc<dyn Store>,
    spawn: Spawn,
    tomb: Arc<Mutex<Tombstones>>,
    handoff: Handoff,
    /// Seconds to add to UTC for local time at a timestamp.
    offset_of: fn(i64) -> i64,
}

impl Env {
    fn disk() -> Env {
        Env { store: Arc::new(DiskStore), spawn: spawn_thread, tomb: global_tombstones(), handoff: global_handoff(), offset_of: timefmt::local_offset_secs }
    }

    fn alive(&self, mut v: Vec<Entry>) -> Vec<Entry> {
        let t = self.tomb.lock().unwrap_or_else(|e| e.into_inner());
        v.retain(|e| !t.hides(e));
        v
    }

    fn hidden(&self, e: &Entry) -> bool {
        self.tomb.lock().unwrap_or_else(|e| e.into_inner()).hides(e)
    }
}

/// History list page (`Route::History`).
pub fn model() -> Box<dyn PageModel> {
    Box::new(HistoryPage::new(Env::disk()))
}

/// Detail page of one entry (`Route::HistoryEntry(id)`, `id` = `core::history::Entry::id`).
pub fn entry_model(id: i64) -> Box<dyn PageModel> {
    Box::new(EntryPage::new(Env::disk(), id))
}

fn app_of(e: &Entry) -> Option<&str> {
    e.app_name.as_deref().map(str::trim).filter(|a| !a.is_empty())
}

fn clipboard_toast() -> Effect {
    Effect::Toast("Copied to clipboard".into())
}

// ---------------------------------------------------------------------------------------------
// The list
// ---------------------------------------------------------------------------------------------

/// Results of worker threads, picked up by `wake`.
#[derive(Default)]
struct Slot {
    /// `(token, query, matches)` of a finished search.
    search: Option<(u64, String, history::Recent)>,
    summary: Option<Stats>,
}

struct Hits {
    /// The trimmed query these entries answer.
    query: String,
    entries: Vec<Entry>,
    /// Older matches exist than `entries` holds.
    more: bool,
}

struct HistoryPage {
    env: Env,
    /// Newest first; at most `MAX_LOADED`.
    pool: Vec<Entry>,
    /// Older entries exist on disk than the pool holds (also once the pool is full).
    more: bool,
    /// Live text of the search field.
    query: String,
    show_discarded: bool,
    /// Whole-file answer to the committed query (only used while it matches the field).
    hits: Option<Hits>,
    /// A worker is searching: `(token, trimmed query)`.
    pending: Option<(u64, String)>,
    token: u64,
    summary: Option<Stats>,
    slot: Arc<Mutex<Slot>>,
    /// Indices into the current source (pool or hits) that pass the filters; cached.
    view: Vec<usize>,
    now_ms: i64,
}

impl HistoryPage {
    fn new(env: Env) -> HistoryPage {
        HistoryPage {
            env,
            pool: Vec::new(),
            more: false,
            query: String::new(),
            show_discarded: false,
            hits: None,
            pending: None,
            token: 0,
            summary: None,
            slot: Arc::new(Mutex::new(Slot::default())),
            view: Vec::new(),
            now_ms: 0,
        }
    }

    fn q(&self) -> &str {
        self.query.trim()
    }

    /// Entries the list is drawn from: the whole-file answer when it matches the field, else the pool.
    fn source(&self) -> &[Entry] {
        match &self.hits {
            Some(h) if !self.q().is_empty() && h.query == self.q() => &h.entries,
            _ => &self.pool,
        }
    }

    fn refresh_view(&mut self) {
        let v = histview::visible(self.source(), &self.query, self.show_discarded);
        self.view = v;
    }

    fn reload(&mut self, keep: usize) {
        let r = self.env.store.load_recent(keep.clamp(PAGE, MAX_LOADED));
        self.pool = self.env.alive(r.entries);
        self.more = r.more;
    }

    /// "Load older" has something to read and room to hold it.
    fn can_load_more(&self) -> bool {
        self.more && self.pool.len() < MAX_LOADED
    }

    /// Take what the workers delivered.
    fn absorb(&mut self) {
        let (search, summary) = {
            let mut s = self.slot.lock().unwrap_or_else(|e| e.into_inner());
            (s.search.take(), s.summary.take())
        };
        // Whole-file totals only matter while the pool holds part of the file; a finished worker
        // must not overwrite the exact numbers of a complete pool (e.g. after "Clear all").
        if let Some(s) = summary.filter(|_| self.more) {
            self.summary = Some(s);
        }
        if let Some((token, query, found)) = search
            && self.pending.as_ref().is_some_and(|(t, q)| *t == token && *q == query)
        {
            self.pending = None;
            if self.q() == query {
                self.hits = Some(Hits { query, entries: self.env.alive(found.entries), more: found.more });
            }
        }
        self.refresh_view();
    }

    fn start_search(&mut self, cx: &Cx) {
        let q = self.q().to_string();
        if q.is_empty() || !self.more || self.hits.as_ref().is_some_and(|h| h.query == q) || self.pending.as_ref().is_some_and(|(_, p)| *p == q) {
            return;
        }
        self.token += 1;
        let token = self.token;
        self.pending = Some((token, q.clone()));
        let (store, slot, waker) = (self.env.store.clone(), self.slot.clone(), cx.waker.clone());
        (self.env.spawn)(Box::new(move || {
            let found = store.search(&q, MAX_LOADED);
            slot.lock().unwrap_or_else(|e| e.into_inner()).search = Some((token, q, found));
            waker();
        }));
    }

    fn start_summary(&self, cx: &Cx) {
        if !self.more {
            return;
        }
        let (store, slot, waker) = (self.env.store.clone(), self.slot.clone(), cx.waker.clone());
        (self.env.spawn)(Box::new(move || {
            let s = store.summary();
            slot.lock().unwrap_or_else(|e| e.into_inner()).summary = Some(s);
            waker();
        }));
    }

    fn stats_note(&self) -> Option<String> {
        if self.pool.is_empty() && !self.more {
            return None;
        }
        Some(match (&self.summary, self.more) {
            (Some(s), true) => histview::stats_line(s),
            (_, false) => histview::stats_line(&histview::stats(&self.pool)),
            (None, true) => format!("Newest {} entries \u{b7} {}", histview::thousands(self.pool.len()), histview::stats_line(&histview::stats(&self.pool))),
        })
    }

    fn empty_note(&self, history_on: bool) -> Option<String> {
        if !self.view.is_empty() {
            return None;
        }
        if !self.q().is_empty() {
            return Some(if self.pending.is_some() { "No matches yet.".into() } else { format!("No dictations match \u{201c}{}\u{201d}.", truncate(self.q(), 40)) });
        }
        if !self.pool.is_empty() {
            return Some("Only discarded dictations so far. Turn on \u{201c}Show discarded\u{201d} to see them.".into());
        }
        Some(if history_on { "No dictations yet. They appear here after you dictate.".into() } else { "No saved dictations.".into() })
    }

    fn row(&self, i: usize) -> Row {
        let e = &self.source()[self.view[i]];
        // the app is its icon; the first line is just the time
        let primary = timefmt::when_label(e.ts, self.now_ms, &self.env.offset_of);
        let id = ROW_BASE + i.min(usize::from(u16::MAX - ROW_BASE)) as u16;
        let mut r = Row::new(id, primary, ROW_H_HISTORY);
        r.lead = Lead::App { path: e.app_path.clone(), exe: e.app_exe.clone(), name: app_of(e).unwrap_or_default().to_string() };
        r.secondary = match e.status {
            Status::Failed => e.error.as_deref().map(one_line).filter(|s| !s.is_empty()).unwrap_or_else(|| "Transcription failed.".into()),
            _ => {
                let text = if e.text.trim().is_empty() { one_line(&e.raw) } else { one_line(&e.text) };
                if text.is_empty() { "(empty)".into() } else { truncate(&text, ROW_TEXT_CHARS) }
            }
        };
        // Only the latency: provider and model are on the detail page, and a longer meta crowds the
        // time and app name out of the first line (the renderer shares it with the chips).
        if e.status == Status::Ok && e.latency_ms > 0 {
            r.meta = histview::format_latency(e.latency_ms);
        }
        match e.status {
            Status::Failed => r.chips.push(("Failed".into(), Tone::Err)),
            Status::Discarded => r.chips.push(("Discarded".into(), Tone::Warn)),
            Status::Ok => {}
        }
        if e.rewritten {
            r.chips.push(("Rewritten".into(), Tone::Info));
        } else if e.ai_cleaned {
            r.chips.push(("AI".into(), Tone::Info));
        } else if e.cleanup_failed {
            r.chips.push(("STT fallback".into(), Tone::Warn));
        }
        r.trailing = if e.status == Status::Ok && !e.text.trim().is_empty() { Trailing::Copy } else { Trailing::Chevron };
        r
    }

    fn entry_at(&self, index: usize) -> Option<&Entry> {
        self.view.get(index).and_then(|&i| self.source().get(i))
    }
}

impl PageModel for HistoryPage {
    fn title(&self) -> String {
        "History".into()
    }

    fn items(&self, cx: &Cx) -> Vec<Item> {
        let mut v = Vec::new();
        if !cx.cfg.history_enabled {
            v.push(Item::Banner { tone: Tone::Warn, text: "History is off, so new dictations are not saved. You can turn it on under Settings > Privacy & Data.".into() });
        }
        if let Some(s) = self.stats_note() {
            v.push(Item::Note(s));
        }
        v.push(Item::Field {
            id: F_SEARCH,
            label: String::new(),
            hint: "Search".into(),
            value: self.query.clone(),
            kind: FieldKind::Search,
            error: None,
            max_chars: 200,
        });
        v.push(Item::Toggle { id: T_DISCARDED, label: "Show discarded".into(), desc: String::new(), on: self.show_discarded });
        if let Some(n) = self.empty_note(cx.cfg.history_enabled) {
            v.push(Item::Note(n));
        }
        v
    }

    fn virtual_rows(&self) -> Option<VirtualRows<'_>> {
        (!self.view.is_empty()).then(|| VirtualRows { count: self.view.len(), row_h: ROW_H_HISTORY, row: Box::new(move |i| Item::Row(self.row(i))) })
    }

    fn tail_items(&self, _cx: &Cx) -> Vec<Item> {
        let mut v = Vec::new();
        if self.pending.is_some() {
            v.push(Item::Note("Searching all history\u{2026}".into()));
        }
        if let Some(h) = self.hits.as_ref().filter(|h| h.more && h.query == self.q()) {
            v.push(Item::Note(format!("Showing the newest {} matches. Narrow the search to see older ones.", histview::thousands(h.entries.len()))));
        }
        if self.can_load_more() && self.q().is_empty() {
            v.push(Item::Button { id: B_MORE, label: "Load older".into(), style: BtnStyle::Outline, enabled: true });
        } else if self.more && self.q().is_empty() {
            v.push(Item::Note(format!("Showing the newest {}. Search to find older dictations.", histview::thousands(MAX_LOADED))));
        }
        v.push(Item::Space(4.0));
        let any = !self.pool.is_empty() || self.more;
        v.push(Item::Tools { id: B_CLEAR, label: "Clear history".into(), sub: String::new(), tools: vec![Tool::new(Icon::Trash, "Clear all", ToolTone::Danger, any)] });
        v
    }

    fn enter(&mut self, cx: &mut Cx) {
        self.now_ms = cx.now_ms;
        let keep = self.pool.len();
        self.reload(keep);
        if let Some(h) = &mut self.hits {
            let entries = std::mem::take(&mut h.entries);
            h.entries = self.env.alive(entries);
        }
        self.start_summary(cx);
        self.absorb();
        self.start_search(cx);
    }

    fn event(&mut self, id: ItemId, ev: Event, cx: &mut Cx) -> Vec<Effect> {
        self.now_ms = cx.now_ms;
        match (id, ev) {
            (F_SEARCH, Event::Edited(t)) => {
                self.query = t;
                self.refresh_view();
            }
            (F_SEARCH, Event::Committed(t)) => {
                self.query = t;
                if self.q().is_empty() {
                    self.hits = None;
                    self.pending = None;
                }
                self.refresh_view();
                self.start_search(cx);
            }
            (T_DISCARDED, Event::Toggled(on)) => {
                self.show_discarded = on;
                self.refresh_view();
            }
            (B_MORE, Event::Pressed) if self.can_load_more() => {
                let keep = self.pool.len() + PAGE;
                self.reload(keep);
                self.refresh_view();
            }
            (B_CLEAR, Event::Pressed | Event::Picked(0)) => {
                return vec![Effect::Confirm {
                    title: "Clear history?".into(),
                    body: "This permanently deletes all saved dictations. It cannot be undone.".into(),
                    action: "Clear all".into(),
                    destructive: true,
                    on_confirm: ConfirmId::HistoryClear,
                }];
            }
            _ => {}
        }
        Vec::new()
    }

    fn row_event(&mut self, index: usize, ev: Event, _cx: &mut Cx) -> Vec<Effect> {
        let Some(e) = self.entry_at(index) else { return Vec::new() };
        match ev {
            Event::Activated => {
                *self.env.handoff.lock().unwrap_or_else(|p| p.into_inner()) = Some(e.clone());
                vec![Effect::Navigate(Route::HistoryEntry(e.id))]
            }
            Event::Copy => vec![Effect::CopyText(e.text.clone()), clipboard_toast()],
            _ => Vec::new(),
        }
    }

    fn confirmed(&mut self, id: ConfirmId, cx: &mut Cx) -> Vec<Effect> {
        if id != ConfirmId::HistoryClear {
            return Vec::new();
        }
        self.env.tomb.lock().unwrap_or_else(|e| e.into_inner()).cleared_at = Some(cx.now_ms);
        self.pool.clear();
        self.more = false;
        self.hits = None;
        self.pending = None;
        self.summary = None;
        self.refresh_view();
        vec![Effect::Do(PopupAction::HistoryClear), Effect::Toast("History cleared".into())]
    }

    fn wake(&mut self, cx: &mut Cx) {
        self.now_ms = cx.now_ms;
        self.absorb();
    }
}

// ---------------------------------------------------------------------------------------------
// The detail page
// ---------------------------------------------------------------------------------------------

const B_COPY: ItemId = 1;
const B_COPY_RAW: ItemId = 2;
const B_COPY_ORIGINAL: ItemId = 3;
const B_RETRY: ItemId = 4;
const B_DELETE: ItemId = 5;

struct EntryPage {
    env: Env,
    id: i64,
    entry: Option<Entry>,
    /// This is the newest entry of the file (the only one the daemon can retry).
    newest: bool,
}

impl EntryPage {
    fn new(env: Env, id: i64) -> EntryPage {
        EntryPage { env, id, entry: None, newest: false }
    }

    /// The buttons at the bottom: retry (when possible) and delete.
    fn actions(&self, cx: &Cx, e: &Entry) -> Vec<(ItemId, Tool)> {
        let mut a = Vec::new();
        if self.can_retry(cx) {
            let tip = if e.status == Status::Discarded { "Recover recording" } else { "Retry transcription" };
            a.push((B_RETRY, Tool::new(Icon::Retry, tip, ToolTone::Plain, true)));
        }
        a.push((B_DELETE, Tool::new(Icon::Trash, "Delete", ToolTone::Danger, true)));
        a
    }

    fn can_retry(&self, cx: &Cx) -> bool {
        self.newest && cx.cfg.keep_failed_audio && self.entry.as_ref().is_some_and(|e| e.status != Status::Ok)
    }

    /// The raw transcript when it adds something beyond the text and the pre-rewrite text.
    fn raw_extra(e: &Entry) -> Option<&str> {
        let raw = e.raw.trim();
        let same = |other: &str| other.trim() == raw;
        (!raw.is_empty() && !same(&e.text) && !e.original.as_deref().is_some_and(same)).then_some(e.raw.as_str())
    }
}

impl PageModel for EntryPage {
    fn title(&self) -> String {
        "Dictation".into()
    }

    fn items(&self, cx: &Cx) -> Vec<Item> {
        let Some(e) = &self.entry else {
            return vec![Item::Note("This dictation is no longer in your history.".into())];
        };
        let mut v = Vec::new();
        match e.status {
            Status::Failed => v.push(Item::Banner { tone: Tone::Err, text: e.error.as_deref().map(str::trim).filter(|s| !s.is_empty()).unwrap_or("Transcription failed.").to_string() }),
            Status::Discarded => v.push(Item::Banner { tone: Tone::Warn, text: "Discarded: too short or silent to paste.".into() }),
            Status::Ok => {}
        }
        let when = timefmt::full(e.ts, (self.env.offset_of)(e.ts));
        v.push(Item::Note(match app_of(e) {
            Some(app) => format!("{when} \u{b7} {app}"),
            None => when,
        }));
        let mut meta = histview::meta_line(e);
        if e.duration_ms > 0 {
            if !meta.is_empty() {
                meta.push_str(" \u{b7} ");
            }
            meta.push_str(&format!("{} of speech", histview::format_latency(e.duration_ms)));
        }
        if !meta.is_empty() {
            v.push(Item::Note(meta));
        }
        if e.rewritten {
            v.push(Item::Banner { tone: Tone::Info, text: "Rewritten by AI: you ended with the rewrite phrase.".into() });
        } else if e.ai_cleaned {
            v.push(Item::Banner { tone: Tone::Info, text: "Cleaned up by AI.".into() });
        } else if e.cleanup_failed {
            v.push(Item::Banner { tone: Tone::Warn, text: "Cleanup failed \u{b7} STT fallback".into() });
        }
        // each text has its own copy button on its heading line
        let copy = |id: ItemId, label: &str, tip: &str| Item::Tools { id, label: label.into(), sub: String::new(), tools: vec![Tool::new(Icon::Copy, tip, ToolTone::Plain, true)] };
        if !e.text.trim().is_empty() {
            v.push(copy(B_COPY, "Text", "Copy text"));
            v.push(Item::Text(e.text.trim().to_string()));
        }
        if e.rewritten {
            let original = e.original.as_deref().map(str::trim).filter(|s| !s.is_empty());
            v.push(if original.is_some() { copy(B_COPY_ORIGINAL, "Before the rewrite", "Copy text before the rewrite") } else { Item::Group("Before the rewrite".into()) });
            v.push(Item::Text(original.unwrap_or("(not saved)").to_string()));
        }
        if let Some(raw) = Self::raw_extra(e) {
            v.push(copy(B_COPY_RAW, "Raw transcript", "Copy raw transcript"));
            v.push(Item::Text(raw.trim().to_string()));
        } else if e.raw.trim().is_empty() && e.text.trim().is_empty() && e.status != Status::Discarded {
            v.push(Item::Note("No transcript was recorded.".into()));
        }
        v.push(Item::Divider);
        v.push(action_line("", "", &self.actions(cx, e)));
        v
    }

    fn enter(&mut self, _cx: &mut Cx) {
        let handed = self.env.handoff.lock().unwrap_or_else(|p| p.into_inner()).take().filter(|e| e.id == self.id);
        self.entry = handed.or_else(|| self.env.store.find(self.id)).filter(|e| !self.env.hidden(e));
        self.newest = self.env.store.load_recent(1).entries.first().is_some_and(|e| e.id == self.id);
    }

    fn event(&mut self, id: ItemId, ev: Event, cx: &mut Cx) -> Vec<Effect> {
        let Some(e) = &self.entry else { return Vec::new() };
        let id = match picked_action(&self.actions(cx, e), id, &ev) {
            Some(a) => a,
            // a copy button on a heading line
            None if ev == Event::Picked(0) && matches!(id, B_COPY | B_COPY_RAW | B_COPY_ORIGINAL) => id,
            None if ev == Event::Pressed => id,
            None => return Vec::new(),
        };
        match id {
            B_COPY => vec![Effect::CopyText(e.text.trim().to_string()), clipboard_toast()],
            B_COPY_RAW => vec![Effect::CopyText(e.raw.trim().to_string()), clipboard_toast()],
            B_COPY_ORIGINAL => vec![Effect::CopyText(e.original.clone().unwrap_or_default().trim().to_string()), clipboard_toast()],
            B_RETRY if self.can_retry(cx) => vec![Effect::Do(PopupAction::Retry), Effect::Toast("Retrying the last dictation".into()), Effect::Back],
            B_DELETE => vec![Effect::Confirm {
                title: "Delete this dictation?".into(),
                body: "It is removed from your history. This cannot be undone.".into(),
                action: "Delete".into(),
                destructive: true,
                on_confirm: ConfirmId::HistoryDelete(e.id),
            }],
            _ => Vec::new(),
        }
    }

    fn confirmed(&mut self, id: ConfirmId, _cx: &mut Cx) -> Vec<Effect> {
        let ConfirmId::HistoryDelete(del) = id else { return Vec::new() };
        if del != self.id {
            return Vec::new();
        }
        self.env.tomb.lock().unwrap_or_else(|e| e.into_inner()).ids.insert(del);
        self.entry = None;
        vec![Effect::Do(PopupAction::HistoryDelete(del)), Effect::Toast("Dictation deleted".into()), Effect::Back]
    }
}

// ---------------------------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------------------------

#[cfg(test)]
pub(super) mod testkit {
    //! Text snapshots of `items()` for reviewing pages without a renderer.
    use super::super::{FieldKind, Item, Trailing};

    pub fn dump(items: &[Item]) -> String {
        items.iter().map(line).collect::<Vec<_>>().join("\n")
    }

    pub fn line(it: &Item) -> String {
        match it {
            Item::Group(t) => format!("== {t}"),
            Item::Note(t) => format!("note: {t}"),
            Item::Text(t) => format!("text: {t}"),
            Item::Banner { tone, text } => format!("banner[{tone:?}]: {text}"),
            Item::Toggle { id, label, desc, on } => format!("#{id} toggle {label} [{}] ({desc})", if *on { "on" } else { "off" }),
            Item::Segmented { id, label, options, sel } => format!("#{id} segmented {label} {options:?} sel={sel}"),
            Item::Choice { id, label, value } => format!("#{id} choice {label} = {value}"),
            Item::Radio { id, label, desc, on } => format!("#{id} radio {label} [{}] ({desc})", if *on { "x" } else { " " }),
            Item::Field { id, label, hint, value, kind, error, max_chars } => {
                let k = match kind {
                    FieldKind::Text => "text".to_string(),
                    FieldKind::Secret => "secret".into(),
                    FieldKind::Multiline { rows } => format!("multiline{rows}"),
                    FieldKind::Hotkey => "hotkey".into(),
                    FieldKind::Search => "search".into(),
                };
                format!("#{id} field {label} [{k}] value={value:?} hint={hint:?} max={max_chars}{}", error.as_ref().map(|e| format!(" ERROR={e}")).unwrap_or_default())
            }
            Item::Button { id, label, style, enabled } => format!("#{id} button {label} [{style:?}]{}", if *enabled { "" } else { " (disabled)" }),
            Item::Tools { id, label, sub, tools } => {
                let t: Vec<String> = tools.iter().map(|t| format!("{}{}", t.tip, if t.enabled { "" } else { " (disabled)" })).collect();
                format!("#{id} tools {label} | {sub} | {t:?}")
            }
            Item::Result { ok, badge, text } => format!("result[{ok}] {badge}: {text}"),
            Item::Row(r) => {
                let t = match &r.trailing {
                    Trailing::None => "-".to_string(),
                    Trailing::Chevron => ">".into(),
                    Trailing::Value(v) => format!("{v} >"),
                    Trailing::Badge { text, tone } => format!("badge {text}/{tone:?}"),
                    Trailing::Delete => "x".into(),
                    Trailing::Copy => "copy".into(),
                };
                let chips: Vec<String> = r.chips.iter().map(|(c, t)| format!("{c}/{t:?}")).collect();
                format!("#{} row h={} | {} | {} | {} | chips={chips:?} | {t}", r.id, r.height, r.primary, r.secondary, r.meta)
            }
            Item::Divider => "---".into(),
            Item::Space(h) => format!("space {h}"),
        }
    }

    /// Everything a page shows, top to bottom: items, virtual rows (first six), tail items.
    pub fn snapshot(m: &dyn super::PageModel, cx: &super::Cx) -> String {
        let mut out = vec![format!("# {}", m.title()), dump(&m.items(cx))];
        if let Some(v) = m.virtual_rows() {
            out.push(format!("[virtual rows: {} x {}px]", v.count, v.row_h));
            for i in 0..v.count.min(6) {
                out.push(line(&(v.row)(i)));
            }
            if v.count > 6 {
                out.push(format!("... {} more", v.count - 6));
            }
        }
        let tail = m.tail_items(cx);
        if !tail.is_empty() {
            out.push(dump(&tail));
        }
        out.join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::testkit::*;
    use super::*;
    use crate::core::config::Config;
    use crate::core::timefmt::days_from_civil;

    fn inline(job: Box<dyn FnOnce() + Send>) {
        job();
    }

    fn utc(_: i64) -> i64 {
        0
    }

    fn ms(y: i32, m: u32, d: u32, h: i64, min: i64) -> i64 {
        (days_from_civil(y, m, d) * 86_400 + h * 3600 + min * 60) * 1000
    }

    /// 2026-10-09 12:00 UTC.
    fn now() -> i64 {
        ms(2026, 10, 9, 12, 0)
    }

    struct Fake {
        /// Newest first.
        entries: Mutex<Vec<Entry>>,
        searches: Mutex<Vec<String>>,
    }

    impl Fake {
        fn new(entries: Vec<Entry>) -> Arc<Fake> {
            Arc::new(Fake { entries: Mutex::new(entries), searches: Mutex::default() })
        }
    }

    impl Store for Fake {
        fn load_recent(&self, n: usize) -> history::Recent {
            let all = self.entries.lock().unwrap();
            history::Recent { entries: all.iter().take(n).cloned().collect(), more: all.len() > n }
        }
        fn find(&self, id: i64) -> Option<Entry> {
            self.entries.lock().unwrap().iter().find(|e| e.id == id).cloned()
        }
        fn search(&self, query: &str, cap: usize) -> history::Recent {
            self.searches.lock().unwrap().push(query.to_string());
            let all = self.entries.lock().unwrap();
            let hits: Vec<Entry> = histview::visible(&all, query, true).into_iter().map(|i| all[i].clone()).collect();
            history::Recent { more: hits.len() > cap, entries: hits.into_iter().take(cap).collect() }
        }
        fn summary(&self) -> Stats {
            histview::stats(&self.entries.lock().unwrap())
        }
    }

    fn env(store: Arc<Fake>) -> Env {
        Env { store, spawn: inline, tomb: Arc::default(), handoff: Arc::default(), offset_of: utc }
    }

    fn e(id: i64, ts: i64, text: &str) -> Entry {
        Entry { id, ts, text: text.into(), raw: text.into(), provider: "groq".into(), model: "whisper-large-v3-turbo".into(), latency_ms: 840, ..Default::default() }
    }

    fn sample() -> Vec<Entry> {
        vec![
            Entry { app_name: Some("Slack".into()), ai_cleaned: true, raw: "uh ship it on friday".into(), ..e(5, ms(2026, 10, 9, 11, 30), "Ship it on Friday.") },
            Entry { status: Status::Failed, error: Some("Groq rejected the key (401).".into()), text: String::new(), raw: String::new(), ..e(4, ms(2026, 10, 9, 9, 5), "") },
            Entry { status: Status::Discarded, text: String::new(), raw: String::new(), duration_ms: 300, ..e(3, ms(2026, 10, 8, 16, 0), "") },
            Entry {
                rewritten: true,
                original: Some("send the report tomorrow morning if possible thanks".into()),
                raw: "send the report tomorrow morning if possible thanks rewrite this concisely".into(),
                app_name: Some("Outlook".into()),
                ..e(2, ms(2026, 10, 2, 8, 0), "Send the report tomorrow.")
            },
            e(1, ms(2025, 12, 31, 23, 0), "Happy new year"),
        ]
    }

    fn new_page(store: Arc<Fake>) -> HistoryPage {
        HistoryPage::new(env(store))
    }

    fn entered(store: Arc<Fake>, cfg: &mut Config) -> HistoryPage {
        let mut p = new_page(store);
        p.enter(&mut Cx::new(cfg, now()));
        p
    }

    fn rows(p: &HistoryPage) -> Vec<i64> {
        (0..p.view.len()).map(|i| p.entry_at(i).unwrap().id).collect()
    }

    #[test]
    fn list_snapshot_shows_stats_search_toggle_rows_and_clear() {
        let mut cfg = Config::default();
        let p = entered(Fake::new(sample()), &mut cfg);
        let cx = Cx::new(&mut cfg, now());
        let snap = snapshot(&p, &cx);
        println!("{snap}");
        let want = [
            "# History",
            "note: 3 dictations \u{b7} 11 words \u{b7} avg 840 ms \u{b7} 1 failed",
            "#1 field  [search] value=\"\" hint=\"Search\" max=200",
            "#2 toggle Show discarded [off] ()",
            "[virtual rows: 4 x 58px]",
            "#100 row h=58 | 11:30 | Ship it on Friday. | 840 ms | chips=[\"AI/Info\"] | copy",
            "#101 row h=58 | 09:05 | Groq rejected the key (401). |  | chips=[\"Failed/Err\"] | >",
            "#102 row h=58 | Oct 2, 08:00 | Send the report tomorrow. | 840 ms | chips=[\"Rewritten/Info\"] | copy",
            "#103 row h=58 | Dec 31, 2025, 23:00 | Happy new year | 840 ms | chips=[] | copy",
            "space 4",
            "#4 tools Clear history |  | [\"Clear all\"]",
        ]
        .join("\n");
        assert_eq!(snap, want);
    }

    #[test]
    fn show_discarded_adds_the_discarded_rows() {
        let mut cfg = Config::default();
        let mut p = entered(Fake::new(sample()), &mut cfg);
        assert_eq!(rows(&p), vec![5, 4, 2, 1]);
        let mut cx = Cx::new(&mut cfg, now());
        assert!(p.event(T_DISCARDED, Event::Toggled(true), &mut cx).is_empty());
        assert_eq!(rows(&p), vec![5, 4, 3, 2, 1]);
        let r = p.row(2);
        assert_eq!(r.primary, "Yesterday 16:00");
        assert_eq!(r.chips, vec![("Discarded".to_string(), Tone::Warn)]);
        assert_eq!(r.secondary, "(empty)");
        assert_eq!(r.trailing, Trailing::Chevron);
        p.event(T_DISCARDED, Event::Toggled(false), &mut cx);
        assert_eq!(rows(&p), vec![5, 4, 2, 1]);
    }

    #[test]
    fn search_filters_live_over_the_loaded_entries() {
        let mut cfg = Config::default();
        let fake = Fake::new(sample());
        let mut p = entered(fake.clone(), &mut cfg);
        let mut cx = Cx::new(&mut cfg, now());
        p.event(F_SEARCH, Event::Edited("slack".into()), &mut cx);
        assert_eq!(rows(&p), vec![5]);
        p.event(F_SEARCH, Event::Edited("report TOMORROW".into()), &mut cx);
        assert_eq!(rows(&p), vec![2], "all words, any case");
        p.event(F_SEARCH, Event::Edited("rejected 401".into()), &mut cx);
        assert_eq!(rows(&p), vec![4], "error text is searchable");
        p.event(F_SEARCH, Event::Edited("zzz".into()), &mut cx);
        assert!(rows(&p).is_empty());
        assert!(p.virtual_rows().is_none());
        assert!(p.items(&cx).iter().any(|i| matches!(i, Item::Note(n) if n == "No dictations match \u{201c}zzz\u{201d}.")));
        // Everything was loaded, so a commit needs no whole-file scan.
        p.event(F_SEARCH, Event::Committed("zzz".into()), &mut cx);
        assert!(fake.searches.lock().unwrap().is_empty());
        p.event(F_SEARCH, Event::Committed("".into()), &mut cx);
        assert_eq!(rows(&p), vec![5, 4, 2, 1]);
    }

    fn many(n: i64) -> Vec<Entry> {
        (1..=n).rev().map(|i| e(i, 1_000 + i, &format!("note{i:04} body"))).collect()
    }

    #[test]
    fn a_long_history_loads_one_page_then_load_older_reads_further() {
        let mut cfg = Config::default();
        let mut p = entered(Fake::new(many(450)), &mut cfg);
        assert_eq!(p.pool.len(), PAGE);
        assert!(p.more);
        // The totals worker (inline here) has already delivered the whole-file numbers.
        assert_eq!(p.stats_note().unwrap(), "450 dictations \u{b7} 900 words \u{b7} avg 840 ms");
        let cx = Cx::new(&mut cfg, now());
        assert!(p.tail_items(&cx).iter().any(|i| matches!(i, Item::Button { id: B_MORE, .. })));
        let mut cx = Cx::new(&mut cfg, now());
        p.event(B_MORE, Event::Pressed, &mut cx);
        assert_eq!(p.pool.len(), 400);
        p.event(B_MORE, Event::Pressed, &mut cx);
        assert_eq!(p.pool.len(), 450);
        assert!(!p.more);
        assert!(!p.tail_items(&cx).iter().any(|i| matches!(i, Item::Button { id: B_MORE, .. })), "nothing older left");
        assert_eq!(p.entry_at(0).unwrap().id, 450);
        assert_eq!(p.entry_at(449).unwrap().id, 1);
    }

    #[test]
    fn a_full_pool_stops_loading_but_search_still_reaches_the_rest_of_the_file() {
        let mut cfg = Config::default();
        let fake = Fake::new(many(MAX_LOADED as i64 + 300));
        let mut p = entered(fake.clone(), &mut cfg);
        let mut cx = Cx::new(&mut cfg, now());
        while p.can_load_more() {
            p.event(B_MORE, Event::Pressed, &mut cx);
        }
        assert_eq!(p.pool.len(), MAX_LOADED);
        assert!(p.more, "older entries are still on disk");
        let tail = p.tail_items(&cx);
        assert!(!tail.iter().any(|i| matches!(i, Item::Button { id: B_MORE, .. })));
        assert!(tail.iter().any(|i| matches!(i, Item::Note(n) if n.starts_with("Showing the newest 2,000"))));
        assert!(p.event(B_MORE, Event::Pressed, &mut cx).is_empty() && p.pool.len() == MAX_LOADED, "a stray press does nothing");
        // The oldest entries are outside the pool; a committed search finds them on the worker.
        p.event(F_SEARCH, Event::Committed("note0005".into()), &mut cx);
        p.wake(&mut cx);
        assert_eq!(*fake.searches.lock().unwrap(), vec!["note0005"]);
        assert_eq!(rows(&p), vec![5]);
    }

    #[test]
    fn a_search_with_more_matches_than_the_cap_says_so() {
        let mut cfg = Config::default();
        let mut p = entered(Fake::new(many(MAX_LOADED as i64 + 50)), &mut cfg);
        let mut cx = Cx::new(&mut cfg, now());
        p.event(F_SEARCH, Event::Committed("body".into()), &mut cx);
        p.wake(&mut cx);
        assert_eq!(p.view.len(), MAX_LOADED);
        assert!(p.tail_items(&cx).iter().any(|i| matches!(i, Item::Note(n) if n.starts_with("Showing the newest 2,000 matches"))));
    }

    #[test]
    fn a_whole_file_total_arriving_after_clear_all_does_not_resurrect_the_numbers() {
        let mut cfg = Config::default();
        let mut p = new_page(Fake::new(many(300)));
        static JOBS: Mutex<Vec<Box<dyn FnOnce() + Send>>> = Mutex::new(Vec::new());
        p.env.spawn = |job| JOBS.lock().unwrap().push(job);
        let mut cx = Cx::new(&mut cfg, now());
        p.enter(&mut cx);
        assert_eq!(JOBS.lock().unwrap().len(), 1, "300 entries: part of the file is loaded, the totals need a scan");
        p.confirmed(ConfirmId::HistoryClear, &mut cx);
        let jobs: Vec<_> = JOBS.lock().unwrap().drain(..).collect();
        jobs.into_iter().for_each(|j| j());
        p.wake(&mut cx);
        assert_eq!(p.stats_note(), None, "the cleared page stays empty");
    }

    #[test]
    fn a_complete_pool_needs_no_totals_scan() {
        let mut cfg = Config::default();
        let mut p = new_page(Fake::new(sample()));
        static RAN: Mutex<usize> = Mutex::new(0);
        p.env.spawn = |_job| *RAN.lock().unwrap() += 1;
        p.enter(&mut Cx::new(&mut cfg, now()));
        assert_eq!(*RAN.lock().unwrap(), 0);
        assert!(p.stats_note().is_some());
    }

    #[test]
    fn stats_say_latest_until_the_whole_file_total_is_known() {
        let mut cfg = Config::default();
        let mut p = new_page(Fake::new(many(300)));
        p.env.spawn = |_job| {}; // the totals worker never finishes
        p.enter(&mut Cx::new(&mut cfg, now()));
        assert_eq!(p.stats_note().unwrap(), "Newest 200 entries \u{b7} 200 dictations \u{b7} 400 words \u{b7} avg 840 ms");
    }

    #[test]
    fn a_committed_search_over_a_partial_history_scans_the_whole_file_on_a_worker() {
        let mut cfg = Config::default();
        let fake = Fake::new(many(450));
        let mut p = entered(fake.clone(), &mut cfg);
        let mut cx = Cx::new(&mut cfg, now());
        // Live filtering only sees the loaded 200 (ids 251..450): nothing there matches.
        p.event(F_SEARCH, Event::Edited("note007".into()), &mut cx);
        assert!(rows(&p).is_empty());
        // The commit starts the scan; the inline worker answered at once and left its result in the slot.
        p.event(F_SEARCH, Event::Committed("note007".into()), &mut cx);
        assert_eq!(*fake.searches.lock().unwrap(), vec!["note007"]);
        assert!(p.pending.is_some());
        assert!(p.items(&cx).iter().chain(p.tail_items(&cx).iter()).any(|i| matches!(i, Item::Note(n) if n.starts_with("Searching"))));
        p.wake(&mut cx);
        assert!(p.pending.is_none());
        assert_eq!(rows(&p), vec![79, 78, 77, 76, 75, 74, 73, 72, 71, 70]);
        assert!(!p.tail_items(&cx).iter().any(|i| matches!(i, Item::Note(n) if n.starts_with("Searching"))));
        // The same query again does not rescan.
        p.event(F_SEARCH, Event::Committed("note007".into()), &mut cx);
        assert_eq!(fake.searches.lock().unwrap().len(), 1);
        // Editing the field falls back to the live filter over the pool.
        p.event(F_SEARCH, Event::Edited("note0070".into()), &mut cx);
        assert!(rows(&p).is_empty());
        // Clearing the field returns to the pool and drops the answer.
        p.event(F_SEARCH, Event::Committed(String::new()), &mut cx);
        assert!(p.hits.is_none());
        assert_eq!(p.view.len(), PAGE);
    }

    #[test]
    fn a_stale_search_result_is_dropped() {
        let mut cfg = Config::default();
        let mut p = new_page(Fake::new(many(450)));
        // Deferred worker: keep the jobs and run them by hand.
        static JOBS: Mutex<Vec<Box<dyn FnOnce() + Send>>> = Mutex::new(Vec::new());
        p.env.spawn = |job| JOBS.lock().unwrap().push(job);
        let run_all = || {
            let jobs: Vec<_> = JOBS.lock().unwrap().drain(..).collect();
            for j in jobs {
                j();
            }
        };
        let mut cx = Cx::new(&mut cfg, now());
        p.enter(&mut cx);
        run_all();
        p.event(F_SEARCH, Event::Committed("note001".into()), &mut cx);
        assert_eq!(p.pending.as_ref().unwrap().1, "note001");
        p.event(F_SEARCH, Event::Committed("note002".into()), &mut cx);
        assert_eq!(p.pending.as_ref().unwrap().1, "note002");
        run_all(); // both finish; the first answer is overwritten in the slot by the second
        p.wake(&mut cx);
        assert!(p.pending.is_none());
        assert_eq!(p.hits.as_ref().unwrap().query, "note002");
        assert_eq!(rows(&p), (20..=29).rev().collect::<Vec<i64>>());
        // A late result for a query that is no longer in the field is ignored.
        p.event(F_SEARCH, Event::Committed("note003".into()), &mut cx);
        p.event(F_SEARCH, Event::Edited("something else".into()), &mut cx);
        run_all();
        p.wake(&mut cx);
        assert!(p.hits.as_ref().is_none_or(|h| h.query != "note003"));
    }

    #[test]
    fn row_events_open_the_detail_or_copy() {
        let mut cfg = Config::default();
        let mut p = entered(Fake::new(sample()), &mut cfg);
        let mut cx = Cx::new(&mut cfg, now());
        assert_eq!(p.row_event(0, Event::Activated, &mut cx), vec![Effect::Navigate(Route::HistoryEntry(5))]);
        assert_eq!(p.row_event(0, Event::Copy, &mut cx), vec![Effect::CopyText("Ship it on Friday.".into()), Effect::Toast("Copied to clipboard".into())]);
        assert_eq!(p.row_event(2, Event::Activated, &mut cx), vec![Effect::Navigate(Route::HistoryEntry(2))], "index is into the visible rows");
        assert!(p.row_event(99, Event::Activated, &mut cx).is_empty());
    }

    #[test]
    fn the_detail_page_takes_the_entry_from_the_list_instead_of_reading_the_file() {
        let mut cfg = Config::default();
        let store = Fake::new(sample());
        let e = env(store.clone());
        let mut list = HistoryPage::new(e.clone());
        list.enter(&mut Cx::new(&mut cfg, now()));
        list.row_event(0, Event::Activated, &mut Cx::new(&mut cfg, now()));
        // The file no longer has it (e.g. it was far back and the store would have to scan for it).
        store.entries.lock().unwrap().clear();
        let mut p = EntryPage::new(e.clone(), 5);
        open(&mut p, &mut cfg);
        assert_eq!(p.entry.as_ref().map(|x| x.id), Some(5));
        // A handed-over entry of another page is ignored and consumed once.
        let mut other = EntryPage::new(e, 4);
        open(&mut other, &mut cfg);
        assert!(other.entry.is_none());
    }

    #[test]
    fn clear_all_confirms_then_asks_the_daemon_and_empties_the_page() {
        let mut cfg = Config::default();
        let mut p = entered(Fake::new(sample()), &mut cfg);
        let mut cx = Cx::new(&mut cfg, now());
        let fx = p.event(B_CLEAR, Event::Pressed, &mut cx);
        assert!(matches!(&fx[..], [Effect::Confirm { destructive: true, on_confirm: ConfirmId::HistoryClear, .. }]));
        assert_eq!(p.pool.len(), 5, "nothing happens before the confirmation");
        let fx = p.confirmed(ConfirmId::HistoryClear, &mut cx);
        assert_eq!(fx, vec![Effect::Do(PopupAction::HistoryClear), Effect::Toast("History cleared".into())]);
        assert!(p.virtual_rows().is_none());
        let items = p.items(&cx);
        assert!(items.iter().any(|i| matches!(i, Item::Note(n) if n.starts_with("No dictations yet"))), "{}", dump(&items));
        assert!(matches!(p.tail_items(&cx).last(), Some(Item::Tools { id: B_CLEAR, tools, .. }) if !tools[0].enabled));
        // The daemon has not rewritten the file yet: coming back to the page must not resurrect rows.
        p.enter(&mut cx);
        assert!(p.pool.is_empty());
        // A dictation made after the clear shows up.
        p.env.store = Fake::new(vec![e(9, now() + 1000, "after")]);
        p.enter(&mut cx);
        assert_eq!(rows(&p), vec![9]);
    }

    #[test]
    fn history_off_shows_a_banner_and_the_empty_state_says_so() {
        let mut cfg = Config { history_enabled: false, ..Config::default() };
        let p = entered(Fake::new(Vec::new()), &mut cfg);
        let cx = Cx::new(&mut cfg, now());
        let snap = snapshot(&p, &cx);
        println!("{snap}");
        assert!(snap.contains("banner[Warn]: History is off"), "{snap}");
        assert!(snap.contains("note: No saved dictations."), "{snap}");
        assert!(!snap.contains("dictations \u{b7}"), "no stats for an empty history");
    }

    #[test]
    fn only_discarded_entries_explain_the_empty_list() {
        let mut cfg = Config::default();
        let only = vec![Entry { status: Status::Discarded, ..e(1, 10, "") }];
        let p = entered(Fake::new(only), &mut cfg);
        let cx = Cx::new(&mut cfg, now());
        assert!(p.items(&cx).iter().any(|i| matches!(i, Item::Note(n) if n.contains("Show discarded"))));
    }

    #[test]
    fn long_text_is_one_line_and_cut() {
        let mut cfg = Config::default();
        let long = format!("first line\nsecond   line {}", "word ".repeat(100));
        let p = entered(Fake::new(vec![e(1, now() - 5000, &long)]), &mut cfg);
        let r = p.row(0);
        assert!(r.secondary.starts_with("first line second line word"));
        assert!(r.secondary.chars().count() <= ROW_TEXT_CHARS + 2 && r.secondary.ends_with("..."), "{}", r.secondary.len());
    }

    #[test]
    fn row_ids_are_unique_and_do_not_collide_with_page_items() {
        let mut cfg = Config::default();
        let p = entered(Fake::new(many(150)), &mut cfg);
        let cx = Cx::new(&mut cfg, now());
        let fixed: Vec<ItemId> = p.items(&cx).iter().chain(p.tail_items(&cx).iter()).filter_map(Item::id).collect();
        let mut seen = HashSet::new();
        for i in 0..p.view.len() {
            let id = p.row(i).id;
            assert!(seen.insert(id) && !fixed.contains(&id));
        }
    }

    // ----- detail page

    fn detail(id: i64, entries: Vec<Entry>) -> EntryPage {
        EntryPage::new(env(Fake::new(entries)), id)
    }

    fn open(p: &mut EntryPage, cfg: &mut Config) {
        p.enter(&mut Cx::new(cfg, now()));
    }

    #[test]
    fn detail_of_a_rewritten_entry_shows_text_original_raw_and_copy_buttons() {
        let mut cfg = Config::default();
        let mut p = detail(2, sample());
        open(&mut p, &mut cfg);
        let snap = dump(&p.items(&Cx::new(&mut cfg, now())));
        println!("{snap}");
        let want = [
            "note: Oct 2, 2026, 08:00 \u{b7} Outlook",
            "note: Groq \u{b7} whisper-large-v3-turbo \u{b7} 840 ms",
            "banner[Info]: Rewritten by AI: you ended with the rewrite phrase.",
            "#1 tools Text |  | [\"Copy text\"]",
            "text: Send the report tomorrow.",
            "#3 tools Before the rewrite |  | [\"Copy text before the rewrite\"]",
            "text: send the report tomorrow morning if possible thanks",
            "#2 tools Raw transcript |  | [\"Copy raw transcript\"]",
            "text: send the report tomorrow morning if possible thanks rewrite this concisely",
            "---",
            "#5 tools  |  | [\"Delete\"]",
        ]
        .join("\n");
        assert_eq!(snap, want);
        let mut cx = Cx::new(&mut cfg, now());
        assert_eq!(p.event(B_COPY_ORIGINAL, Event::Pressed, &mut cx), vec![Effect::CopyText("send the report tomorrow morning if possible thanks".into()), Effect::Toast("Copied to clipboard".into())]);
        assert_eq!(p.event(B_COPY_RAW, Event::Pressed, &mut cx)[0], Effect::CopyText("send the report tomorrow morning if possible thanks rewrite this concisely".into()));
        assert_eq!(p.event(B_COPY, Event::Pressed, &mut cx)[0], Effect::CopyText("Send the report tomorrow.".into()));
    }

    #[test]
    fn a_rewritten_entry_without_a_saved_original_says_so() {
        let mut cfg = Config::default();
        let old = Entry { rewritten: true, original: None, ..e(1, now() - 1000, "Short.") };
        let mut p = detail(1, vec![old]);
        open(&mut p, &mut cfg);
        let snap = dump(&p.items(&Cx::new(&mut cfg, now())));
        assert!(snap.contains("== Before the rewrite\ntext: (not saved)") && !snap.contains("Copy text before"), "{snap}");
    }

    #[test]
    fn detail_of_a_plain_entry_has_no_raw_section_when_nothing_differs() {
        let mut cfg = Config::default();
        let mut p = detail(1, sample());
        open(&mut p, &mut cfg);
        let snap = dump(&p.items(&Cx::new(&mut cfg, now())));
        assert!(!snap.contains("Raw transcript") && !snap.contains("Before the rewrite") && !snap.contains("Retry"), "{snap}");
        assert!(snap.contains("Dec 31, 2025, 23:00") && snap.contains("#1 tools Text |  | [\"Copy text\"]\ntext: Happy new year"), "{snap}");
    }

    #[test]
    fn detail_of_a_cleaned_entry_shows_the_raw_transcript() {
        let mut cfg = Config::default();
        let mut p = detail(5, sample());
        open(&mut p, &mut cfg);
        let snap = dump(&p.items(&Cx::new(&mut cfg, now())));
        assert!(snap.contains("banner[Info]: Cleaned up by AI.") && snap.contains("tools Raw transcript |  | [\"Copy raw transcript\"]\ntext: uh ship it on friday"), "{snap}");
        assert!(snap.contains("Oct 9, 2026, 11:30 \u{b7} Slack"), "{snap}");
    }

    #[test]
    fn failed_newest_entry_can_be_retried_only_with_the_kept_recording() {
        let mut cfg = Config::default();
        let list = vec![Entry { status: Status::Failed, error: Some("timeout".into()), text: String::new(), raw: String::new(), ..e(9, now() - 1000, "") }, e(8, now() - 5000, "older")];
        let mut p = detail(9, list);
        open(&mut p, &mut cfg);
        let snap = dump(&p.items(&Cx::new(&mut cfg, now())));
        println!("{snap}");
        assert!(snap.starts_with("banner[Err]: timeout"), "{snap}");
        assert!(snap.contains("#4 tools  |  | [\"Retry transcription\", \"Delete\"]") && !snap.contains("Copy text"), "{snap}");
        assert!(snap.contains("note: No transcript was recorded."));
        let fx = p.event(B_RETRY, Event::Pressed, &mut Cx::new(&mut cfg, now()));
        assert_eq!(fx, vec![Effect::Do(PopupAction::Retry), Effect::Toast("Retrying the last dictation".into()), Effect::Back]);
        // Without the kept recording there is nothing to retry, and the button is gone.
        cfg.keep_failed_audio = false;
        assert!(!dump(&p.items(&Cx::new(&mut cfg, now()))).contains("Retry"));
        assert!(p.event(B_RETRY, Event::Pressed, &mut Cx::new(&mut cfg, now())).is_empty());
        // An older failed entry cannot be retried even with the recording kept.
        cfg.keep_failed_audio = true;
        let older_failed = vec![e(10, now(), "newer"), Entry { status: Status::Failed, ..e(9, now() - 1000, "") }];
        let mut q = detail(9, older_failed);
        open(&mut q, &mut cfg);
        assert!(!dump(&q.items(&Cx::new(&mut cfg, now()))).contains("Retry"));
    }

    #[test]
    fn discarded_entry_offers_recover_recording() {
        let mut cfg = Config::default();
        let mut p = detail(3, vec![Entry { status: Status::Discarded, duration_ms: 400, ..e(3, now() - 1000, "") }]);
        open(&mut p, &mut cfg);
        let snap = dump(&p.items(&Cx::new(&mut cfg, now())));
        assert!(snap.contains("banner[Warn]: Discarded") && snap.contains("Recover recording") && snap.contains("400 ms of speech"), "{snap}");
        assert!(!snap.contains("No transcript was recorded"), "a discarded recording has no transcript by design");
    }

    #[test]
    fn delete_confirms_then_hides_the_entry_everywhere() {
        let mut cfg = Config::default();
        let fake = Fake::new(sample());
        let tomb: Arc<Mutex<Tombstones>> = Arc::default();
        let mk = |tomb: &Arc<Mutex<Tombstones>>| Env { store: fake.clone(), spawn: inline, tomb: tomb.clone(), handoff: Arc::default(), offset_of: utc };
        let mut list = HistoryPage::new(mk(&tomb));
        list.enter(&mut Cx::new(&mut cfg, now()));
        let mut d = EntryPage::new(mk(&tomb), 2);
        let mut cx = Cx::new(&mut cfg, now());
        d.enter(&mut cx);
        let fx = d.event(B_DELETE, Event::Pressed, &mut cx);
        assert!(matches!(&fx[..], [Effect::Confirm { destructive: true, on_confirm: ConfirmId::HistoryDelete(2), .. }]));
        assert!(d.confirmed(ConfirmId::HistoryDelete(7), &mut cx).is_empty(), "someone else's confirmation");
        let fx = d.confirmed(ConfirmId::HistoryDelete(2), &mut cx);
        assert_eq!(fx, vec![Effect::Do(PopupAction::HistoryDelete(2)), Effect::Toast("Dictation deleted".into()), Effect::Back]);
        // Back on the list: the file still has the entry (the daemon has not run), but the page hides it.
        list.enter(&mut cx);
        assert_eq!(rows(&list), vec![5, 4, 1]);
        // Opening the deleted entry again says it is gone.
        let mut again = EntryPage::new(mk(&tomb), 2);
        again.enter(&mut cx);
        assert_eq!(dump(&again.items(&cx)), "note: This dictation is no longer in your history.");
    }

    #[test]
    fn a_missing_entry_shows_a_note_and_inert_events() {
        let mut cfg = Config::default();
        let mut p = detail(404, sample());
        open(&mut p, &mut cfg);
        let mut cx = Cx::new(&mut cfg, now());
        assert_eq!(dump(&p.items(&cx)), "note: This dictation is no longer in your history.");
        assert!(p.event(B_DELETE, Event::Pressed, &mut cx).is_empty());
    }

    #[test]
    fn real_constructors_build_pages_that_survive_an_empty_or_missing_history() {
        let mut cfg = Config::default();
        let mut cx = Cx::new(&mut cfg, now());
        let mut m = model();
        m.enter(&mut cx);
        assert_eq!(m.title(), "History");
        assert!(!m.items(&cx).is_empty());
        let mut d = entry_model(-1);
        d.enter(&mut cx);
        assert_eq!(d.title(), "Dictation");
        assert!(!d.items(&cx).is_empty());
        d.leave(&mut cx);
        m.leave(&mut cx);
    }
}
