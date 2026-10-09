//! Model-output hygiene: `<think>` stripping (port of OpenWhispr `stripThinking.js`), preamble /
//! fence / quote stripping, the duplicate-halves rule (port of `src/utils/cleanupOutput.ts`) and
//! the length-ratio / empty-output checks.
use crate::llm::LlmError;
use std::collections::HashMap;

const THINK_OPEN: &str = "<think>";
const THINK_CLOSE: &str = "</think>";

/// Remove `<think>…</think>` blocks (innermost first, so nesting cannot leave a stray closer),
/// then an unterminated trailing `<think>…`, then orphan `</think>` tags; trims the result.
pub fn strip_thinking(text: &str) -> String {
    let low = text.to_ascii_lowercase();
    if !low.contains("think>") {
        return text.trim().to_string();
    }
    let mut out = text.to_string();
    // ASCII lowercase keeps byte offsets, so positions found in the lowercase copy index `out`.
    while let Some((s, e)) = innermost_closed(&out.to_ascii_lowercase()) {
        out.replace_range(s..e, "");
    }
    if let Some(i) = out.to_ascii_lowercase().find(THINK_OPEN) {
        out.truncate(i);
    }
    let low = out.to_ascii_lowercase();
    let mut cleaned = String::with_capacity(out.len());
    let mut last = 0;
    for (i, _) in low.match_indices(THINK_CLOSE) {
        cleaned.push_str(&out[last..i]);
        last = i + THINK_CLOSE.len();
    }
    cleaned.push_str(&out[last..]);
    cleaned.trim().to_string()
}

/// Leftmost `<think>` whose first following `</think>` has no other `<think>` in between.
fn innermost_closed(low: &str) -> Option<(usize, usize)> {
    for (o, _) in low.match_indices(THINK_OPEN) {
        let body = o + THINK_OPEN.len();
        let c = body + low[body..].find(THINK_CLOSE)?;
        if !low[body..c].contains(THINK_OPEN) {
            return Some((o, c + THINK_CLOSE.len()));
        }
    }
    None
}

// ---------------------------------------------------------------------------------------------
// Sanitizing (preamble / fences / quotes)
// ---------------------------------------------------------------------------------------------

/// Strip thinking blocks, an echoed `<transcript>` wrapper, code fences, leading "Here is the
/// cleaned transcript:" style labels and wrapping quotes. `raw` is the input that was sent; text
/// that the speaker actually dictated (a quote, a label) is never stripped.
pub fn sanitize(output: &str, raw: &str) -> String {
    let raw_trim = raw.trim();
    let raw_low = raw_trim.to_lowercase();
    let mut s = strip_thinking(output);
    for _ in 0..4 {
        let before = s.clone();
        s = strip_fence(&s);
        s = strip_transcript_tags(&s);
        s = strip_preamble(&s, &raw_low);
        s = strip_wrapping_quotes(&s, raw_trim);
        if s == before {
            break;
        }
    }
    s
}

fn strip_fence(s: &str) -> String {
    let t = s.trim();
    if t.len() > 6 && t.starts_with("```") && t.ends_with("```") {
        let inner = &t[3..t.len() - 3];
        // First line after the opening fence is an optional language tag.
        let body = match inner.split_once('\n') {
            Some((tag, rest)) if !tag.contains(' ') => rest,
            _ => inner,
        };
        return body.trim().to_string();
    }
    t.to_string()
}

fn strip_transcript_tags(s: &str) -> String {
    let mut t = s.trim();
    if let Some(r) = t.strip_prefix("<transcript>") {
        t = r;
    }
    if let Some(r) = t.strip_suffix("</transcript>") {
        t = r;
    }
    t.trim().to_string()
}

const LABEL_HEADS: &[&str] = &[
    "here is", "here's", "here are", "heres", "cleaned", "clean transcript", "clean-up", "cleanup",
    "output", "sure", "certainly", "of course", "corrected", "transcript", "final", "result",
    "revised", "edited", "the cleaned", "the corrected",
];
const LABEL_WORDS: &[&str] = &["transcript", "text", "version", "output", "result", "cleaned"];

/// Lowercase `line` without markdown emphasis / heading / quote markers around it.
fn plain_label(line: &str) -> String {
    line.trim()
        .trim_matches(|c: char| matches!(c, '*' | '_' | '#' | '>' | '`') || c.is_whitespace())
        .to_lowercase()
}

/// Is `head` (text before a colon, lowercased, markup stripped) a model-added label?
fn is_label_head(head: &str) -> bool {
    let words = head.split_whitespace().count();
    if words == 0 || words > 10 {
        return false;
    }
    if !LABEL_HEADS.iter().any(|h| head.starts_with(h)) {
        return false;
    }
    head == "output" || LABEL_WORDS.iter().any(|w| head.contains(w))
}

/// Drop a leading interjection ("Sure! ") from an already-lowercased label line.
fn without_interjection(plain: String) -> String {
    for p in ["sure!", "sure,", "certainly!", "of course!"] {
        if let Some(r) = plain.strip_prefix(p) {
            return r.trim().to_string();
        }
    }
    plain
}

/// Did the speaker dictate `label` followed by a colon (typed, or spoken as "colon")? Then it is
/// content ("output colon done" -> "Output: done"), not a label the model added.
fn speaker_said_label(raw_low: &str, label: &str) -> bool {
    raw_low.contains(&format!("{label}:")) || raw_low.contains(&format!("{label} colon"))
}

fn strip_preamble(s: &str, raw_low: &str) -> String {
    let mut cur = s.trim().to_string();
    for _ in 0..3 {
        let (first, rest) = match cur.split_once('\n') {
            Some((f, r)) => (f.to_string(), Some(r.to_string())),
            None => (cur.clone(), None),
        };
        let first_plain = without_interjection(plain_label(&first));
        if let Some(head) = first_plain.strip_suffix(':') {
            // Whole line is a label.
            if is_label_head(head.trim()) && !speaker_said_label(raw_low, head.trim()) {
                match rest {
                    Some(r) => {
                        cur = r.trim().to_string();
                        continue;
                    }
                    None => return String::new(),
                }
            }
        } else if let Some((head, tail)) = first.split_once(':') {
            // "Cleaned transcript: Hello there." -> keep the text after the label.
            let head_plain = without_interjection(plain_label(head));
            if is_label_head(&head_plain) && !tail.trim().is_empty() && !speaker_said_label(raw_low, &head_plain) {
                let tail = tail.trim_start_matches(|c: char| c == '*' || c.is_whitespace());
                cur = match rest {
                    Some(r) => format!("{tail}\n{r}"),
                    None => tail.to_string(),
                }
                .trim()
                .to_string();
                continue;
            }
        }
        break;
    }
    cur
}

fn strip_wrapping_quotes(s: &str, raw_trim: &str) -> String {
    let t = s.trim();
    let n = t.chars().count();
    let (Some(first), Some(last)) = (t.chars().next(), t.chars().next_back()) else { return t.to_string() };
    if n < 2 {
        return t.to_string();
    }
    let closer = match first {
        '"' => '"',
        '\u{201C}' => '\u{201D}',
        '\u{00AB}' => '\u{00BB}',
        '\u{201E}' => '\u{201C}',
        _ => return t.to_string(),
    };
    if last != closer || raw_trim.starts_with(first) {
        return t.to_string();
    }
    let inner: String = t.chars().skip(1).take(n - 2).collect();
    // Only a wrapper: the inside must not contain more quote marks of the same kind.
    if inner.contains(first) || inner.contains(closer) {
        return t.to_string();
    }
    inner.trim().to_string()
}

// ---------------------------------------------------------------------------------------------
// Tokenizing (stand-in for Intl.Segmenter word granularity)
// ---------------------------------------------------------------------------------------------

fn is_cjk(c: char) -> bool {
    matches!(c as u32,
        0x3040..=0x30FF | 0x31F0..=0x31FF | 0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xF900..=0xFAFF | 0x20000..=0x2FFFF)
}

/// Combining marks / joiners that continue a word (cheap stand-in for \p{M}).
fn is_mark(c: char) -> bool {
    matches!(c as u32,
        0x0300..=0x036F | 0x0483..=0x0489 | 0x0591..=0x05C7 | 0x064B..=0x065F | 0x0670 | 0x0900..=0x0DFF
        | 0x0E31 | 0x0E34..=0x0E3A | 0x0E47..=0x0E4E | 0x200C | 0x200D | 0x20D0..=0x20FF | 0xFE00..=0xFE0F)
}

fn fold(c: char) -> char {
    // Minimal NFKC: fullwidth ASCII variants and the ideographic space.
    match c as u32 {
        0xFF01..=0xFF5E => char::from_u32(c as u32 - 0xFEE0).unwrap_or(c),
        0x3000 => ' ',
        _ => c,
    }
}

/// Lowercased word-like tokens: NFKC-ish fold, apostrophes dropped (contractions stay one word),
/// punctuation and symbols separate words, CJK ideographs / kana count one token each.
pub fn comparison_tokens(text: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut cur = String::new();
    for c in text.chars().map(fold) {
        if matches!(c, '\'' | '\u{2019}' | '\u{2018}' | '`' | '\u{02BC}') {
            continue;
        }
        if is_cjk(c) {
            if !cur.is_empty() {
                tokens.push(std::mem::take(&mut cur));
            }
            tokens.push(c.to_string());
        } else if c.is_alphanumeric() || (is_mark(c) && !cur.is_empty()) {
            cur.extend(c.to_lowercase());
        } else if !cur.is_empty() {
            tokens.push(std::mem::take(&mut cur));
        }
    }
    if !cur.is_empty() {
        tokens.push(cur);
    }
    tokens
}

fn is_cleanup_label(line: &str) -> bool {
    let t = line.trim();
    t.eq_ignore_ascii_case("Cleaned transcript:")
        || t.eq_ignore_ascii_case("**Cleaned transcript:**")
        || t.eq_ignore_ascii_case("**Cleaned transcript**:")
}

/// Port of `assertValidCleanupOutput`: `true` when `output` is the transcript said twice by the
/// model rather than by the speaker (a trailing copy of the first half that the raw speech does
/// not account for). Needs at least six word tokens per half.
pub fn is_duplicated_transcript(raw: &str, output: &str) -> bool {
    // Collapse stutters ("the the") so they don't read as saying a word twice.
    let mut original = comparison_tokens(raw);
    original.dedup();
    let mut tokens: Vec<String> = Vec::new();
    for line in output.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if is_cleanup_label(line) {
            continue;
        }
        tokens.extend(comparison_tokens(line));
    }
    if tokens.len() % 2 != 0 {
        return false;
    }
    let half = tokens.len() / 2;
    if half < 6 || tokens[..half] != tokens[half..] {
        return false;
    }
    // A speaker who really said it twice said most of its words twice: each word in the copy uses
    // up two of its occurrences in the raw transcript.
    let mut raw_counts: HashMap<&str, usize> = HashMap::new();
    for t in &original {
        *raw_counts.entry(t.as_str()).or_insert(0) += 1;
    }
    let mut said_twice = 0usize;
    for t in &tokens[..half] {
        let remaining = raw_counts.get(t.as_str()).copied().unwrap_or(0);
        if remaining < 2 {
            continue;
        }
        raw_counts.insert(t.as_str(), remaining - 2);
        said_twice += 1;
    }
    said_twice * 2 <= half
}

const FILLER_WORDS: &[&str] = &[
    "um", "umm", "uh", "uhh", "uhm", "er", "erm", "ah", "eh", "hm", "hmm", "mm", "mmm", "like", "you", "know",
];

fn non_filler_words(tokens: &[String]) -> usize {
    tokens.iter().filter(|t| !FILLER_WORDS.contains(&t.as_str())).count()
}

/// Minimum input size for the output/input word-ratio check.
const RATIO_MIN_INPUT_WORDS: usize = 8;
const RATIO_MIN: f64 = 0.3;
const RATIO_MAX: f64 = 3.0;

/// Full validation of model output against the transcript that was sent. Returns the sanitized
/// text, or `InvalidOutput` for: duplicated transcript, output/input word ratio outside 0.3..3.0
/// (inputs of 8+ words) and empty output for input with 3+ non-filler words.
pub fn validate(raw: &str, output: &str) -> Result<String, LlmError> {
    let cleaned = sanitize(output, raw);
    let raw_tokens = comparison_tokens(raw);
    if cleaned.trim().is_empty() {
        if non_filler_words(&raw_tokens) >= 3 {
            return Err(LlmError::InvalidOutput("empty output for non-empty input".into()));
        }
        return Ok(String::new());
    }
    if is_duplicated_transcript(raw, &cleaned) {
        return Err(LlmError::InvalidOutput("AI cleanup repeated the transcript".into()));
    }
    if raw_tokens.len() >= RATIO_MIN_INPUT_WORDS {
        let ratio = comparison_tokens(&cleaned).len() as f64 / raw_tokens.len() as f64;
        if !(RATIO_MIN..=RATIO_MAX).contains(&ratio) {
            return Err(LlmError::InvalidOutput(format!(
                "output/input word ratio {ratio:.2} outside {RATIO_MIN}..{RATIO_MAX}"
            )));
        }
    }
    Ok(cleaned)
}

#[cfg(test)]
mod tests {
    use super::*;

    const RAW: &str = "um so can you uh send me the report by friday";
    const CLEAN: &str = "Can you send me the report by Friday?";
    const LONG_RAW: &str = "the team wants to ship the release next week but the team also wants the tests to pass first so the release might slip to the week after";
    const LONG_CLEAN: &str = "The team wants to ship the release next week, but the team also wants the tests to pass first, so the release might slip to the week after.";

    // --- ported from OpenWhispr test/utils/cleanupOutput.test.js ---

    #[test]
    fn rejects_whole_output_duplication() {
        let outputs = [
            format!("{CLEAN} {CLEAN}"),
            format!("Cleaned transcript:\n{CLEAN}\n\n{CLEAN}"),
            format!("**Cleaned transcript:**\n{CLEAN}\n**Cleaned transcript:**\n{CLEAN}"),
            format!("**Cleaned transcript**:\n{CLEAN}\n{CLEAN}"),
            format!("Can you send me\nCleaned transcript:\nthe report by Friday? {CLEAN}"),
            format!("{CLEAN}\n{CLEAN}\nCleaned transcript:"),
            "CAN YOU SEND ME THE REPORT BY FRIDAY!\ncan you send me the report by Friday?".to_string(),
            "Ｃａｎ you send me the report by Friday?\nCan you send me the report by Friday?".to_string(),
            "Please send the updated report tomorrow. Please send the updated report tomorrow.".to_string(),
            "请你明天把修改后的报告发送给项目负责人。请你明天把修改后的报告发送给项目负责人。".to_string(),
            "Envoyez le rapport complet à Marie demain. Envoyez le rapport complet à Marie demain.".to_string(),
        ];
        for o in &outputs {
            assert!(is_duplicated_transcript(RAW, o), "should reject: {o}");
        }
        // Filler-heavy / stuttered speech is not the same as saying the cleanup twice.
        let cases = [
            ("um so uh basically can you uh like send me the report by friday um yeah", CLEAN),
            (LONG_RAW, LONG_CLEAN),
            (
                "I I I think we we should uh we should move the the meeting to to friday because because the the client is is out on on thursday",
                "I think we should move the meeting to Friday because the client is out on Thursday.",
            ),
        ];
        for (raw, out) in cases {
            assert!(is_duplicated_transcript(raw, &format!("{out} {out}")), "should reject: {out}");
        }
    }

    #[test]
    fn leaves_legitimate_ambiguous_and_out_of_scope_output_alone() {
        let long_twice = format!("{LONG_RAW} {LONG_RAW}");
        let long_clean_twice = format!("{LONG_CLEAN} {LONG_CLEAN}");
        let clean_twice_raw = format!("{CLEAN} {CLEAN}");
        let clean_upper = format!("{}\n{CLEAN}", CLEAN.to_uppercase());
        let cases: Vec<(String, String)> = vec![
            (RAW.into(), CLEAN.into()),
            (RAW.into(), "Please send the report tomorrow. Please send the report tomorrow.".into()),
            (RAW.into(), "I'm sorry, I can't. I'm sorry, I can't.".into()),
            (RAW.into(), format!("{CLEAN} Can you send me the report by Monday?")),
            (RAW.into(), format!("Introduction. {CLEAN} {CLEAN}")),
            (RAW.into(), format!("{CLEAN} {CLEAN} Additional details.")),
            (RAW.into(), format!("**Cleaned transcript:**\n{CLEAN}")),
            (RAW.into(), format!("**{CLEAN}**")),
            (RAW.into(), "What's the capital of France?".into()),
            (RAW.into(), "".into()),
            (RAW.into(), "  \n ".into()),
            (clean_twice_raw, clean_upper),
            ("Send it Thursday no wait Friday".into(), "Send it Friday.".into()),
            ("déjà vu élève".into(), "Déjà vu, élève.".into()),
            (
                "um please send the report by friday please send the report by friday".into(),
                "Please send the report by Friday. Please send the report by Friday.".into(),
            ),
            (
                "im gonna send the report to marie tomorrow im gonna send the report to marie tomorrow".into(),
                "I'm going to send the report to Marie tomorrow. I'm going to send the report to Marie tomorrow.".into(),
            ),
            (long_twice, long_clean_twice),
            (
                "were gonna ship it on friday for sure um were gonna ship it on friday for sure".into(),
                "We're going to ship it on Friday for sure. We're going to ship it on Friday for sure.".into(),
            ),
        ];
        for (raw, out) in &cases {
            assert!(!is_duplicated_transcript(raw, out), "should accept: {out:?}");
        }
    }

    // --- stripThinking port ---

    #[test]
    fn strip_thinking_cases() {
        assert_eq!(strip_thinking("<think>reasoning</think>Hello."), "Hello.");
        assert_eq!(strip_thinking("A <think>x</think>B<THINK>y</THINK> C"), "A B C");
        assert_eq!(strip_thinking("<think>a<think>b</think>c</think>Done"), "Done");
        assert_eq!(strip_thinking("Hello <think>never closed"), "Hello");
        assert_eq!(strip_thinking("stray </think> closer"), "stray  closer");
        assert_eq!(strip_thinking("  plain text  "), "plain text");
        assert_eq!(strip_thinking("héllo <think>ü</think>wörld"), "héllo wörld");
    }

    // --- sanitize ---

    #[test]
    fn sanitize_strips_preambles_labels_quotes_fences() {
        let raw = "send it by friday";
        for (out, want) in [
            ("Here is the cleaned transcript:\n\nSend it by Friday.", "Send it by Friday."),
            ("Here's the cleaned transcript:\nSend it by Friday.", "Send it by Friday."),
            ("Cleaned transcript:\nSend it by Friday.", "Send it by Friday."),
            ("**Cleaned transcript:**\nSend it by Friday.", "Send it by Friday."),
            ("**Cleaned transcript**: Send it by Friday.", "Send it by Friday."),
            ("Cleaned transcript: Send it by Friday.", "Send it by Friday."),
            ("Sure! Here is the cleaned transcript:\n\"Send it by Friday.\"", "Send it by Friday."),
            ("\"Send it by Friday.\"", "Send it by Friday."),
            ("\u{201C}Send it by Friday.\u{201D}", "Send it by Friday."),
            ("```\nSend it by Friday.\n```", "Send it by Friday."),
            ("```text\nSend it by Friday.\n```", "Send it by Friday."),
            ("<transcript>\nSend it by Friday.\n</transcript>", "Send it by Friday."),
            ("<think>hmm</think>\nSend it by Friday.", "Send it by Friday."),
            ("Output: Send it by Friday.", "Send it by Friday."),
        ] {
            assert_eq!(sanitize(out, raw), want, "{out}");
        }
    }

    #[test]
    fn sanitize_keeps_real_content() {
        // Quotes inside, or dictated by the speaker, stay.
        assert_eq!(sanitize("\"Hi,\" she said, \"bye.\"", "hi she said bye"), "\"Hi,\" she said, \"bye.\"");
        assert_eq!(sanitize("\"Hello.\"", "\"hello\""), "\"Hello.\"");
        // A colon in the first line that is not a label.
        assert_eq!(sanitize("Note: call Marie.", "note call marie"), "Note: call Marie.");
        assert_eq!(sanitize("Dear Marie: here is the plan.", "dear marie here is the plan"), "Dear Marie: here is the plan.");
        // The speaker dictated the label itself.
        assert_eq!(sanitize("Cleaned transcript: it works.", "cleaned transcript: it works"), "Cleaned transcript: it works.");
        assert_eq!(sanitize("**Send it.**", "send it"), "**Send it.**");
        // A spoken "colon" after a label-like word is dictated content.
        assert_eq!(sanitize("Output: done.", "output colon done"), "Output: done.");
        assert_eq!(sanitize("Result: pass", "result colon pass"), "Result: pass");
    }

    // --- validate ---

    #[test]
    fn validate_accepts_normal_cleanup() {
        assert_eq!(validate(RAW, CLEAN).unwrap(), CLEAN);
        assert_eq!(validate(RAW, "Cleaned transcript:\nCan you send me the report by Friday?").unwrap(), CLEAN);
        assert_eq!(validate("um uh", "").unwrap(), "");
        assert_eq!(validate("um like you know", "  ").unwrap(), "");
        assert_eq!(validate("ok", "OK.").unwrap(), "OK.");
        assert_eq!(validate(LONG_RAW, LONG_CLEAN).unwrap(), LONG_CLEAN);
    }

    fn invalid(r: Result<String, LlmError>) -> String {
        match r {
            Err(LlmError::InvalidOutput(m)) => m,
            other => panic!("expected InvalidOutput, got {other:?}"),
        }
    }

    #[test]
    fn validate_rejects_duplicates_ratio_and_empty() {
        assert!(invalid(validate(RAW, &format!("{CLEAN} {CLEAN}"))).contains("repeated"));
        assert!(invalid(validate(RAW, "")).contains("empty"));
        assert!(invalid(validate(RAW, "Cleaned transcript:")).contains("empty"));
        // Ratio: 11-word input answered with a long essay / a single word.
        let essay = "A report is a structured document that presents findings, analysis and recommendations about a particular subject, usually prepared for a specific audience such as managers, customers or regulators, and it typically includes an introduction, a body with evidence, and a conclusion.";
        assert!(invalid(validate(RAW, essay)).contains("ratio"));
        assert!(invalid(validate(LONG_RAW, "Okay.")).contains("ratio"));
        // Short inputs skip the ratio check.
        assert!(validate("hello there friend", "Hello there, friend! How can I help you today with anything?").is_ok());
    }

    #[test]
    fn tokens_handle_contractions_cjk_and_fullwidth() {
        assert_eq!(comparison_tokens("I'm going, to Marie's!"), ["im", "going", "to", "maries"]);
        assert_eq!(comparison_tokens("请你明天"), ["请", "你", "明", "天"]);
        assert_eq!(comparison_tokens("Ｃａｎ you"), ["can", "you"]);
        assert_eq!(comparison_tokens("$300 at 5:30"), ["300", "at", "5", "30"]);
    }
}
