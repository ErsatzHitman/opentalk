//! Deterministic transcript cleanup (always on, < 1 ms). OpenWhispr had none (LLM-only);
//! see docs/PLAN.md §6 for the required stages and docs/SPEC.md. TASK cleanup implements.
//!
//! Order: strip non-speech tags -> hallucination guard -> loop collapse -> hesitation fillers ->
//! context fillers (like / you know) -> [aggressive extras] -> stutters/false starts ->
//! punctuation repair + capitalization -> dictionary case-fix -> fuzzy dictionary ->
//! spoken line breaks -> snippets (last).
use crate::core::config::{FillerMode, Snippet};

#[derive(Clone, Debug)]
pub struct CleanupCtx<'a> {
    /// "auto" or language code ("en", "de", ...). Detected language preferred when known.
    pub language: &'a str,
    pub filler_mode: FillerMode,
    pub dictionary: &'a [String],
    pub snippets: &'a [Snippet],
    /// Milliseconds of detected speech in the recording (hallucination guard). None = unknown.
    pub speech_ms: Option<u64>,
    pub fuzzy_dictionary: bool,
    pub spoken_line_breaks: bool,
}

/// Full deterministic pipeline. Returns "" when nothing but fillers/hallucination remains.
pub fn process(raw: &str, ctx: &CleanupCtx) -> String {
    let _ = (raw, ctx);
    todo!("cleanup")
}

/// Pipeline without snippet expansion (used before optional LLM cleanup; snippets run last).
pub fn process_pre_llm(raw: &str, ctx: &CleanupCtx) -> String {
    let _ = (raw, ctx);
    todo!("cleanup")
}
