//! Settings pages (agent S): the index (`index_model`), one page per [`Section`] (`model`) and the
//! list pickers (`picker_model`). S adds `prefs.rs`, `hotkeys.rs`, `stt.rs`, `cleanup.rs`
//! (first group "AI rewrite": `rewrite_enabled`, `rewrite_trigger`), `privacy.rs`, `about.rs`,
//! `pickers.rs` next to this file and dispatches to them from here.
//!
//! STUB: placeholders until S implements them. The constructor signatures are the contract.
use super::{PageModel, PickerId, Placeholder, Section};

/// The Settings index (`Route::SettingsIndex`): one row per section plus a "Rewrite trigger"
/// shortcut row that jumps to the trigger field on the Cleanup & AI page.
pub fn index_model() -> Box<dyn PageModel> {
    Placeholder::boxed("Settings")
}

/// One Settings section page (`Route::Settings`).
pub fn model(section: Section) -> Box<dyn PageModel> {
    Placeholder::boxed(section.label())
}

/// A list picker (`Route::Picker`): radio rows, optional filter field; picking applies and goes Back.
pub fn picker_model(id: PickerId) -> Box<dyn PageModel> {
    Placeholder::boxed(match id {
        PickerId::Language => "Language",
        PickerId::InputDevice => "Input device",
        PickerId::MicWarm => "Keep microphone warm",
        PickerId::Retention => "Keep transcripts for",
        PickerId::SttProvider => "Speech provider",
        PickerId::LlmProvider => "AI provider",
        PickerId::LlmModel => "AI model",
    })
}
