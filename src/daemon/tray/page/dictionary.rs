//! Dictionary and snippet pages (agent H): words list with add/filter/budget banner/import/export,
//! snippets list, and the snippet editor (Trigger single-line, Replacement `Multiline { rows: 5 }`).
//!
//! STUB: placeholders until H implements them. The constructor signatures are the contract.
use super::{PageModel, Placeholder};

/// Dictionary words page (`Route::Dictionary`).
pub fn model() -> Box<dyn PageModel> {
    Placeholder::boxed("Dictionary")
}

/// Snippets list (`Route::Snippets`).
pub fn snippets_model() -> Box<dyn PageModel> {
    Placeholder::boxed("Snippets")
}

/// Snippet editor (`Route::SnippetEdit`): `Some(i)` edits `Config.snippets[i]`, `None` adds one.
pub fn snippet_edit_model(index: Option<usize>) -> Box<dyn PageModel> {
    Placeholder::boxed(if index.is_some() { "Edit snippet" } else { "New snippet" })
}
