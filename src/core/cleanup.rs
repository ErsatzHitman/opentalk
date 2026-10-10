//! Deterministic transcript cleanup (always on, < 1 ms). OpenWhispr had none (LLM-only);
//! see docs/PLAN.md §6 for the required stages and docs/SPEC.md.
//!
//! Order: strip non-speech tags -> hallucination guard -> loop collapse -> hesitation fillers ->
//! context fillers (like / you know) -> [aggressive extras] -> stutters/false starts ->
//! punctuation repair + capitalization -> dictionary case-fix -> fuzzy dictionary ->
//! spoken line breaks -> snippets (last).
//!
//! Filler mode `Off` keeps the transcript verbatim apart from the stages that remove garbage the
//! recognizer invented (tags, hallucinations, loops) and the dictionary / line-break / snippet
//! features, which are separate settings.
use crate::core::config::{FillerMode, Snippet};
use crate::core::dictionary;

mod fillers;
mod lists;
mod prep;
mod repair;
mod stutter;
mod tokens;

#[cfg(test)]
mod tests;

#[derive(Clone, Debug)]
pub struct CleanupCtx<'a> {
    /// "auto" or language code ("en", "de", ...). Detected language preferred when known.
    pub language: &'a str,
    pub filler_mode: FillerMode,
    pub dictionary: &'a [String],
    /// Only read by [`process`] (tests); the pipeline expands snippets itself, after the LLM.
    #[allow(dead_code)]
    pub snippets: &'a [Snippet],
    /// Milliseconds of detected speech in the recording (hallucination guard). None = unknown.
    pub speech_ms: Option<u64>,
    pub fuzzy_dictionary: bool,
    pub spoken_line_breaks: bool,
}

/// Below this much detected speech the recording is treated as near-silent and known
/// hallucinated phrases ("Thank you for watching") are dropped.
const HALLUCINATION_SPEECH_MS: u64 = 600;

/// Primary language subtag, lower-cased; "" for auto / unknown.
fn primary_language(language: &str) -> String {
    let l = language.trim();
    if l.eq_ignore_ascii_case("auto") {
        return String::new();
    }
    l.split(['-', '_']).next().unwrap_or("").to_ascii_lowercase()
}

/// Cheap pre-check for "new line" / "new paragraph" so the common case skips a second pass.
fn has_break_phrase(s: &str) -> bool {
    s.as_bytes().windows(5).any(|w| {
        w[..4].eq_ignore_ascii_case(b"new ") && matches!(w[4].to_ascii_lowercase(), b'l' | b'p')
    })
}

/// Full deterministic pipeline. Returns "" when nothing but fillers/hallucination remains.
/// The daemon runs [`process_pre_llm`] and expands snippets itself, so only tests call this.
#[cfg(test)]
pub fn process(raw: &str, ctx: &CleanupCtx) -> String {
    let text = process_pre_llm(raw, ctx);
    if text.is_empty() {
        return text;
    }
    dictionary::expand_snippets(&text, ctx.snippets)
}

/// Pipeline without snippet expansion (used before optional LLM cleanup; snippets run last).
pub fn process_pre_llm(raw: &str, ctx: &CleanupCtx) -> String {
    // 1. Non-speech tags.
    let text = prep::strip_nonspeech(raw);
    if text.trim().is_empty() {
        return String::new();
    }
    // 2. Hallucination guard: only for recordings with almost no speech.
    if ctx.speech_ms.is_some_and(|ms| ms < HALLUCINATION_SPEECH_MS) && prep::is_hallucination(&text) {
        return String::new();
    }
    let mut doc = tokens::tokenize(&text);
    // 3. Loop collapse.
    prep::collapse_loops(&mut doc);

    let lang = primary_language(ctx.language);
    let auto = lang.is_empty();
    if ctx.filler_mode != FillerMode::Off {
        // "um" / "er" are words in German, Portuguese, Dutch and Scandinavian languages: with an
        // unknown language they only count as fillers when the text shows no sign of those.
        let english = lang == "en"
            || (auto && !doc.toks.iter().any(|t| t.is_word() && lists::is_non_english_marker(&t.lc)));
        let english_marker = auto && doc.toks.iter().any(|t| t.is_word() && lists::is_english_marker(&t.lc));
        // 4-6. Hesitations, context fillers, aggressive extras (with their commas).
        fillers::remove_fillers(
            &mut doc,
            &fillers::Opts {
                en: english,
                um_at_pause_only: auto && !english_marker,
                ja: auto || lang == "ja",
                zh: auto || lang == "zh" || lang == "yue",
                aggressive: ctx.filler_mode == FillerMode::Aggressive,
                dictionary: ctx.dictionary,
            },
        );
        // 7. Stutters and false starts.
        stutter::collapse_stutters(&mut doc);
        // 8. Punctuation repair, capitalization, lone "i".
        repair::repair(
            &mut doc,
            &repair::RepairOpts {
                turkish: matches!(lang.as_str(), "tr" | "az"),
                english: lang == "en",
                french: lang == "fr",
                auto,
            },
        );
    }
    if !doc.toks.iter().any(|t| t.is_word()) {
        return String::new();
    }
    let mut out = doc.render();

    // 9-10. Dictionary.
    if !ctx.dictionary.is_empty() {
        out = dictionary::case_fix(&out, ctx.dictionary);
        if ctx.fuzzy_dictionary {
            out = dictionary::fuzzy_fix(&out, ctx.dictionary);
        }
    }
    // 11. Spoken line breaks.
    if ctx.spoken_line_breaks && has_break_phrase(&out) {
        let mut doc = tokens::tokenize(&out);
        repair::apply_line_breaks(&mut doc, matches!(lang.as_str(), "tr" | "az"));
        out = doc.render();
    }
    let trimmed = out.trim_end_matches([' ', '\t']).len();
    out.truncate(trimmed);
    out
}
