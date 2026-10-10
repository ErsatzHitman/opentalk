//! Pure dictionary and snippet logic shared by the tray pages and the cleanup pipeline: word
//! parsing, add/filter, the STT prompt budget and its warning, import/export formats, snippet
//! merging. No Win32; everything is unit-tested below.
use super::config::{Config, Snippet, SttProviderId};

/// Split free text on commas, semicolons and newlines; trims, drops empties and duplicates
/// (case-insensitive, first spelling wins).
pub fn parse_words(input: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for part in input.split([',', ';', '\n', '\r']) {
        let t = part.trim();
        if t.is_empty() {
            continue;
        }
        if seen.insert(t.to_lowercase()) {
            out.push(t.to_string());
        }
    }
    out
}

/// Add `new` to `words`, skipping case-insensitive duplicates. Returns how many were added.
pub fn add_words(words: &mut Vec<String>, new: &[String]) -> usize {
    // One lowercase set instead of a pairwise scan: a bulk import of thousands of words would
    // otherwise allocate O(n^2) lowercase strings on the UI thread.
    let mut seen: std::collections::HashSet<String> = words.iter().map(|o| o.to_lowercase()).collect();
    let mut added = 0;
    for n in new {
        if seen.insert(n.to_lowercase()) {
            words.push(n.clone());
            added += 1;
        }
    }
    added
}

/// Indices of words containing `query` (case-insensitive); everything for an empty query.
pub fn filter_words(words: &[String], query: &str) -> Vec<usize> {
    let q = query.trim().to_lowercase();
    words.iter().enumerate().filter(|(_, w)| q.is_empty() || w.to_lowercase().contains(&q)).map(|(i, _)| i).collect()
}

/// Characters of the prompt string `words.join(", ")`.
pub fn prompt_chars(words: &[String]) -> usize {
    if words.is_empty() { 0 } else { words.iter().map(|w| w.chars().count()).sum::<usize>() + (words.len() - 1) * 2 }
}

/// Dictionary words + snippet triggers, deduplicated (what the provider actually receives).
pub fn hint_words(dictionary: &[String], snippets: &[Snippet]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    add_words(&mut out, dictionary);
    let triggers: Vec<String> = snippets.iter().map(|s| s.trigger.trim().to_string()).filter(|t| !t.is_empty()).collect();
    add_words(&mut out, &triggers);
    out
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Budget {
    pub provider: &'static str,
    pub max_chars: Option<usize>,
    pub max_terms: Option<usize>,
    /// Provider ignores the dictionary entirely.
    pub unused: bool,
}

/// Prompt budget of the active STT provider/model (docs/PLAN.md 4a, OpenWhispr dictionaryPromptCap).
pub fn budget(cfg: &Config) -> Budget {
    let info = crate::stt::registry::provider(cfg.stt.provider);
    let model = cfg.stt_model();
    let (max_chars, max_terms, unused) = match cfg.stt.provider {
        SttProviderId::Openai if model.starts_with("whisper") => (Some(900), None, false),
        SttProviderId::Openai => (Some(8000), None, false),
        SttProviderId::Groq => (Some(890), None, false),
        SttProviderId::Xai | SttProviderId::Mistral | SttProviderId::Deepgram | SttProviderId::Assemblyai => (None, Some(100), false),
        SttProviderId::Gemini | SttProviderId::Openrouter => (Some(8000), None, false),
        SttProviderId::Tinfoil | SttProviderId::Custom => (Some(900), None, false),
        SttProviderId::Corti => (None, None, true),
    };
    Budget { provider: info.name, max_chars, max_terms, unused }
}

/// How many of `words` (in order) fit the budget.
pub fn fitting_count(words: &[String], b: &Budget) -> usize {
    if b.unused {
        return 0;
    }
    let mut n = words.len();
    if let Some(t) = b.max_terms {
        n = n.min(t);
    }
    if let Some(c) = b.max_chars {
        let mut used = 0usize;
        let mut fit = 0usize;
        for w in &words[..n] {
            let add = w.chars().count() + if fit == 0 { 0 } else { 2 };
            if used + add > c {
                break;
            }
            used += add;
            fit += 1;
        }
        n = fit;
    }
    n
}

/// Warning shown under the list when the active provider cannot take every word.
pub fn budget_warning(cfg: &Config) -> Option<String> {
    let b = budget(cfg);
    let words = hint_words(&cfg.dictionary, &cfg.snippets);
    if words.is_empty() {
        return None;
    }
    if b.unused {
        return Some(format!("{} does not use a custom dictionary, so these words only help the local cleanup step.", b.provider));
    }
    let fit = fitting_count(&words, &b);
    if fit < words.len() {
        let limit = match (b.max_chars, b.max_terms) {
            (Some(c), _) => format!("{c} characters"),
            (None, Some(t)) => format!("{t} terms"),
            _ => "its limit".into(),
        };
        Some(format!("{} accepts about {limit} of hints, so only the first {fit} of {} words are sent. Remove rarely used words to stay under the limit.", b.provider, words.len()))
    } else {
        None
    }
}

/// Parse an imported file: JSON (`["a","b"]`, `{"dictionary":[..],"snippets":[{trigger,replacement}]}`,
/// or a bare snippet array) or plain text split on commas/newlines.
pub fn parse_import(text: &str) -> (Vec<String>, Vec<Snippet>) {
    let t = text.trim_start_matches('\u{feff}').trim();
    if (t.starts_with('[') || t.starts_with('{'))
        && let Ok(v) = serde_json::from_str::<serde_json::Value>(t) {
            let mut words = Vec::new();
            let mut snippets = Vec::new();
            collect_json(&v, &mut words, &mut snippets);
            return (words, snippets);
        }
    (parse_words(t), Vec::new())
}

fn collect_json(v: &serde_json::Value, words: &mut Vec<String>, snippets: &mut Vec<Snippet>) {
    match v {
        serde_json::Value::Array(a) => {
            for x in a {
                collect_json(x, words, snippets);
            }
        }
        serde_json::Value::String(s) => {
            let mut parsed = parse_words(s);
            words.append(&mut parsed);
        }
        serde_json::Value::Object(m) => {
            if let (Some(t), Some(r)) = (m.get("trigger").and_then(|x| x.as_str()), m.get("replacement").and_then(|x| x.as_str())) {
                snippets.push(Snippet { trigger: t.to_string(), replacement: r.to_string() });
            }
            for key in ["dictionary", "words", "snippets"] {
                if let Some(x) = m.get(key) {
                    collect_json(x, words, snippets);
                }
            }
        }
        _ => {}
    }
}

pub fn export_words_json(words: &[String]) -> String {
    serde_json::to_string_pretty(&serde_json::json!({ "dictionary": words })).unwrap_or_default()
}

pub fn export_words_txt(words: &[String]) -> String {
    words.join("\n")
}

pub fn export_snippets_json(snippets: &[Snippet]) -> String {
    serde_json::to_string_pretty(&serde_json::json!({ "snippets": snippets })).unwrap_or_default()
}

/// Largest dictionary/snippet file we import (a real word list is a few KB).
pub const MAX_IMPORT_BYTES: u64 = 4 * 1024 * 1024;

pub fn read_import_file(path: &str) -> std::io::Result<String> {
    let len = std::fs::metadata(path)?.len();
    if len > MAX_IMPORT_BYTES {
        return Err(std::io::Error::other("the file is larger than 4 MB"));
    }
    std::fs::read_to_string(path)
}

/// Append imported snippets, trimming triggers and skipping blank ones and triggers already in
/// use (case-insensitive, as the editor enforces). Returns how many were added.
pub fn merge_snippets(list: &mut Vec<Snippet>, new: Vec<Snippet>) -> usize {
    let mut added = 0;
    for sn in new {
        let trigger = sn.trigger.trim().to_string();
        if trigger.is_empty() || sn.replacement.trim().is_empty() || trigger_taken(list, usize::MAX, &trigger) {
            continue;
        }
        list.push(Snippet { trigger, replacement: sn.replacement });
        added += 1;
    }
    added
}


/// True when another snippet (not `skip`) already uses `trigger` (case-insensitive).
pub fn trigger_taken(snippets: &[Snippet], skip: usize, trigger: &str) -> bool {
    let t = trigger.trim().to_lowercase();
    !t.is_empty() && snippets.iter().enumerate().any(|(i, s)| i != skip && s.trigger.trim().to_lowercase() == t)
}

pub fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max { s.to_string() } else { format!("{}...", s.chars().take(max - 1).collect::<String>()) }
}


#[cfg(test)]
mod tests {
    use super::*;

    fn sn(t: &str, r: &str) -> Snippet {
        Snippet { trigger: t.into(), replacement: r.into() }
    }

    #[test]
    fn parse_words_splits_trims_and_dedupes() {
        assert_eq!(parse_words(" OpenAI, Priya ;kubectl\n\nopenai , "), vec!["OpenAI", "Priya", "kubectl"]);
        assert!(parse_words(" , ;\n").is_empty());
    }

    #[test]
    fn merge_snippets_trims_and_skips_blank_or_taken_triggers() {
        let mut list = vec![Snippet { trigger: "My Email".into(), replacement: "a@b.c".into() }];
        let new = vec![
            Snippet { trigger: "  sig  ".into(), replacement: "Best, A".into() },
            Snippet { trigger: "   ".into(), replacement: "x".into() },
            Snippet { trigger: "blank".into(), replacement: "  ".into() },
            Snippet { trigger: "my email".into(), replacement: "dup".into() },
            Snippet { trigger: "SIG".into(), replacement: "dup within import".into() },
        ];
        assert_eq!(merge_snippets(&mut list, new), 1);
        assert_eq!(list.len(), 2);
        assert_eq!(list[1].trigger, "sig");
    }

    #[test]
    fn add_words_skips_duplicates_case_insensitively() {
        let mut w = vec!["OpenAI".to_string()];
        assert_eq!(add_words(&mut w, &["openai".into(), "Groq".into(), "groq".into()]), 1);
        assert_eq!(w, vec!["OpenAI", "Groq"]);
    }

    #[test]
    fn filter_is_case_insensitive_substring() {
        let w: Vec<String> = ["OpenWhispr", "Priya", "whisper"].iter().map(|s| s.to_string()).collect();
        assert_eq!(filter_words(&w, "WHISP"), vec![0, 2]);
        assert_eq!(filter_words(&w, ""), vec![0, 1, 2]);
        assert!(filter_words(&w, "zzz").is_empty());
    }

    #[test]
    fn prompt_chars_counts_separators() {
        assert_eq!(prompt_chars(&[]), 0);
        assert_eq!(prompt_chars(&["ab".into()]), 2);
        assert_eq!(prompt_chars(&["ab".into(), "cd".into(), "e".into()]), 2 + 2 + 2 + 2 + 1);
    }

    #[test]
    fn hints_merge_snippet_triggers() {
        let h = hint_words(&["Priya".into()], &[sn("my email", "x@y.z"), sn("priya", "dup"), sn("  ", "blank")]);
        assert_eq!(h, vec!["Priya", "my email"]);
    }

    #[test]
    fn budget_by_provider() {
        let mut cfg = Config::default();
        cfg.stt.provider = SttProviderId::Groq;
        assert_eq!(budget(&cfg).max_chars, Some(890));
        cfg.stt.provider = SttProviderId::Openai;
        cfg.stt.models.insert(SttProviderId::Openai, "whisper-1".into());
        assert_eq!(budget(&cfg).max_chars, Some(900));
        cfg.stt.models.insert(SttProviderId::Openai, "gpt-4o-transcribe".into());
        assert_eq!(budget(&cfg).max_chars, Some(8000));
        cfg.stt.provider = SttProviderId::Xai;
        assert_eq!(budget(&cfg).max_terms, Some(100));
        cfg.stt.provider = SttProviderId::Corti;
        assert!(budget(&cfg).unused);
    }

    #[test]
    fn fitting_respects_chars_and_terms() {
        let words: Vec<String> = (0..5).map(|_| "abcd".to_string()).collect(); // 4 each, 2 sep
        let b = Budget { provider: "t", max_chars: Some(14), max_terms: None, unused: false };
        assert_eq!(fitting_count(&words, &b), 2, "4 + 2 + 4 = 10, a third word would need 16");
        let b = Budget { provider: "t", max_chars: None, max_terms: Some(3), unused: false };
        assert_eq!(fitting_count(&words, &b), 3);
        let b = Budget { provider: "t", max_chars: None, max_terms: None, unused: true };
        assert_eq!(fitting_count(&words, &b), 0);
    }

    #[test]
    fn warning_only_when_words_are_dropped() {
        let mut cfg = Config::default();
        cfg.stt.provider = SttProviderId::Groq;
        cfg.dictionary = vec!["Priya".into()];
        assert!(budget_warning(&cfg).is_none());
        cfg.dictionary = (0..400).map(|i| format!("term{i:03}")).collect();
        let msg = budget_warning(&cfg).expect("over the 890 character budget");
        assert!(msg.contains("Groq") && msg.contains("890"), "{msg}");
        cfg.stt.provider = SttProviderId::Corti;
        assert!(budget_warning(&cfg).unwrap().contains("does not use"));
        cfg.dictionary.clear();
        assert!(budget_warning(&cfg).is_none());
    }

    #[test]
    fn import_formats() {
        assert_eq!(parse_import("a, b\nc"), (vec!["a".into(), "b".into(), "c".into()], vec![]));
        assert_eq!(parse_import(r#"["x","y, z"]"#).0, vec!["x", "y", "z"]);
        let (w, s) = parse_import(r#"{"dictionary":["Priya"],"snippets":[{"trigger":"sig","replacement":"Best, A"}]}"#);
        assert_eq!(w, vec!["Priya"]);
        assert_eq!(s, vec![sn("sig", "Best, A")]);
        let (w, s) = parse_import("\u{feff}[{\"trigger\":\"t\",\"replacement\":\"r\"}]");
        assert!(w.is_empty());
        assert_eq!(s, vec![sn("t", "r")]);
        // Broken JSON falls back to plain text.
        assert_eq!(parse_import("[oops").0, vec!["[oops"]);
    }

    #[test]
    fn export_roundtrips() {
        let words = vec!["A".to_string(), "B c".to_string()];
        assert_eq!(parse_import(&export_words_json(&words)).0, words);
        assert_eq!(parse_import(&export_words_txt(&words)).0, words);
        let snips = vec![sn("t", "multi\nline")];
        assert_eq!(parse_import(&export_snippets_json(&snips)).1, snips);
    }

    #[test]
    fn edited_trigger_may_not_clash_with_another_snippet() {
        let list = vec![sn("my email", "a"), sn("sig", "b")];
        assert!(trigger_taken(&list, 1, " My Email "));
        assert!(!trigger_taken(&list, 0, "my email"), "a snippet may keep its own trigger");
        assert!(!trigger_taken(&list, 0, "   "));
    }

    #[test]
    fn display_helpers() {
        assert_eq!(one_line("a\n  b\tc"), "a b c");
        assert_eq!(truncate("abcdef", 4), "abc...");
        assert_eq!(truncate("abc", 4), "abc");
    }
}
