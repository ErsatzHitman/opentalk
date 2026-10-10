//! History pages (agent H): the list (`model`) with stats line, search, show-discarded and
//! "Clear all", virtualised by [`VirtualRows`](super::VirtualRows) with `ROW_H_HISTORY` rows;
//! and the detail page (`entry_model`) with the full text, raw transcript, Copy, Retry, Delete.
//!
//! STUB: placeholders until H implements them. The constructor signatures are the contract.
use super::{PageModel, Placeholder};

/// History list page (`Route::History`).
pub fn model() -> Box<dyn PageModel> {
    Placeholder::boxed("History")
}

/// Detail page of one entry (`Route::HistoryEntry(id)`, `id` = `core::history::Entry::id`).
pub fn entry_model(id: i64) -> Box<dyn PageModel> {
    let _ = id;
    Placeholder::boxed("Dictation")
}
