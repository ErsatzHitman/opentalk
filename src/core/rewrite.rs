//! Voice rewrite trigger: detects a spoken phrase (default "rewrite this concisely") at the END of
//! a cleaned dictation and splits it into the body that is sent to the AI provider.
//!
//! Pure, no Win32, no config I/O. Matching is token-aligned and end-anchored, insensitive to
//! punctuation, case, whitespace and apostrophes, with no edit-distance fuzz; one trailing
//! courtesy group ([`COURTESY`]) may follow the phrase and trailing connectives
//! ([`CONNECTIVES`]) are stripped from the body. Runs after `cleanup::process_pre_llm`.
//!
//! CONTRACT STUB: the signatures below are final; the bodies are placeholders (`Trigger::new`
//! returns `None`, `detect` returns `NoMatch`) until the rewrite agent implements them.
// Contract module: consumers land in later commits; remove this once everything is wired.
#![allow(dead_code)]

/// A validated trigger phrase, squashed to lowercase letters/digits without separators.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Trigger {
    squashed: String,
}

impl Trigger {
    /// Build a trigger from the user's phrase. `clean` is `cleanup::process_pre_llm` bound to
    /// the dictation's filler mode and language (so a phrase containing filler words still
    /// matches the equally cleaned dictation). `None` when the phrase has fewer than 2 tokens,
    /// fewer than 6 squashed characters, or more than 80 characters (the UI shows the same rule).
    pub fn new(phrase: &str, clean: impl Fn(&str) -> String) -> Option<Trigger> {
        let _ = (phrase, &clean);
        None
    }
}

/// Result of [`detect`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Detect {
    /// The text does not end with the trigger.
    NoMatch,
    /// The text is only the trigger (nothing to rewrite).
    OnlyPhrase,
    /// The text ends with the trigger; `body` is everything before it, trimmed, with trailing
    /// connectives removed.
    Match { body: String },
}

/// Words stripped from the end of the body, before the trigger ("..., and rewrite this concisely").
pub const CONNECTIVES: &[&str] = &["and", "then", "so", "please", "also"];
/// Courtesy groups allowed AFTER the trigger ("rewrite this concisely, thank you").
pub const COURTESY: &[&[&str]] = &[&["please"], &["thanks"], &["thank", "you"]];

/// Detect the trigger at the end of `text` (already through `process_pre_llm`).
pub fn detect(text: &str, t: &Trigger) -> Detect {
    let _ = (text, t);
    Detect::NoMatch
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stub_never_matches() {
        assert!(Trigger::new("rewrite this concisely", |s| s.to_string()).is_none());
        let t = Trigger { squashed: "rewritethisconcisely".into() };
        assert_eq!(detect("Send it Friday. Rewrite this concisely.", &t), Detect::NoMatch);
        assert!(CONNECTIVES.contains(&"and") && COURTESY.iter().any(|g| g == &["thank", "you"]));
    }
}
