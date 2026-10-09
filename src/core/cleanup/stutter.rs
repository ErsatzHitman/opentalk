//! Stage 7: stutters and false starts ("the the", "I- I", "wh- what", "b-b-but").
use super::lists::{is_comma_stutter_word, is_stutter_word};
use super::tokens::{Doc, Tok, Ws};

/// Collapse "b-b-but" / "I-I" inside a single hyphenated token into its last part.
fn collapse_hyphenated(text: &str) -> Option<String> {
    if !text.contains('-') {
        return None;
    }
    let parts: Vec<&str> = text.split('-').collect();
    if parts.len() < 2 || parts.iter().any(|p| p.is_empty() || !p.chars().all(char::is_alphabetic)) {
        return None;
    }
    let (last, lead) = parts.split_last()?;
    let first_lc = lead[0].to_lowercase();
    if !lead.iter().all(|p| p.to_lowercase() == first_lc) {
        return None;
    }
    // "re-read", "co-op", "mm-hmm": a single multi-letter lead is a real compound, not a stutter.
    let single_letter = lead.len() == 1 && first_lc.chars().count() == 1;
    let repeated = lead.len() >= 2;
    if !(single_letter || repeated) || !last.to_lowercase().starts_with(&first_lc) {
        return None;
    }
    let mut out = (*last).to_string();
    let lead_upper = lead[0].chars().next().is_some_and(char::is_uppercase);
    let last_lower = last.chars().next().is_some_and(char::is_lowercase);
    if lead_upper && last_lower {
        let mut cs = out.chars();
        if let Some(f) = cs.next() {
            out = f.to_uppercase().chain(cs).collect();
        }
    }
    Some(out)
}

fn is_hyphen_break(t: &Tok<'_>) -> bool {
    t.is_dash() && t.ws == Ws::None
}

pub fn collapse_stutters(doc: &mut Doc<'_>) {
    for t in doc.toks.iter_mut().filter(|t| t.is_word()) {
        if let Some(s) = collapse_hyphenated(&t.text) {
            t.set_text(s);
        }
    }
    let n = doc.toks.len();
    if n < 2 {
        return;
    }
    let toks = &doc.toks;
    let mut del = vec![false; n];
    let mut i = 0;
    while i < n {
        let t = &toks[i];
        if del[i] || !t.is_word() {
            i += 1;
            continue;
        }
        // "I- I", "wh- what": a word cut off by a dash, then the whole word.
        if i + 2 < n && is_hyphen_break(&toks[i + 1]) && toks[i + 2].is_word() {
            let frag = &t.lc;
            let alpha = frag.chars().all(char::is_alphabetic) && frag.chars().count() <= 6;
            if alpha && toks[i + 2].lc.starts_with(&**frag) {
                del[i] = true;
                del[i + 1] = true;
                i += 2;
                continue;
            }
        }
        // "the the", "I I I", "the, the": keep the first copy of a repeated function word.
        if is_stutter_word(&t.lc) && !t.is_numeric() {
            let mut k = i + 1;
            loop {
                if k < n && toks[k].is_word() && toks[k].lc == t.lc && toks[k].ws != Ws::None {
                    del[k] = true;
                    k += 1;
                } else if k + 1 < n
                    && toks[k].is_comma()
                    && toks[k].ws == Ws::None
                    && toks[k + 1].is_word()
                    && toks[k + 1].lc == t.lc
                    && is_comma_stutter_word(&t.lc)
                {
                    del[k] = true;
                    del[k + 1] = true;
                    k += 2;
                } else {
                    break;
                }
            }
            i = k;
            continue;
        }
        i += 1;
    }
    doc.remove(&del);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::cleanup::tokens::tokenize;

    fn run(s: &str) -> String {
        let mut d = tokenize(s);
        collapse_stutters(&mut d);
        d.render()
    }

    #[test]
    fn collapses_function_word_repeats() {
        assert_eq!(run("because the the client"), "because the client");
        assert_eq!(run("I I I think"), "I think");
        assert_eq!(run("The the cat"), "The cat");
        assert_eq!(run("we we should go to to the shop"), "we should go to the shop");
        assert_eq!(run("I, I think so"), "I think so");
        assert_eq!(run("the, the client"), "the client");
    }

    #[test]
    fn collapses_false_starts() {
        assert_eq!(run("I- I think"), "I think");
        assert_eq!(run("wh- what happened"), "what happened");
        assert_eq!(run("Wh- what happened"), "what happened");
        assert_eq!(run("I—I think"), "I think");
        assert_eq!(run("it was go- going well"), "it was going well");
        assert_eq!(run("b-b-but why"), "but why");
        assert_eq!(run("B-b-but why"), "But why");
        assert_eq!(run("I-I know"), "I know");
        assert_eq!(run("s-so then"), "so then");
    }

    #[test]
    fn keeps_legitimate_doubles() {
        for s in [
            "he had had enough",
            "I know that that is true",
            "it is very very good",
            "call 555 555",
            "no no no",
            "so so",
            "re-read the e-mail",
            "a mm-hmm and uh-huh",
            "pre- and post-war",
            "the one- or two-year plan",
            "there, there",
            "bye-bye now",
            "Hey you, you there",
        ] {
            assert_eq!(run(s), s, "{s}");
        }
        assert_eq!(run("Stop. The the end"), "Stop. The end");
    }
}
