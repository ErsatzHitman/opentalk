//! Stage 8 (punctuation repair, sentence capitalisation, lone "i") and stage 11 (spoken line breaks).
use super::lists::{is_abbreviation, is_english_marker};
use super::tokens::{Doc, Kind, Tok, Ws};

pub struct RepairOpts {
    /// Language is Turkish / Azeri: capitalise "i" as "İ".
    pub turkish: bool,
    /// Text is English: lone "i" becomes "I".
    pub english: bool,
    /// Language is French (or looks French): keep the space before `? ! : ; »`.
    pub french: bool,
    /// The language is "auto": decide `english` / `french` from the text itself.
    pub auto: bool,
}

fn is_terminalish(t: &Tok<'_>) -> bool {
    t.is_sentence_end() || t.is_ellipsis()
}

/// Mark orphan punctuation inside runs of consecutive punctuation tokens.
fn mark_punct_runs(toks: &[Tok<'_>], del: &mut [bool]) {
    let n = toks.len();
    let mut i = 0;
    while i < n {
        if toks[i].kind != Kind::Punct {
            i += 1;
            continue;
        }
        let start = i;
        while i < n && toks[i].kind == Kind::Punct {
            i += 1;
        }
        for k in start..i {
            let t = &toks[k];
            let prev = (k > start).then(|| &toks[k - 1]);
            let next = (k + 1 < i).then(|| &toks[k + 1]);
            let sentence_level = |x: &Tok<'_>| is_terminalish(x) || x.is_comma() || matches!(&*x.text, ":" | ";");
            if t.is_comma() || matches!(&*t.text, ":" | ";") {
                // A comma next to a stronger mark ("," + "."), or doubled, is an orphan.
                if next.is_some_and(sentence_level) || prev.is_some_and(is_terminalish) {
                    del[k] = true;
                }
            } else if &*t.text == "." && prev.is_some_and(|p| &*p.text == "." || p.is_ellipsis()) {
                del[k] = true;
            }
        }
    }
}

fn looks_french(toks: &[Tok<'_>]) -> bool {
    toks.iter().any(|t| {
        t.is_word()
            && matches!(
                &*t.lc,
                "le" | "les" | "est" | "vous" | "nous" | "pas" | "une" | "des" | "du" | "que" | "qui" | "pour" | "dans"
                    | "avec" | "merci" | "bonjour" | "c'est" | "j'ai" | "je" | "ça" | "très" | "oui" | "non"
            )
    })
}

fn url_like(w: &str) -> bool {
    if w.contains(['@', '/']) {
        return true;
    }
    let mut parts = w.rsplit('.');
    let last = parts.next().unwrap_or("");
    w.contains('.')
        && last.chars().count() >= 2
        && last.chars().all(char::is_alphabetic)
        && parts.next().is_some_and(|p| p.chars().count() >= 2)
}

/// Upper-case the first letter of a word that is all lower case. Returns None when the word
/// must stay as is (mixed case such as "iPhone", URLs, numbers).
fn capitalized(t: &Tok<'_>, turkish: bool) -> Option<String> {
    let mut chars = t.text.chars();
    let first = chars.next()?;
    if !first.is_lowercase() || t.text.chars().any(char::is_uppercase) || url_like(&t.text) {
        return None;
    }
    let mut out = String::with_capacity(t.text.len() + 2);
    if turkish && first == 'i' {
        out.push('İ');
    } else {
        out.extend(first.to_uppercase());
    }
    out.push_str(chars.as_str());
    Some(out)
}

pub fn repair(doc: &mut Doc<'_>, o: &RepairOpts) {
    let n = doc.toks.len();
    if n == 0 {
        return;
    }
    // 1. Orphan punctuation.
    let mut del = vec![false; n];
    mark_punct_runs(&doc.toks, &mut del);
    // Stand-alone spaced ellipsis between words is a pause mark, not punctuation.
    for i in 1..n.saturating_sub(1) {
        let t = &doc.toks[i];
        if t.is_ellipsis()
            && t.ws == Ws::Space
            && doc.toks[i - 1].is_word()
            && doc.toks[i + 1].is_word()
            && doc.toks[i + 1].ws == Ws::Space
        {
            del[i] = true;
        }
    }
    // Leading junk and dangling trailing commas.
    let mut k = 0;
    while k < n && (del[k] || is_terminalish(&doc.toks[k]) || doc.toks[k].is_comma() || matches!(&*doc.toks[k].text, ":" | ";")) {
        del[k] = true;
        k += 1;
    }
    let mut k = n;
    while k > 0 && (del[k - 1] || doc.toks[k - 1].is_comma()) {
        del[k - 1] = true;
        k -= 1;
    }
    doc.remove(&del);
    if doc.toks.is_empty() {
        return;
    }

    let auto = o.auto;
    let french = o.french || (auto && looks_french(&doc.toks));

    // 2. Spacing around punctuation.
    for i in 0..doc.toks.len() {
        if doc.toks[i].kind != Kind::Punct {
            continue;
        }
        let keep_space_before = french && matches!(&*doc.toks[i].text, "?" | "!" | ":" | ";" | "»") && doc.toks[i].ws != Ws::None;
        if i > 0 && doc.toks[i].is_closing() && doc.toks[i].ws != Ws::None && !keep_space_before && !matches!(doc.toks[i].ws, Ws::Hard(_)) {
            doc.toks[i].ws = Ws::None;
        }
        if doc.toks[i].is_opening()
            && i + 1 < doc.toks.len()
            && doc.toks[i + 1].ws == Ws::Space
            && !matches!(&*doc.toks[i].text, "«")
        {
            doc.toks[i + 1].ws = Ws::None;
        }
        // "word,next" -> "word, next" for ASCII separators before a letter.
        if matches!(&*doc.toks[i].text, "," | ";" | "!" | "?")
            && let Some(next) = doc.toks.get(i + 1)
            && next.is_word()
            && next.ws == Ws::None
            && next.first().is_some_and(|c| c.is_alphabetic() && !super::tokens::is_spaceless(c))
        {
            doc.toks[i + 1].ws = Ws::Space;
        }
    }

    // 3. Capitalisation and the English pronoun "I".
    let english = o.english
        || (auto
            && doc.toks.iter().any(|t| t.is_word() && is_english_marker(&t.lc))
            && !doc.toks.iter().any(|t| t.is_word() && super::lists::is_non_english_marker(&t.lc)));
    let mut at_start = true;
    for i in 0..doc.toks.len() {
        if doc.toks[i].kind == Kind::Punct {
            let t = &doc.toks[i];
            if t.is_sentence_end() {
                let abbreviation = &*t.text == "."
                    && t.ws == Ws::None
                    && i > 0
                    && doc.toks[i - 1].is_word()
                    && {
                        let p = &doc.toks[i - 1];
                        is_abbreviation(&p.lc)
                            || (p.text.chars().count() == 1
                                && p.first().is_some_and(char::is_uppercase)
                                && !matches!(&*p.text, "I" | "A"))
                            || (p.text.contains('.') && !url_like(&p.text))
                    };
                if !abbreviation {
                    at_start = true;
                }
            } else if t.is_comma() || matches!(&*t.text, ":" | ";") {
                at_start = false;
            }
            continue;
        }
        if at_start && let Some(c) = capitalized(&doc.toks[i], o.turkish) {
            doc.toks[i].set_text(c);
        }
        at_start = false;
        if english {
            let t = &doc.toks[i];
            let fix = &*t.text == "i"
                || (t.text.starts_with("i'") || t.text.starts_with("i’"))
                    && matches!(&*t.lc, "i'm" | "i'll" | "i've" | "i'd" | "i'd've" | "i'ma" | "i’m" | "i’ll" | "i’ve" | "i’d");
            if fix {
                let fixed = format!("I{}", &t.text[1..]);
                doc.toks[i].set_text(fixed);
            }
        }
    }
}

/// Stage 11: "new line" / "new paragraph" become line breaks. Whisper punctuates the phrase
/// ("Hello. New line. World."), so a full stop or comma right after it goes too.
pub fn apply_line_breaks(doc: &mut Doc<'_>, turkish: bool) {
    let n = doc.toks.len();
    let mut del = vec![false; n];
    let mut breaks: Vec<(usize, u8)> = Vec::new(); // (span end, newline count)
    let mut i = 0;
    while i + 1 < n {
        let (a, b) = (&doc.toks[i], &doc.toks[i + 1]);
        let kind = if a.is_word() && &*a.lc == "new" && b.is_word() && b.ws != Ws::None {
            match &*b.lc {
                "line" => Some(1u8),
                "paragraph" => Some(2u8),
                _ => None,
            }
        } else {
            None
        };
        let blocked = kind.is_some() && {
            let prev_det = i > 0
                && doc.toks[i - 1].is_word()
                && matches!(
                    &*doc.toks[i - 1].lc,
                    "a" | "an" | "the" | "this" | "that" | "each" | "every" | "another" | "one" | "per" | "any" | "no"
                        | "my" | "your" | "our" | "their" | "his" | "her" | "its"
                );
            let next_of = doc.toks.get(i + 2).is_some_and(|t| t.is_word() && &*t.lc == "of");
            prev_det || next_of
        };
        let Some(nl) = kind.filter(|_| !blocked) else {
            i += 1;
            continue;
        };
        del[i] = true;
        del[i + 1] = true;
        let mut end = i + 2;
        if doc.toks.get(end).is_some_and(|t| (t.is_comma() || &*t.text == ".") && t.ws == Ws::None) {
            del[end] = true;
            end += 1;
        }
        breaks.push((end, nl));
        i = end;
    }
    if breaks.is_empty() {
        return;
    }
    // Newlines belong to the next surviving token (or the end of the text).
    let mut pending: u8 = 0;
    let mut bi = 0;
    let mut trailing = 0u8;
    for k in 0..=n {
        while bi < breaks.len() && breaks[bi].0 == k {
            pending = pending.saturating_add(breaks[bi].1).min(4);
            bi += 1;
        }
        if k == n {
            trailing = pending;
            break;
        }
        if !del[k] && pending > 0 {
            doc.toks[k].ws = Ws::Newlines(pending);
            pending = 0;
        }
    }
    let leading = del[0];
    doc.remove(&del);
    doc.trailing_nl = doc.trailing_nl.max(trailing);
    for (i, t) in doc.toks.iter_mut().enumerate() {
        // A break at the very start leaves no newline, but the text after it still starts a line.
        if (matches!(t.ws, Ws::Newlines(_)) || (i == 0 && leading))
            && t.is_word()
            && let Some(c) = capitalized(t, turkish)
        {
            t.set_text(c);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::cleanup::tokens::tokenize;

    fn rep(s: &str) -> String {
        let mut d = tokenize(s);
        repair(&mut d, &RepairOpts { turkish: false, english: true, french: false, auto: false });
        d.render()
    }

    #[test]
    fn repairs_punctuation() {
        assert_eq!(rep(", so we go ."), "So we go.");
        assert_eq!(rep("we go , then stop"), "We go, then stop");
        assert_eq!(rep("wait,, what"), "Wait, what");
        assert_eq!(rep("ok , ."), "Ok.");
        assert_eq!(rep("hello .. world"), "Hello. World");
        assert_eq!(rep("... so we go"), "So we go");
        assert_eq!(rep("so we go,"), "So we go");
        assert_eq!(rep("hello,world"), "Hello, world");
        assert_eq!(rep("( hello )"), "(Hello)");
        assert_eq!(rep("i think we go ... then"), "I think we go then");
    }

    #[test]
    fn capitalises_sentence_starts_only_where_safe() {
        assert_eq!(rep("hello. how are you? fine! ok"), "Hello. How are you? Fine! Ok");
        assert_eq!(rep("iPhone sales. eBay too"), "iPhone sales. eBay too");
        assert_eq!(rep("see example.com. then go"), "See example.com. Then go");
        assert_eq!(rep("example.com is up"), "example.com is up");
        assert_eq!(rep("ask dr. smith about it"), "Ask dr. smith about it");
        assert_eq!(rep("\"hello,\" she said. \"come in.\""), "\"Hello,\" she said. \"Come in.\"");
        assert_eq!(rep("(hello) there"), "(Hello) there");
        assert_eq!(rep("3 apples. then 4"), "3 apples. Then 4");
    }

    #[test]
    fn fixes_lone_i() {
        assert_eq!(rep("so i think i'm right and i'll go"), "So I think I'm right and I'll go");
        assert_eq!(rep("the i key"), "The I key");
        let mut d = tokenize("i think");
        repair(&mut d, &RepairOpts { turkish: false, english: false, french: false, auto: false });
        assert_eq!(d.render(), "I think");
        let mut d = tokenize("mama i tata");
        repair(&mut d, &RepairOpts { turkish: false, english: false, french: false, auto: true });
        assert_eq!(d.render(), "Mama i tata");
    }

    #[test]
    fn auto_language_detects_english_for_i() {
        let mut d = tokenize("well i think the plan is good");
        repair(&mut d, &RepairOpts { turkish: false, english: false, french: false, auto: true });
        assert_eq!(d.render(), "Well I think the plan is good");
    }

    #[test]
    fn french_keeps_space_before_marks() {
        let mut d = tokenize("bonjour !");
        repair(&mut d, &RepairOpts { turkish: false, english: false, french: true, auto: false });
        assert_eq!(d.render(), "Bonjour !");
        let mut d = tokenize("merci beaucoup , vous ?");
        repair(&mut d, &RepairOpts { turkish: false, english: false, french: false, auto: true });
        assert_eq!(d.render(), "Merci beaucoup, vous ?");
        assert_eq!(rep("hello !"), "Hello!");
    }

    #[test]
    fn turkish_capital_i() {
        let mut d = tokenize("istanbul güzel. iyi");
        repair(&mut d, &RepairOpts { turkish: true, english: false, french: false, auto: false });
        assert_eq!(d.render(), "İstanbul güzel. İyi");
    }

    fn lb(s: &str) -> String {
        let mut d = tokenize(s);
        apply_line_breaks(&mut d, false);
        d.render()
    }

    #[test]
    fn spoken_line_breaks() {
        assert_eq!(lb("Hello. New line. How are you?"), "Hello.\nHow are you?");
        assert_eq!(lb("Dear John, new line, thanks for the note"), "Dear John,\nThanks for the note");
        assert_eq!(lb("First point. New paragraph. Second point."), "First point.\n\nSecond point.");
        assert_eq!(lb("done new line"), "done\n");
        assert_eq!(lb("x new line new line y"), "x\n\nY");
    }

    #[test]
    fn line_break_phrase_inside_normal_speech_is_kept() {
        for s in ["I added a new line of products", "start the new line of code", "this new line is long", "new line of work"] {
            assert_eq!(lb(s), s);
        }
    }
}
