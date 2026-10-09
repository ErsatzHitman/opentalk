//! Personal dictionary + snippets helpers. Ports OpenWhispr's dictionaryPromptCap.js,
//! dictionaryEchoFilter.js and snippets.ts; adds dictionary case-fix and conservative fuzzy fix.
use crate::core::config::Snippet;
use std::collections::HashSet;

mod common_words;
mod nfc;
mod snippets;
mod terms;

#[allow(unused_imports)]
pub use snippets::{expand_snippets, trigger_ranges};
pub use terms::{case_fix, fuzzy_fix};

/// Groq rejects prompts > 896 chars; 890 leaves margin for UTF-16 vs codepoint counting drift.
pub const GROQ_PROMPT_CHARS: usize = 890;
/// Whisper-family decoders read at most 223 prompt tokens; 900 chars only bounds the request.
pub const WHISPER_PROMPT_CHARS: usize = 900;
/// What the decoder's 223-token window is worth in characters (UI warning, not a trim).
pub const WHISPER_DECODER_PROMPT_CHARS: usize = 550;
/// gpt-4o-transcribe family: a guard against absurd lists, not a limit anyone should hit.
pub const TRANSCRIBE_PROMPT_CHARS: usize = 8000;
/// OpenAI accepts exactly 65,536 characters of overflow prompt for gpt-transcribe.
pub const TRANSCRIPTION_OVERFLOW_PROMPT_CHARS: usize = 65536;

/// Words fed to STT/LLM as hints: dictionary words + snippet triggers (deduped, case-insensitive).
pub fn hint_words(dictionary: &[String], snippets: &[Snippet]) -> Vec<String> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut out = Vec::with_capacity(dictionary.len() + snippets.len());
    let triggers = snippets.iter().map(|s| s.trigger.as_str());
    for word in dictionary.iter().map(String::as_str).chain(triggers) {
        let word = word.trim();
        if !word.is_empty() && seen.insert(word.to_lowercase()) {
            out.push(word.to_string());
        }
    }
    out
}

/// A dictionary prompt cut to a provider's budget.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrimmedPrompt {
    pub prompt: String,
    pub original_len: usize,
    pub truncated: bool,
}

/// Cut at the last comma inside the budget so no entry is sent half-spelled
/// (OpenWhispr `trimDictionaryPrompt`). Classify echoes against the returned prompt, not the
/// original. Lengths count chars.
pub fn trim_prompt(prompt: &str, max_chars: usize) -> TrimmedPrompt {
    let original_len = prompt.chars().count();
    if original_len <= max_chars {
        return TrimmedPrompt { prompt: prompt.to_string(), original_len, truncated: false };
    }
    let head_end = prompt.char_indices().nth(max_chars).map_or(prompt.len(), |(i, _)| i);
    let head = &prompt[..head_end];
    // A cut that lands exactly on an entry boundary keeps the whole head.
    let cut = if prompt[head_end..].starts_with(',') {
        head_end
    } else {
        match head.rfind(',') {
            Some(i) if i > 0 => i,
            _ => head_end,
        }
    };
    TrimmedPrompt { prompt: prompt[..cut].to_string(), original_len, truncated: true }
}

/// `words.join(", ")` trimmed at the last comma within `max_chars` (OpenWhispr trimDictionaryPrompt).
pub fn build_prompt(words: &[String], max_chars: usize) -> String {
    let joined = words
        .iter()
        .map(|w| w.trim())
        .filter(|w| !w.is_empty())
        .collect::<Vec<_>>()
        .join(", ");
    trim_prompt(&joined, max_chars).prompt
}

/// Lower-case, keep letters/digits/whitespace, collapse whitespace.
fn normalize(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars().flat_map(char::to_lowercase) {
        if c.is_alphanumeric() {
            out.push(c);
        } else if c.is_whitespace() && !out.ends_with(' ') && !out.is_empty() {
            out.push(' ');
        }
    }
    if out.ends_with(' ') {
        out.pop();
    }
    out
}

/// Whisper echoed the prompt back instead of transcribing (90% / 70% rule).
pub fn is_prompt_echo(text: &str, prompt: &str) -> bool {
    let (nt, np) = (normalize(text), normalize(prompt));
    if nt.is_empty() || np.is_empty() {
        return false;
    }
    if nt == np {
        return true;
    }
    let dict_words: HashSet<&str> = np.split(' ').collect();
    let text_words: HashSet<&str> = nt.split(' ').collect();
    let matched = text_words.iter().filter(|w| dict_words.contains(*w)).count() as f64;
    matched / text_words.len() as f64 >= 0.9 && matched / dict_words.len() as f64 >= 0.7
}

/// Shape analysis of a possible prompt continuation (OpenWhispr `analyzeDictionaryPromptFragment`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct PromptFragment {
    pub is_prompt_fragment: bool,
    pub has_repeated_words: bool,
    pub unique_word_count: usize,
}

const MAX_SHORT_FRAGMENT_CHARS: usize = 30;
const LOOPED_WORD_MIN_OCCURRENCES: usize = 3;
const MIN_CONSECUTIVE_TERM_RUN: usize = 3;

fn is_delimiter(c: char) -> bool {
    matches!(c, ',' | '、' | '，')
}

fn has_looped_word(words: &[&str]) -> bool {
    let mut counts = std::collections::HashMap::new();
    words.iter().any(|w| {
        let n = counts.entry(*w).or_insert(0usize);
        *n += 1;
        *n >= LOOPED_WORD_MIN_OCCURRENCES
    })
}

/// True when the text is 3+ consecutive prompt entries in the prompt's own order.
fn matches_consecutive_term_run(text_words: &[&str], prompt: &str) -> bool {
    let mut seq: Vec<(String, usize)> = Vec::new();
    let mut term_index = 0;
    for term in prompt.split(is_delimiter) {
        let nt = normalize(term);
        if nt.is_empty() {
            continue;
        }
        seq.extend(nt.split(' ').map(|w| (w.to_string(), term_index)));
        term_index += 1;
    }
    if text_words.is_empty() || seq.len() < text_words.len() {
        return false;
    }
    (0..=seq.len() - text_words.len()).any(|start| {
        text_words.iter().enumerate().all(|(k, w)| seq[start + k].0 == *w)
            && seq[start + text_words.len() - 1].1 - seq[start].1 + 1 >= MIN_CONSECUTIVE_TERM_RUN
    })
}

/// Detects Whisper continuing the dictionary prompt: a looped term, a short fragment left dangling
/// on the prompt's separator ("testing, "), or a run of consecutive entries in prompt order.
pub fn analyze_prompt_fragment(text: &str, prompt: &str) -> PromptFragment {
    if text.is_empty() || prompt.is_empty() {
        return PromptFragment::default();
    }
    let (nt, np) = (normalize(text), normalize(prompt));
    if nt.is_empty() || np.is_empty() {
        return PromptFragment::default();
    }
    let text_words: Vec<&str> = nt.split(' ').collect();
    let prompt_words: HashSet<&str> = np.split(' ').collect();
    let unique: HashSet<&str> = text_words.iter().copied().collect();
    let has_repeated_words = text_words.len() > unique.len();
    let matched = unique.iter().filter(|w| prompt_words.contains(*w)).count() as f64;
    let trailing_delimiter = text.trim_end().chars().next_back().is_some_and(is_delimiter);
    let continues_shape = has_looped_word(&text_words)
        || (trailing_delimiter && nt.chars().count() <= MAX_SHORT_FRAGMENT_CHARS)
        || (text.contains(is_delimiter) && matches_consecutive_term_run(&text_words, prompt));
    PromptFragment {
        is_prompt_fragment: matched / unique.len() as f64 >= 0.9 && continues_shape,
        has_repeated_words,
        unique_word_count: unique.len(),
    }
}

/// Shorthand for `analyze_prompt_fragment(..).is_prompt_fragment`.
pub fn is_prompt_fragment(text: &str, prompt: &str) -> bool {
    analyze_prompt_fragment(text, prompt).is_prompt_fragment
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sn(trigger: &str, replacement: &str) -> Snippet {
        Snippet { trigger: trigger.into(), replacement: replacement.into() }
    }

    fn strs(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    // ---- ported from test/utils/snippets.test.js ----

    #[test]
    fn expands_trigger_with_turkish_capital_i_dot() {
        let s = [sn("İmza", "Best regards,\nUmut")];
        assert_eq!(expand_snippets("İmza", &s), "Best regards,\nUmut");
    }

    #[test]
    fn i_dot_trigger_matches_every_casing() {
        let s = [sn("İmza", "Best regards,\nUmut")];
        for spoken in ["imza", "İmza", "İMZA", "Imza"] {
            assert_eq!(
                expand_snippets(&format!("Meeting is over. {spoken} please."), &s),
                "Meeting is over. Best regards,\nUmut please.",
                "expected {spoken:?} to expand"
            );
        }
    }

    #[test]
    fn lowercase_trigger_matches_capital_i_dot() {
        let s = [sn("imza", "Best regards,\nUmut")];
        assert_eq!(expand_snippets("İmza goes here", &s), "Best regards,\nUmut goes here");
        assert_eq!(expand_snippets("İMZA goes here", &s), "Best regards,\nUmut goes here");
    }

    #[test]
    fn dotless_trigger_matches_capital_i() {
        let s = [sn("ışık", "LIGHT")];
        for spoken in ["ışık", "Işık", "IŞIK"] {
            assert_eq!(expand_snippets(&format!("{spoken} on"), &s), "LIGHT on", "{spoken:?}");
        }
    }

    #[test]
    fn trigger_saved_with_capital_i_matches_both_readings() {
        let s = [sn("Işık", "LIGHT")];
        for spoken in ["ışık", "Işık", "IŞIK", "işık"] {
            assert_eq!(expand_snippets(&format!("{spoken} on"), &s), "LIGHT on", "{spoken:?}");
        }
    }

    #[test]
    fn all_caps_english_trigger_still_matches() {
        let s = [sn("IBAN", "TR00 0000 0000")];
        assert_eq!(expand_snippets("iban please", &s), "TR00 0000 0000 please");
        assert_eq!(expand_snippets("IBAN please", &s), "TR00 0000 0000 please");
        assert_eq!(expand_snippets("İBAN please", &s), "TR00 0000 0000 please");
    }

    #[test]
    fn decomposed_i_dot_still_matches() {
        let s = [sn("İmza", "Best regards,\nUmut")];
        assert_eq!(expand_snippets("I\u{307}mza done", &s), "Best regards,\nUmut done");
    }

    #[test]
    fn i_dot_respects_word_boundaries() {
        let s = [sn("İmza", "SIGNATURE")];
        assert_eq!(expand_snippets("imzalar are ready", &s), "imzalar are ready");
        assert_eq!(expand_snippets("(İmza) is required", &s), "(SIGNATURE) is required");
    }

    #[test]
    fn ascii_triggers_fold_case_both_ways() {
        let s = [sn("Signoff", "Regards")];
        assert_eq!(expand_snippets("SIGNOFF now", &s), "Regards now");
        assert_eq!(expand_snippets("signoff now", &s), "Regards now");
    }

    #[test]
    fn multiple_occurrences_and_unmatched_text_preserved() {
        let s = [sn("İmza", "X")];
        assert_eq!(expand_snippets("İmza and imza, done", &s), "X and X, done");
    }

    #[test]
    fn expand_with_no_snippets_leaves_text_unchanged() {
        assert_eq!(expand_snippets("hello there", &[]), "hello there");
        assert_eq!(expand_snippets("", &[sn("a", "b")]), "");
        assert_eq!(expand_snippets("hello", &[sn("  ", "b")]), "hello");
    }

    #[test]
    fn hint_words_empty_inputs() {
        assert!(hint_words(&[], &[]).is_empty());
    }

    #[test]
    fn hint_words_preserve_dictionary_when_snippets_empty() {
        assert_eq!(hint_words(&strs(&["OpenWhispr", "Supabase"]), &[]), strs(&["OpenWhispr", "Supabase"]));
    }

    #[test]
    fn hint_words_extract_triggers_when_dictionary_empty() {
        let s = [sn("my cal", "cal.com/me"), sn("zoom link", "zoom.us/j/123")];
        assert_eq!(hint_words(&[], &s), strs(&["my cal", "zoom link"]));
    }

    #[test]
    fn hint_words_combine_dictionary_and_triggers() {
        let s = [sn("my cal", "cal.com/me"), sn("zoom link", "zoom.us/j/123")];
        assert_eq!(
            hint_words(&strs(&["OpenWhispr", "Supabase"]), &s),
            strs(&["OpenWhispr", "Supabase", "my cal", "zoom link"])
        );
    }

    #[test]
    fn hint_words_dedupe_case_insensitively_and_skip_empty() {
        let s = [sn("OPENWHISPR", "x"), sn("", "y"), sn("zoom", "z")];
        assert_eq!(hint_words(&strs(&["OpenWhispr", " ", "zoom"]), &s), strs(&["OpenWhispr", "zoom"]));
    }

    #[test]
    fn trigger_ranges_report_where_each_trigger_occurs() {
        let s = [sn("openwhispr review", "Review the PR")];
        assert_eq!(trigger_ranges("openwhispr review this PR", &s), vec![0..17]);
        assert_eq!(trigger_ranges("please openwhispr review it", &s), vec![7..24]);
        assert!(trigger_ranges("nothing to expand here", &s).is_empty());
        assert!(trigger_ranges("openwhispr review", &[]).is_empty());
    }

    #[test]
    fn trigger_ranges_only_report_ranges_expand_would_replace() {
        let s = [sn("ımza", "Saygılarımla")];
        assert_eq!(expand_snippets("imza raporu hazırla", &s), "imza raporu hazırla");
        assert!(trigger_ranges("imza raporu hazırla", &s).is_empty());
    }

    // ---- snippet behaviours added on top of the port ----

    #[test]
    fn longest_trigger_wins() {
        let s = [sn("ask", "A"), sn("investor ask", "B")];
        assert_eq!(expand_snippets("the investor ask is big, ask me", &s), "the B is big, A me");
    }

    #[test]
    fn trailing_sentence_period_does_not_corrupt_replacement() {
        let sig = [sn("my signature", "Best regards,\nUmut")];
        assert_eq!(expand_snippets("Thanks. My signature.", &sig), "Thanks. Best regards,\nUmut");
        let url = [sn("zoom link", "zoom.us/j/123")];
        assert_eq!(expand_snippets("Join via zoom link.", &url), "Join via zoom.us/j/123");
        let mail = [sn("my email", "me@example.com")];
        assert_eq!(expand_snippets("Write to my email.", &mail), "Write to me@example.com");
        assert_eq!(expand_snippets("Write to my email, thanks", &mail), "Write to me@example.com, thanks");
    }

    #[test]
    fn terminal_punctuation_is_not_doubled() {
        let s = [sn("thanks line", "Thanks for your time!"), sn("ending", "Cheers,")];
        assert_eq!(expand_snippets("Okay. Thanks line.", &s), "Okay. Thanks for your time!");
        assert_eq!(expand_snippets("Bye. Ending.", &s), "Bye. Cheers,");
        assert_eq!(expand_snippets("Thanks line, Anna", &s), "Thanks for your time! Anna");
    }

    #[test]
    fn prose_replacement_keeps_sentence_period() {
        let s = [sn("brb", "be right back")];
        assert_eq!(expand_snippets("I will brb.", &s), "I will be right back.");
        assert_eq!(expand_snippets("brb!", &s), "be right back!");
        assert_eq!(expand_snippets("wait... brb...", &s), "wait... be right back...");
    }

    // ---- ported from test/helpers/dictionaryEchoFilter.test.js ----

    const LARGE_PROMPT: &str = "OpenWhispr, Parakeet, Alcahest, Chromium, TypeScript, Electron, testing, data, benchmark, inference, transcription, dictionary, microphone, renderer, latency, pipeline";

    #[test]
    fn echo_detects_verbatim_and_variants() {
        let dict = "OpenWhispr, Parakeet, Alcahest";
        for text in [
            "OpenWhispr, Parakeet, Alcahest",
            "OpenWhispr, Parakeet, Alcahest.",
            "openwhispr, parakeet, alcahest",
            "OpenWhispr Parakeet Alcahest",
            "OpenWhispr,  Parakeet,  Alcahest",
            "OpenWhispr, Parakeet, Alcahest, OpenWhispr, Parakeet, Alcahest",
        ] {
            assert!(is_prompt_echo(text, dict), "{text:?}");
        }
    }

    #[test]
    fn echo_does_not_flag_speech() {
        let dict = "OpenWhispr, Parakeet, Alcahest";
        assert!(!is_prompt_echo("I just installed OpenWhispr and it works great", dict));
        assert!(!is_prompt_echo("OpenWhispr, Parakeet", dict));
        assert!(!is_prompt_echo("The quick brown fox jumps over the lazy dog", dict));
    }

    #[test]
    fn echo_empty_inputs() {
        assert!(!is_prompt_echo("some text", ""));
        assert!(!is_prompt_echo("", "OpenWhispr"));
        assert!(!is_prompt_echo("", ""));
        for (t, p) in [("...", "..."), ("!!!", ",,,"), ("???", "OpenWhispr, Parakeet"), ("OpenWhispr, Parakeet", "!!!")] {
            assert!(!is_prompt_echo(t, p), "{t:?} {p:?}");
        }
    }

    #[test]
    fn echo_single_word_dictionary() {
        assert!(is_prompt_echo("OpenWhispr", "OpenWhispr"));
        assert!(!is_prompt_echo("OpenWhispr is great", "OpenWhispr"));
    }

    #[test]
    fn echo_unicode_and_cjk() {
        assert!(is_prompt_echo("Müller, François, José", "Müller, François, José"));
        assert!(!is_prompt_echo("muller francois jose", "Müller, François, José"));
        assert!(is_prompt_echo("東京, 大阪", "東京, 大阪"));
    }

    #[test]
    fn echo_tolerates_minor_additions() {
        let dict = "Alpha, Bravo, Charlie, Delta, Echo, Foxtrot, Golf, Hotel, India, Juliet";
        assert!(is_prompt_echo("Alpha Bravo Charlie Delta Echo Foxtrot Golf Hotel India Juliet the", dict));
    }

    #[test]
    fn fragment_detects_short_repeats_and_dangling_separator() {
        assert!(is_prompt_fragment("data, data, data, data,", LARGE_PROMPT));
        assert!(is_prompt_fragment("testing,", LARGE_PROMPT));
        assert!(is_prompt_fragment("testing, ", LARGE_PROMPT));
        assert!(is_prompt_fragment("OpenWhispr, OpenWhispr, OpenWhispr", LARGE_PROMPT));
    }

    #[test]
    fn fragment_ignores_normal_speech() {
        assert!(!is_prompt_fragment("OpenWhispr is working", LARGE_PROMPT));
        assert!(!is_prompt_fragment("hello there", LARGE_PROMPT));
        assert!(!is_prompt_fragment("", LARGE_PROMPT));
        assert!(!is_prompt_fragment("...", LARGE_PROMPT));
        assert!(!is_prompt_fragment("testing", ""));
        assert!(!is_prompt_fragment("OpenWhispr Parakeet Alcahest Chromium", LARGE_PROMPT));
        assert!(!is_prompt_fragment("testing", LARGE_PROMPT));
        assert!(!is_prompt_fragment("Electron renderer latency", LARGE_PROMPT));
        assert!(!is_prompt_fragment("Electron, renderer", LARGE_PROMPT));
        assert!(!is_prompt_fragment("testing, data", LARGE_PROMPT));
        assert!(!is_prompt_fragment("OpenWhispr, OpenWhispr", LARGE_PROMPT));
        assert!(!is_prompt_fragment("yes, no, maybe, dunno,", LARGE_PROMPT));
    }

    #[test]
    fn fragment_ignores_snippet_trigger_words() {
        let with_triggers = format!("{LARGE_PROMPT}, on my way, let me know");
        assert!(!is_prompt_fragment("On my way.", &with_triggers));
        assert!(!is_prompt_fragment("Let me know", &with_triggers));
    }

    #[test]
    fn fragment_detects_consecutive_term_runs() {
        assert!(is_prompt_fragment("TypeScript, Electron, testing, data, benchmark", LARGE_PROMPT));
        assert!(!is_prompt_fragment("benchmark, TypeScript, data, Electron, testing", LARGE_PROMPT));
    }

    // ---- ported from test/helpers/audioManagerDictionaryPromptCap.test.js ----

    fn long_prompt(count: usize) -> String {
        (0..count).map(|i| format!("SpecializedTerm{i:03}")).collect::<Vec<_>>().join(", ")
    }

    #[test]
    fn prompt_caps_keep_head_and_end_on_whole_entries() {
        let full = long_prompt(100);
        assert!(full.len() > 1600);
        for cap in [GROQ_PROMPT_CHARS, WHISPER_PROMPT_CHARS] {
            let t = trim_prompt(&full, cap);
            assert!(t.truncated);
            assert_eq!(t.original_len, full.len());
            assert!(t.prompt.len() <= cap, "got {}", t.prompt.len());
            assert!(full.starts_with(&t.prompt), "truncation must keep the head of the list");
            assert_eq!(&full[t.prompt.len()..t.prompt.len() + 1], ",", "must end on a whole entry");
        }
    }

    #[test]
    fn prompt_guard_for_absurd_lists_is_generous() {
        let huge = long_prompt(700);
        assert!(huge.len() > 12000);
        let t = trim_prompt(&huge, TRANSCRIBE_PROMPT_CHARS);
        assert!(t.prompt.len() <= 8000 && t.prompt.len() > 7000);
        assert!(huge.starts_with(&t.prompt));
    }

    #[test]
    fn prompt_within_budget_is_untouched() {
        let full = long_prompt(10);
        let t = trim_prompt(&full, WHISPER_PROMPT_CHARS);
        assert_eq!((t.prompt.as_str(), t.truncated), (full.as_str(), false));
        assert_eq!(trim_prompt("", 10), TrimmedPrompt { prompt: String::new(), original_len: 0, truncated: false });
    }

    #[test]
    fn prompt_cut_without_comma_falls_back_to_hard_cut() {
        assert_eq!(trim_prompt("abcdefghij", 4).prompt, "abcd");
    }

    #[test]
    fn prompt_cut_exactly_on_entry_boundary_keeps_the_entry() {
        assert_eq!(trim_prompt("aaa, bbb, ccc", 8).prompt, "aaa, bbb");
        assert_eq!(trim_prompt("aaa, bbb, ccc", 9).prompt, "aaa, bbb");
    }

    #[test]
    fn prompt_trim_counts_chars_not_bytes() {
        let p = "日本語, 東京都, 大阪府";
        assert_eq!(trim_prompt(p, 11).prompt, "日本語, 東京都");
        assert_eq!(build_prompt(&strs(&["日本語", "東京都", "大阪府"]), 11), "日本語, 東京都");
    }

    #[test]
    fn build_prompt_joins_and_trims() {
        let w = strs(&["OpenWhispr", " ", "Parakeet", "Alcahest"]);
        assert_eq!(build_prompt(&w, 900), "OpenWhispr, Parakeet, Alcahest");
        assert_eq!(build_prompt(&w, 25), "OpenWhispr, Parakeet");
        assert_eq!(build_prompt(&[], 25), "");
    }
}
