//! Voice rewrite trigger: detects a spoken phrase (default "rewrite this concisely") in a cleaned
//! dictation and splits it into the body that is sent to the AI provider and, when the speaker
//! kept talking after the phrase ("... rewrite this concisely and make it formal"), the extra
//! instructions for the model.
//!
//! Pure, no Win32, no config I/O. Matching is token-aligned, insensitive to punctuation, case,
//! whitespace and apostrophes, with no edit-distance fuzz. A phrase that ends the text (optionally
//! followed by one courtesy group, [`COURTESY`]) wins; otherwise the last occurrence followed by
//! more words is used and those words become the instructions. Trailing connectives
//! ([`CONNECTIVES`]) are stripped from the body. Runs after `cleanup::process_pre_llm`.
//!
//! Why no fuzz: a false positive silently rewrites (destroys) the user's text, a false negative
//! pastes the literal phrase, which is visible and trivially undone. A fuzzy match would also make
//! "write this concisely", a legitimate ending, fire the rewrite.

/// A validated trigger phrase, squashed to lowercase letters/digits without separators.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Trigger {
    squashed: String,
}

/// Longest phrase accepted (characters, before cleaning).
pub const MAX_PHRASE_CHARS: usize = 80;
/// Shortest phrase accepted: this many "weighted" characters (a CJK character counts as two).
pub const MIN_WEIGHT: usize = 6;
/// Fewest tokens a phrase must have (a single common word would hijack ordinary dictations).
pub const MIN_TOKENS: usize = 2;

impl Trigger {
    /// Build a trigger from the user's phrase. `clean` is `cleanup::process_pre_llm` bound to
    /// the dictation's filler mode and language (so a phrase containing filler words still
    /// matches the equally cleaned dictation). `None` when the phrase has fewer than 2 tokens,
    /// fewer than 6 squashed characters (CJK characters count double), or more than 80 characters
    /// (the UI shows the same rule).
    pub fn new(phrase: &str, clean: impl Fn(&str) -> String) -> Option<Trigger> {
        Trigger::check(phrase, clean).ok()
    }

    /// [`Trigger::new`] with the reason a phrase is unusable, for the Settings page.
    pub fn check(phrase: &str, clean: impl Fn(&str) -> String) -> Result<Trigger, TriggerProblem> {
        let phrase = phrase.trim();
        if phrase.is_empty() {
            return Err(TriggerProblem::Empty);
        }
        if phrase.chars().count() > MAX_PHRASE_CHARS {
            return Err(TriggerProblem::TooLong);
        }
        let typed = tokens(phrase).len();
        if typed == 0 {
            return Err(TriggerProblem::Empty);
        }
        let toks = tokens(&clean(phrase));
        if toks.is_empty() {
            // Nothing but fillers: the equally cleaned dictation could never contain it.
            return Err(TriggerProblem::OnlyFillers);
        }
        let squashed: String = toks.iter().map(|t| t.sq.as_str()).collect();
        if toks.len() < MIN_TOKENS {
            return Err(if typed >= MIN_TOKENS { TriggerProblem::FillersRemoved } else { TriggerProblem::TooShort });
        }
        if squashed.chars().map(|c| if is_cjk(c) { 2 } else { 1 }).sum::<usize>() < MIN_WEIGHT {
            return Err(TriggerProblem::TooShort);
        }
        Ok(Trigger { squashed })
    }
}

/// Why [`Trigger::check`] rejected a phrase.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TriggerProblem {
    /// No words.
    Empty,
    /// Fewer than two words or fewer than six letters.
    TooShort,
    /// More than [`MAX_PHRASE_CHARS`] characters.
    TooLong,
    /// Every word is a filler ("um", "you know"): filler removal would delete the phrase.
    OnlyFillers,
    /// Filler removal leaves fewer than two words.
    FillersRemoved,
}

impl TriggerProblem {
    /// One-line text for the Settings page.
    pub fn message(self) -> &'static str {
        match self {
            TriggerProblem::Empty => "Type a phrase of at least two words.",
            TriggerProblem::TooShort => "Use at least two words (six letters).",
            TriggerProblem::TooLong => "Use at most 80 characters.",
            TriggerProblem::OnlyFillers => "Filler words are removed before matching; pick other words.",
            TriggerProblem::FillersRemoved => "Filler words in the phrase are removed before matching, leaving too little.",
        }
    }
}

/// Number of word tokens in `text` (a CJK character counts as one word).
pub fn word_count(text: &str) -> usize {
    tokens(text).len()
}

/// Result of [`detect`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Detect {
    /// The text does not end with the trigger.
    NoMatch,
    /// The text is only the trigger (nothing to rewrite).
    OnlyPhrase,
    /// The text contains the trigger; `body` is everything before it, trimmed, with trailing
    /// connectives removed. `instructions` are the words spoken after the phrase (leading
    /// connectives removed), `None` when the phrase ended the dictation.
    Match { body: String, instructions: Option<String> },
}

/// Words stripped from the end of the body, before the trigger ("..., and rewrite this concisely").
pub const CONNECTIVES: &[&str] = &["and", "then", "so", "please", "also"];
/// Courtesy groups allowed AFTER the trigger ("rewrite this concisely, thank you").
pub const COURTESY: &[&[&str]] = &[&["please"], &["thanks"], &["thank", "you"]];

/// Characters trimmed from the end of the body once the trigger is cut off (a clause break that
/// led into the phrase). Sentence-final `. ! ?` are kept.
/// Opening quotes and brackets are included: an STT that quotes the phrase (`Friday "rewrite this concisely"`
/// with curly quotes) must not leave the opening mark dangling at the end of the body. Closing marks stay.
const TRAILING_SEPARATORS: &[char] = &[
    ',', ';', ':', '-', '\u{2013}', '\u{2014}', '\u{2026}', '\u{FF0C}', '\u{3001}', '\u{FF1B}', '\u{FF1A}', '\u{FF0D}', '\u{201C}', '\u{201E}', '\u{00AB}',
    '\u{2039}', '(', '[', '{', '\u{300C}', '\u{300E}',
];

fn is_separator(c: char) -> bool {
    c.is_whitespace() || TRAILING_SEPARATORS.contains(&c)
}

/// Words dropped from the start of the instructions ("... concisely, and then make it formal").
const LEADING_CONNECTIVES: &[&str] = &["and", "then", "also", "so", "but", "just"];
/// Words that alone are only politeness after the phrase ("..., thank you so much"), not instructions.
const POLITE: &[&str] = &["please", "thanks", "thank", "you", "very", "much", "so", "a", "lot"];

/// Detect the trigger in `text` (already through `process_pre_llm`).
pub fn detect(text: &str, t: &Trigger) -> Detect {
    let toks = tokens(text);
    if toks.is_empty() || t.squashed.is_empty() {
        return Detect::NoMatch;
    }
    // The phrase ends the dictation (optionally before one courtesy group): no instructions.
    if let Some(cut) = align(&toks, toks.len(), &t.squashed).or_else(|| courtesy_start(&toks).and_then(|end| align(&toks, end, &t.squashed))) {
        return split_at(text, &toks, cut, None);
    }
    // The last occurrence followed by more words: those words are instructions for the model.
    for end in (1..toks.len()).rev() {
        if let Some(cut) = align(&toks, end, &t.squashed) {
            return split_at(text, &toks, cut, trim_instructions(&text[toks[end - 1].end..]));
        }
    }
    Detect::NoMatch
}

fn split_at(text: &str, toks: &[Tok], cut: usize, instructions: Option<String>) -> Detect {
    match trim_body(&text[..toks[cut].start]) {
        Some(body) => Detect::Match { body, instructions },
        None => Detect::OnlyPhrase,
    }
}

/// The words after the phrase without the separators and connectives that led into them, or
/// `None` when nothing but politeness is left.
fn trim_instructions(raw: &str) -> Option<String> {
    let lead = |c: char| is_separator(c) || matches!(c, '.' | '!' | '?' | '\u{3002}' | '\u{FF01}' | '\u{FF1F}');
    let mut s = raw.trim_start_matches(lead);
    while let Some(first) = tokens(s).first().filter(|t| t.start == 0 && LEADING_CONNECTIVES.contains(&t.sq.as_str())) {
        s = s[first.end..].trim_start_matches(lead);
    }
    let toks = tokens(s);
    if toks.iter().all(|t| POLITE.contains(&t.sq.as_str())) {
        return None;
    }
    Some(s.trim_end().to_string())
}

/// Index of the first token of `squashed` when it ends exactly at token `end`, token aligned.
fn align(toks: &[Tok], end: usize, squashed: &str) -> Option<usize> {
    let mut acc = String::new();
    for i in (0..end).rev() {
        acc.insert_str(0, &toks[i].sq);
        if acc.len() == squashed.len() {
            return (acc == squashed).then_some(i);
        }
        if acc.len() > squashed.len() {
            return None;
        }
    }
    None
}

/// Token count left after removing ONE trailing courtesy group, if the text ends with one.
fn courtesy_start(toks: &[Tok]) -> Option<usize> {
    COURTESY
        .iter()
        .filter(|g| g.len() < toks.len())
        .find(|g| toks[toks.len() - g.len()..].iter().zip(g.iter()).all(|(t, w)| t.sq == *w))
        .map(|g| toks.len() - g.len())
}

/// Trim the cut-off body and strip the connectives that led into the phrase ("... Friday, and
/// then rewrite this concisely"). `None` when no word is left.
fn trim_body(raw: &str) -> Option<String> {
    let mut end = raw.trim_end_matches(is_separator).len();
    let toks = tokens(&raw[..end]);
    let mut n = toks.len();
    // A connective only counts when it is the last thing before the phrase: a sentence-final
    // "Thanks so." keeps its word.
    while n > 0 && toks[n - 1].end == end && CONNECTIVES.contains(&toks[n - 1].sq.as_str()) {
        end = raw[..toks[n - 1].start].trim_end_matches(is_separator).len();
        n -= 1;
    }
    (n > 0).then(|| raw[..end].trim().to_string())
}

// ---------------------------------------------------------------------------------------------
// Tokenizer with byte spans
// ---------------------------------------------------------------------------------------------

/// One word-like token: lowercase letters/digits (apostrophes dropped) and its byte span in the
/// text it came from.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Tok {
    sq: String,
    start: usize,
    end: usize,
}

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

/// Minimal NFKC: fullwidth ASCII variants and the ideographic space.
fn fold(c: char) -> char {
    match c as u32 {
        0xFF01..=0xFF5E => char::from_u32(c as u32 - 0xFEE0).unwrap_or(c),
        0x3000 => ' ',
        _ => c,
    }
}

fn tokens(s: &str) -> Vec<Tok> {
    let mut out: Vec<Tok> = Vec::new();
    let mut cur: Option<Tok> = None;
    for (i, ch) in s.char_indices() {
        let c = fold(ch);
        let end = i + ch.len_utf8();
        if matches!(c, '\'' | '\u{2019}' | '\u{2018}' | '`' | '\u{02BC}') {
            // Glued to the word: "don't" == "dont".
            continue;
        }
        if is_cjk(c) {
            out.extend(cur.take());
            out.push(Tok { sq: c.to_string(), start: i, end });
        } else if c.is_alphanumeric() || (is_mark(c) && cur.is_some()) {
            let t = cur.get_or_insert_with(|| Tok { sq: String::new(), start: i, end });
            t.sq.extend(c.to_lowercase());
            t.end = end;
        } else {
            out.extend(cur.take());
        }
    }
    out.extend(cur);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::cleanup::{self, CleanupCtx};
    use crate::core::config::FillerMode;

    const DEFAULT: &str = "rewrite this concisely";

    fn ident(s: &str) -> String {
        s.to_string()
    }

    fn trig(phrase: &str) -> Trigger {
        Trigger::new(phrase, ident).unwrap_or_else(|| panic!("trigger {phrase:?} rejected"))
    }

    fn d(text: &str) -> Detect {
        detect(text, &trig(DEFAULT))
    }

    fn body(text: &str) -> String {
        match d(text) {
            Detect::Match { body, .. } => body,
            other => panic!("{text:?} -> {other:?}"),
        }
    }

    fn m(b: &str) -> Detect {
        Detect::Match { body: b.to_string(), instructions: None }
    }

    fn mi(b: &str, i: &str) -> Detect {
        Detect::Match { body: b.to_string(), instructions: Some(i.to_string()) }
    }

    /// The real cleanup stage, as the pipeline binds it.
    fn real_clean(mode: FillerMode, language: &str) -> impl Fn(&str) -> String + '_ {
        move |s: &str| {
            cleanup::process_pre_llm(
                s,
                &CleanupCtx { language, filler_mode: mode, dictionary: &[], snippets: &[], speech_ms: None, fuzzy_dictionary: false, spoken_line_breaks: false },
            )
        }
    }

    // ---- the 28-row table from the research -------------------------------------------------

    #[test]
    fn t01_sentence_then_phrase() {
        assert_eq!(d("Send me the report by Friday. Rewrite this concisely."), m("Send me the report by Friday."));
    }
    #[test]
    fn t02_no_punctuation() {
        assert_eq!(d("send me the report by friday rewrite this concisely"), m("send me the report by friday"));
    }
    #[test]
    fn t03_commas_inside_the_phrase() {
        assert_eq!(d("Send it Friday, rewrite this, concisely!"), m("Send it Friday"));
    }
    #[test]
    fn t04_and_connective() {
        assert_eq!(d("Send it Friday, and rewrite this concisely."), m("Send it Friday"));
    }
    #[test]
    fn t05_and_then_connective() {
        assert_eq!(d("Send it Friday and then rewrite this concisely"), m("Send it Friday"));
    }
    #[test]
    fn t06_please_prefix_is_only_the_phrase() {
        assert_eq!(d("Please rewrite this concisely."), Detect::OnlyPhrase);
    }
    #[test]
    fn t07_shouting() {
        assert_eq!(d("REWRITE THIS CONCISELY"), Detect::OnlyPhrase);
    }
    #[test]
    fn t08_only_the_phrase() {
        assert_eq!(d("Rewrite this concisely."), Detect::OnlyPhrase);
    }
    #[test]
    fn t09_hyphenated() {
        assert_eq!(d("I need it Monday. Re-write this concisely."), m("I need it Monday."));
    }
    #[test]
    fn t10_whitespace_and_newlines() {
        assert_eq!(d("I need it Monday.  Rewrite   this\nconcisely ."), m("I need it Monday."));
    }
    #[test]
    fn t11_courtesy_after_the_phrase() {
        assert_eq!(d("We ship Friday. Rewrite this concisely, please."), m("We ship Friday."));
    }
    #[test]
    fn t12_thanks_after_the_phrase() {
        assert_eq!(d("We ship Friday. Rewrite this concisely. Thanks."), m("We ship Friday."));
        assert_eq!(d("We ship Friday. Rewrite this concisely. Thank you."), m("We ship Friday."));
        assert_eq!(d("We ship Friday. Rewrite this concisely, thank you very much."), m("We ship Friday."), "politeness is not an instruction");
    }
    #[test]
    fn t13_phrase_first_with_instructions_is_only_the_phrase() {
        assert_eq!(d("Rewrite this concisely and send it to Bob."), Detect::OnlyPhrase);
    }
    #[test]
    fn t14_token_misalignment() {
        assert_eq!(d("Please overwrite this concisely."), Detect::NoMatch);
        assert_eq!(d("We rewriter this concisely."), Detect::NoMatch);
    }
    #[test]
    fn t15_no_fuzz() {
        assert_eq!(d("Please write this concisely."), Detect::NoMatch);
        assert_eq!(d("Rewrite these concisely."), Detect::NoMatch);
    }
    #[test]
    fn t16_other_adverb() {
        assert_eq!(d("Rewrite this precisely."), Detect::NoMatch);
    }
    #[test]
    fn t17_words_after_the_phrase_are_instructions() {
        assert_eq!(d("He said rewrite this concisely to me."), mi("He said", "to me."));
        assert_eq!(d("We ship Friday. Rewrite this concisely and make it formal."), mi("We ship Friday.", "make it formal."));
        assert_eq!(d("We ship Friday, rewrite this concisely, and then translate it to Spanish"), mi("We ship Friday", "translate it to Spanish"));
        assert_eq!(d("We ship Friday. Rewrite this concisely. As bullet points, please."), mi("We ship Friday.", "As bullet points, please."));
    }
    #[test]
    fn t18_genuine_sentence_matches_by_design() {
        // Documented limitation: the user ends with another word or turns the feature off.
        assert_eq!(d("we need to rewrite this concisely"), m("we need to"));
    }
    #[test]
    fn t19_apostrophes() {
        let t = trig("don\u{2019}t ramble");
        assert_eq!(detect("Ship it Friday. Don't ramble.", &t), m("Ship it Friday."));
        assert_eq!(detect("Ship it Friday. Don\u{2019}t ramble!", &t), m("Ship it Friday."));
        assert_eq!(detect("Ship it Friday. Dont ramble", &t), m("Ship it Friday."));
        assert_eq!(detect("Ship it Friday. Do not ramble", &t), Detect::NoMatch);
    }
    #[test]
    fn t20_numbers() {
        let t = trig("shorten to 2 lines");
        assert_eq!(detect("Notes from the call. Shorten to 2 lines.", &t), m("Notes from the call."));
        assert_eq!(detect("Notes from the call. Shorten to two lines.", &t), Detect::NoMatch);
    }
    #[test]
    fn t21_german() {
        let t = trig("fasse das kurz");
        assert_eq!(detect("Wir liefern am Freitag. Fasse das kurz.", &t), m("Wir liefern am Freitag."));
        // Umlauts and eszett survive the lowercase fold.
        let t = trig("Fasse dich kürzer Größe");
        assert_eq!(detect("Wir liefern. fasse Dich KÜRZER größe", &t), m("Wir liefern."));
    }
    #[test]
    fn t22_cjk() {
        let t = trig("请简化");
        assert_eq!(detect("明天开会。请简化。", &t), m("明天开会。"));
        assert_eq!(detect("请简化", &t), Detect::OnlyPhrase);
        assert_eq!(detect("请简化明天开会", &t), Detect::OnlyPhrase, "the phrase first: nothing before it to rewrite");
        assert_eq!(detect("明天开会。请简化，用英文", &t), mi("明天开会。", "用英文"));
        let ja = trig("簡潔に書き直して");
        assert_eq!(detect("明日の会議です。簡潔に書き直して", &ja), m("明日の会議です。"));
    }
    #[test]
    fn t23_fullwidth_punctuation() {
        assert_eq!(d("Send it Friday\u{FF0C} rewrite this concisely\u{FF01}"), m("Send it Friday"));
        assert_eq!(d("Send it Friday\u{3002} rewrite this concisely\u{3002}"), m("Send it Friday\u{3002}"));
        // Fullwidth LETTERS fold to ASCII.
        assert_eq!(d("Send it Friday. \u{FF52}\u{FF45}\u{FF57}\u{FF52}\u{FF49}\u{FF54}\u{FF45} this concisely"), m("Send it Friday."));
        // The ideographic space separates words.
        assert_eq!(d("Send it Friday.\u{3000}Rewrite\u{3000}this\u{3000}concisely"), m("Send it Friday."));
    }
    #[test]
    fn t24_trigger_with_filler_is_cleaned_like_the_dictation() {
        let clean = real_clean(FillerMode::Standard, "en");
        let t = Trigger::new("rewrite, um, this concisely", &clean).expect("cleaned phrase is still usable");
        assert_eq!(t, trig(DEFAULT), "the filler is cleaned out of the phrase");
        let text = clean("Ship it Friday. Rewrite, um, this concisely.");
        assert_eq!(detect(&text, &t), m("Ship it Friday."), "{text}");
        // A phrase whose filler would survive in the dictation cannot be matched by cleaned text:
        // the trigger is cleaned the same way, so the pair stays consistent.
        let like = Trigger::new("rewrite this like a pro", &clean).expect("usable");
        let text = clean("Ship it Friday. Rewrite this like a pro.");
        assert!(matches!(detect(&text, &like), Detect::Match { .. }), "{text}");
    }
    #[test]
    fn t25_invalid_triggers() {
        assert!(Trigger::new("", ident).is_none());
        assert!(Trigger::new("   ", ident).is_none());
        assert!(Trigger::new("concisely", ident).is_none(), "one word");
        assert!(Trigger::new("a b c", ident).is_none(), "fewer than 6 letters");
        assert!(Trigger::new("go on", ident).is_none(), "4 letters");
        assert!(Trigger::new("...", ident).is_none());
        assert!(Trigger::new(&"word ".repeat(40), ident).is_none(), "longer than 80 chars");
        assert!(Trigger::new(&format!("{} {}", "a".repeat(40), "b".repeat(39)), ident).is_some(), "exactly 80 chars");
        assert!(Trigger::new(&format!("{} {}", "a".repeat(40), "b".repeat(40)), ident).is_none(), "81 chars");
        assert!(Trigger::new("rewrite this", ident).is_some());
        assert!(Trigger::new("  rewrite   THIS  ", ident).is_some());
        // Only fillers: cleaning empties the phrase.
        let clean = real_clean(FillerMode::Standard, "en");
        assert!(Trigger::new("um uh", &clean).is_none());
        assert_eq!(Trigger::check("um, uh", &clean), Err(TriggerProblem::OnlyFillers));
        assert_eq!(Trigger::check("", &clean), Err(TriggerProblem::Empty));
        assert_eq!(Trigger::check("?!", &clean), Err(TriggerProblem::Empty));
        assert_eq!(Trigger::check("hello", &clean), Err(TriggerProblem::TooShort));
        assert_eq!(Trigger::check(&"x ".repeat(50), &clean), Err(TriggerProblem::TooLong));
        for p in [TriggerProblem::Empty, TriggerProblem::TooShort, TriggerProblem::TooLong, TriggerProblem::OnlyFillers, TriggerProblem::FillersRemoved] {
            assert!(!p.message().is_empty());
        }
    }
    #[test]
    fn t25b_filler_leaves_too_little() {
        let clean = real_clean(FillerMode::Standard, "en");
        // "um" is dropped, "rewrite" alone is one token: refused with the filler explanation.
        assert_eq!(Trigger::check("um rewrite", &clean), Err(TriggerProblem::FillersRemoved));
        // Filler mode off keeps the phrase verbatim.
        let off = real_clean(FillerMode::Off, "en");
        assert!(Trigger::new("um rewrite", &off).is_some());
    }
    #[test]
    fn t26_short_body() {
        assert_eq!(d("ok rewrite this concisely"), m("ok"));
    }
    #[test]
    fn t27_separators_inside_the_phrase() {
        assert_eq!(d("Ship it Friday. re_write this/concisely"), m("Ship it Friday."));
        assert_eq!(d("Ship it Friday. re-write-this-concisely"), m("Ship it Friday."));
        assert_eq!(d("Ship it Friday. rewrite... this... concisely..."), m("Ship it Friday."));
    }
    #[test]
    fn t28_unicode_case() {
        assert_eq!(d("REWRITE THIS CONCISELY"), Detect::OnlyPhrase);
        assert_eq!(d("Rewrite This Concisely"), Detect::OnlyPhrase);
        assert_eq!(d("Ship it. ReWrItE tHiS cOnCiSeLy"), m("Ship it."));
        // Turkish dotted/dotless I and Greek capital sigma are lowercased the same on both sides.
        let t = trig("İSTANBUL kısalt");
        assert_eq!(detect("Toplantı yarın. İSTANBUL kısalt", &t), m("Toplantı yarın."));
        let t = trig("ΣΟΦΟΣ ΚΕΙΜΕΝΟ");
        assert_eq!(detect("Καλημέρα. ΣΟΦΟΣ ΚΕΙΜΕΝΟ", &t), m("Καλημέρα."));
    }

    // ---- body trimming ----------------------------------------------------------------------

    #[test]
    fn body_keeps_terminal_punctuation_and_trims_clause_breaks() {
        assert_eq!(body("Is it ready? Rewrite this concisely."), "Is it ready?");
        assert_eq!(body("Great news! Rewrite this concisely."), "Great news!");
        assert_eq!(body("Send it Friday; rewrite this concisely"), "Send it Friday");
        assert_eq!(body("Send it Friday: rewrite this concisely"), "Send it Friday");
        assert_eq!(body("Send it Friday - rewrite this concisely"), "Send it Friday");
        assert_eq!(body("Send it Friday \u{2014} rewrite this concisely"), "Send it Friday");
        assert_eq!(body("Send it Friday\u{2026} rewrite this concisely"), "Send it Friday");
        assert_eq!(body("Send it Friday... rewrite this concisely"), "Send it Friday...");
    }

    #[test]
    fn a_quoted_phrase_leaves_no_dangling_opening_mark() {
        assert_eq!(body("Send it Friday \u{201C}rewrite this concisely\u{201D}"), "Send it Friday");
        assert_eq!(body("Send it Friday (rewrite this concisely)"), "Send it Friday");
        // A closing mark that belongs to the body stays.
        assert_eq!(body("She said \u{201C}yes\u{201D} rewrite this concisely"), "She said \u{201C}yes\u{201D}");
    }

    #[test]
    fn body_preserves_inner_newlines_and_trims_the_ends() {
        assert_eq!(body("  Hi Sam,\nSend it Friday.\n\nRewrite this concisely.  "), "Hi Sam,\nSend it Friday.");
        assert_eq!(body("Line one.\nLine two\nrewrite this concisely"), "Line one.\nLine two");
    }

    #[test]
    fn connectives_are_stripped_in_any_combination() {
        assert_eq!(body("Send it Friday, and also rewrite this concisely"), "Send it Friday");
        assert_eq!(body("Send it Friday so please rewrite this concisely"), "Send it Friday");
        assert_eq!(body("Send it Friday. And then, please, rewrite this concisely"), "Send it Friday.");
        assert_eq!(body("Send it Friday. Also rewrite this concisely"), "Send it Friday.");
        // Not at the end of the body (a '.' follows): the sentence keeps its last word.
        assert_eq!(body("Thanks so. Rewrite this concisely"), "Thanks so.");
        // A connective inside a longer word is untouched.
        assert_eq!(body("Send it to Sandy rewrite this concisely"), "Send it to Sandy");
        assert_eq!(body("It was a bandwidth and rewrite this concisely"), "It was a bandwidth");
        assert_eq!(body("See the android rewrite this concisely"), "See the android");
    }

    #[test]
    fn only_connectives_means_nothing_to_rewrite() {
        assert_eq!(d("And then rewrite this concisely"), Detect::OnlyPhrase);
        assert_eq!(d("So, please rewrite this concisely."), Detect::OnlyPhrase);
        assert_eq!(d("... rewrite this concisely"), Detect::OnlyPhrase);
        assert_eq!(d("rewrite this concisely please"), Detect::OnlyPhrase, "courtesy only after");
        assert_eq!(d("Please. Rewrite this concisely"), m("Please."), "a sentence is not a bare connective");
    }

    #[test]
    fn courtesy_applies_once_and_only_after_the_phrase() {
        assert_eq!(d("Ship it Friday rewrite this concisely thanks"), m("Ship it Friday"));
        assert_eq!(d("Ship it Friday rewrite this concisely please"), m("Ship it Friday"));
        assert_eq!(d("Ship it Friday rewrite this concisely thank you"), m("Ship it Friday"));
        assert_eq!(d("Ship it Friday rewrite this concisely please thanks"), m("Ship it Friday"), "politeness only, not instructions");
        assert_eq!(d("Ship it Friday rewrite this concisely thank"), m("Ship it Friday"));
        assert_eq!(d("Thanks. Rewrite this concisely. Thanks."), m("Thanks."));
        // A courtesy word BEFORE the phrase is a connective only for "please".
        assert_eq!(d("Ship it Friday thanks rewrite this concisely"), m("Ship it Friday thanks"));
    }

    #[test]
    fn phrase_must_end_the_text() {
        assert_eq!(d(""), Detect::NoMatch);
        assert_eq!(d("   "), Detect::NoMatch);
        assert_eq!(d("..."), Detect::NoMatch);
        assert_eq!(d("rewrite this"), Detect::NoMatch, "partial phrase");
        assert_eq!(d("this concisely"), Detect::NoMatch);
        assert_eq!(d("Send it. Rewrite this concisely now"), mi("Send it.", "now"));
        assert_eq!(d("rewrite this concisely rewrite this concisely"), m("rewrite this concisely"), "the LAST occurrence is cut");
    }

    #[test]
    fn trigger_may_start_mid_text_when_aligned() {
        assert_eq!(d("my rewrite this concisely"), m("my"));
        assert_eq!(d("Ship it. Rewrite this concisely"), m("Ship it."));
    }

    #[test]
    fn digits_and_unicode_words_are_tokens() {
        assert_eq!(d("Room 12. Rewrite this concisely"), m("Room 12."));
        assert_eq!(d("Café ☕ — rewrite this concisely 🙂"), m("Café ☕"));
    }

    #[test]
    fn combining_marks_stay_with_their_word() {
        // "concisely" typed with a combining accent never aligns with the plain trigger.
        assert_eq!(d("Ship it rewrite this concise\u{0301}ly"), Detect::NoMatch);
        let t = trig("re\u{0301}ecrire ceci");
        assert!(matches!(detect("Livre le. Re\u{0301}ecrire ceci", &t), Detect::Match { .. }));
    }

    #[test]
    fn custom_triggers() {
        let t = trig("make it short");
        assert_eq!(detect("Long winded thing. Make it short.", &t), m("Long winded thing."));
        let t = trig("tl;dr please");
        assert_eq!(detect("Long winded thing. TLDR, please!", &t), m("Long winded thing."));
        let t = trig("send to the cloud");
        assert_eq!(detect("Tell Bob hi send to the cloud", &t), m("Tell Bob hi"));
    }

    #[test]
    fn trigger_ending_in_a_courtesy_word_matches_as_written() {
        let t = trig("make it short please");
        assert_eq!(detect("Tell Bob hi. Make it short please.", &t), m("Tell Bob hi."));
        assert_eq!(detect("Tell Bob hi. Make it short.", &t), Detect::NoMatch);
    }

    #[test]
    fn real_cleanup_then_detect() {
        let clean = real_clean(FillerMode::Standard, "en");
        let t = Trigger::new(DEFAULT, &clean).unwrap();
        let text = clean("Um, so I think, uh, we should ship on Friday, you know, because the the client is waiting. Rewrite, um, this concisely.");
        assert_eq!(detect(&text, &t), m("So I think we should ship on Friday because the client is waiting."), "{text}");
        let text = clean("rewrite this this concisely");
        assert_eq!(detect(&text, &t), Detect::OnlyPhrase, "{text}");
        let text = clean("Send it Friday and then, um, rewrite this concisely, please.");
        assert_eq!(detect(&text, &t), m("Send it Friday"), "{text}");
    }

    // ---- robustness ---------------------------------------------------------------------------

    /// Deterministic xorshift for the property test (no extra crate).
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
            &items[(self.next() % items.len() as u64) as usize]
        }
    }

    const PIECES: &[&str] = &[
        "rewrite", "this", "concisely", "Rewrite", "THIS", "re", "write", "and", "then", "please", "thanks", "thank", "you", "so", "also", "ok", "Friday", "don't",
        "don\u{2019}t", "\u{FF52}\u{FF45}", "请", "简化", "日本語", "Ünï", "ß", "İ", "ΣΟΦΟΣ", "e\u{0301}", "\u{200D}", "\u{0301}", "😀", "🇩🇪", "\u{3000}", "\u{FF0C}",
        ".", ",", "!", "?", "-", "\u{2014}", "\u{2026}", "...", ";", ":", "\n", "\r\n", "\t", " ", "  ", "_", "/", "'", "`", "\"", "(", ")", "1", "2", "0", "\u{0}", "\u{FFFD}",
    ];

    fn random_text(rng: &mut Rng) -> String {
        let n = 1 + (rng.next() % 14) as usize;
        let mut s = String::new();
        for _ in 0..n {
            s.push_str(rng.pick(PIECES));
            if !rng.next().is_multiple_of(3) {
                s.push(' ');
            }
        }
        s
    }

    #[test]
    fn property_detect_never_panics_and_returns_a_clean_prefix() {
        let triggers = [trig(DEFAULT), trig("请简化"), trig("don't ramble"), trig("make it short please"), trig("fasse das kurz")];
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        let mut matches = 0;
        for _ in 0..20_000 {
            let mut text = random_text(&mut rng);
            if rng.next().is_multiple_of(2) {
                // Bias toward the interesting case: end with (a variation of) a trigger.
                text.push_str(rng.pick(&["Rewrite this concisely", "rewrite, this concisely!", "re-write this concisely please", "请简化。", "Don't ramble", "make it short please"]));
                text.push_str(rng.pick(&["", ".", " thanks", ", thank you", " please", "!!", " 🙂"]));
            }
            for t in &triggers {
                if let Detect::Match { body, .. } = detect(&text, t) {
                    matches += 1;
                    assert!(!body.is_empty() && body == body.trim(), "untrimmed body {body:?} from {text:?}");
                    assert!(text.contains(&body), "body is not a slice of the text: {body:?} / {text:?}");
                    let (bt, tt) = (tokens(&body), tokens(&text));
                    assert!(!bt.is_empty(), "body without a word {body:?} from {text:?}");
                    assert!(bt.len() < tt.len());
                    assert!(bt.iter().zip(&tt).all(|(a, b)| a.sq == b.sq), "{body:?} vs {text:?}");
                }
            }
        }
        assert!(matches > 500, "the generator should produce many matches, got {matches}");
    }

    #[test]
    fn property_arbitrary_chars_never_panic() {
        let mut rng = Rng(42);
        let t = trig(DEFAULT);
        for _ in 0..5_000 {
            let n = (rng.next() % 40) as usize;
            let s: String = (0..n)
                .map(|_| match rng.next() % 6 {
                    0 => char::from_u32((rng.next() % 0x80) as u32).unwrap(),
                    1 => char::from_u32(0x300 + (rng.next() % 0x100) as u32).unwrap_or('x'),
                    2 => char::from_u32(0x4E00 + (rng.next() % 0x200) as u32).unwrap_or('x'),
                    3 => char::from_u32(0xFF00 + (rng.next() % 0x100) as u32).unwrap_or('x'),
                    4 => char::from_u32(0x1F300 + (rng.next() % 0x200) as u32).unwrap_or('x'),
                    _ => char::from_u32((rng.next() % 0x11_0000) as u32).unwrap_or('x'),
                })
                .collect();
            let _ = detect(&s, &t);
            let _ = Trigger::new(&s, ident);
            let _ = tokens(&s);
        }
    }

    #[test]
    fn appending_the_phrase_always_ends_in_a_decision() {
        let mut rng = Rng(7);
        let t = trig(DEFAULT);
        for _ in 0..3_000 {
            let text = random_text(&mut rng);
            let joined = format!("{text} {}", rng.pick(&["Rewrite this concisely.", "rewrite, this concisely", "REWRITE THIS CONCISELY!"]));
            assert!(matches!(detect(&joined, &t), Detect::Match { .. } | Detect::OnlyPhrase), "{joined:?}");
        }
    }

    #[test]
    fn long_inputs_stay_fast() {
        let t = trig(DEFAULT);
        let text = format!("{} rewrite this concisely", "word and then ".repeat(20_000));
        let started = std::time::Instant::now();
        assert!(matches!(detect(&text, &t), Detect::Match { .. }));
        // Pathological: nothing but connectives in front (linear, not quadratic).
        let only = format!("{}rewrite this concisely", "and ".repeat(50_000));
        assert_eq!(detect(&only, &t), Detect::OnlyPhrase);
        assert!(started.elapsed().as_millis() < 2_000, "{} ms", started.elapsed().as_millis());
    }

    #[test]
    fn tokens_have_valid_spans() {
        for s in ["Don't stop", "re-write", "Café ☕ — ok", "请简化。", "e\u{0301}x", "'quoted' `tick`", "A\u{FF21}\u{3000}b"] {
            for t in tokens(s) {
                assert!(s.is_char_boundary(t.start) && s.is_char_boundary(t.end) && t.start < t.end, "{s:?} {t:?}");
            }
        }
        let toks = tokens("Don't stop");
        assert_eq!((toks[0].sq.as_str(), toks[0].start, toks[0].end), ("dont", 0, 5));
        assert_eq!(toks[1].sq, "stop");
        // An apostrophe opening a word does not move the span.
        assert_eq!(tokens("'hi").first().map(|t| (t.start, t.end)), Some((1, 3)));
    }

    #[test]
    fn word_count_counts_tokens() {
        assert_eq!(word_count("Send it Friday."), 3);
        assert_eq!(word_count("don't re-write"), 3);
        assert_eq!(word_count("明天开会"), 4);
        assert_eq!(word_count(" ... "), 0);
    }

    #[test]
    fn constants_match_the_contract() {
        assert!(CONNECTIVES.contains(&"and") && CONNECTIVES.contains(&"please"));
        assert!(COURTESY.iter().any(|g| g == &["thank", "you"]));
    }
}
