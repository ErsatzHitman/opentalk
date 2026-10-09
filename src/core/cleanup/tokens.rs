//! Lossless tokenizer for the cleanup pipeline: words and single punctuation tokens, each
//! remembering the whitespace that preceded it, so untouched text renders back byte-for-byte
//! (modulo whitespace normalisation) and removals can fix up spacing locally.
use std::borrow::Cow;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Ws {
    None,
    Space,
    /// A single non-breaking / thin space, kept as typed (French typography).
    Hard(char),
    Newlines(u8),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Word,
    Punct,
}

#[derive(Clone, Debug)]
pub struct Tok<'a> {
    pub text: Cow<'a, str>,
    /// Lower-cased `text` (borrows when there is nothing to lower).
    pub lc: Cow<'a, str>,
    pub ws: Ws,
    pub kind: Kind,
}

pub fn lower(s: &str) -> Cow<'_, str> {
    if s.is_ascii() {
        if s.bytes().any(|b| b.is_ascii_uppercase()) { Cow::Owned(s.to_ascii_lowercase()) } else { Cow::Borrowed(s) }
    } else if s.chars().any(|c| c.is_uppercase() || c == '’') {
        // Curly apostrophes compare as straight ones ("I’ve" == "i've").
        Cow::Owned(s.to_lowercase().replace('’', "'"))
    } else {
        Cow::Borrowed(s)
    }
}

impl<'a> Tok<'a> {
    pub fn borrowed(s: &'a str, ws: Ws, kind: Kind) -> Self {
        Tok { text: Cow::Borrowed(s), lc: lower(s), ws, kind }
    }

    pub fn set_text(&mut self, s: String) {
        self.lc = Cow::Owned(lower(&s).into_owned());
        self.text = Cow::Owned(s);
    }

    pub fn is_word(&self) -> bool {
        self.kind == Kind::Word
    }

    fn punct(&self) -> Option<char> {
        if self.kind == Kind::Punct { self.text.chars().next() } else { None }
    }

    pub fn is_comma(&self) -> bool {
        matches!(self.punct(), Some(',' | '、' | '，'))
    }

    /// `. ! ?` (and fullwidth forms) as a single token; ellipses are separate.
    pub fn is_sentence_end(&self) -> bool {
        matches!(self.punct(), Some('.' | '!' | '?' | '。' | '！' | '？')) && !self.is_ellipsis()
    }

    pub fn is_ellipsis(&self) -> bool {
        self.kind == Kind::Punct && (self.text == "…" || (self.text.len() >= 3 && self.text.bytes().all(|b| b == b'.')))
    }

    pub fn is_dash(&self) -> bool {
        self.kind == Kind::Punct && matches!(self.text.chars().next(), Some('-' | '–' | '—' | '‐' | '‑'))
    }

    /// Punctuation that attaches to the word before it.
    pub fn is_closing(&self) -> bool {
        matches!(
            self.punct(),
            Some(',' | '.' | ';' | ':' | '!' | '?' | '…' | ')' | ']' | '}' | '»' | '”' | '。' | '、' | '，' | '！' | '？' | '；' | '：' | '）' | '」' | '』' | '】')
        )
    }

    /// Punctuation that attaches to the word after it.
    pub fn is_opening(&self) -> bool {
        matches!(self.punct(), Some('(' | '[' | '{' | '«' | '“' | '¿' | '¡' | '（' | '「' | '『' | '【'))
    }

    /// First char of the text.
    pub fn first(&self) -> Option<char> {
        self.text.chars().next()
    }

    pub fn last(&self) -> Option<char> {
        self.text.chars().next_back()
    }

    /// True for a token made only of digits and number punctuation.
    pub fn is_numeric(&self) -> bool {
        self.is_word() && self.text.chars().any(|c| c.is_ascii_digit()) && self.text.chars().all(|c| c.is_ascii_digit() || matches!(c, '.' | ',' | ':' | '%' | '$' | '€' | '£' | '-' | '+'))
    }
}

pub struct Doc<'a> {
    pub toks: Vec<Tok<'a>>,
    /// Newlines after the last token (a spoken "new line" at the end of the text).
    pub trailing_nl: u8,
}

fn is_punct_char(c: char) -> bool {
    matches!(
        c,
        ',' | '.' | ';' | ':' | '!' | '?' | '…' | '。' | '、' | '，' | '！' | '？' | '；' | '：'
            | '"' | '\'' | '“' | '”' | '‘' | '’' | '(' | ')' | '[' | ']' | '{' | '}' | '«' | '»'
            | '¿' | '¡' | '-' | '–' | '—' | '‐' | '‑' | '（' | '）' | '「' | '」' | '『' | '』' | '【' | '】'
    )
}

fn is_hard_space(c: char) -> bool {
    matches!(c, '\u{A0}' | '\u{202F}' | '\u{2009}')
}

/// Scripts written without spaces between words.
pub fn is_spaceless(c: char) -> bool {
    matches!(c as u32, 0x0E00..=0x0EFF | 0x1780..=0x17FF | 0x3040..=0x30FF | 0x3400..=0x9FFF | 0xF900..=0xFAFF | 0x20000..=0x2FFFF)
}

pub fn tokenize(s: &str) -> Doc<'_> {
    let cs: Vec<(usize, char)> = s.char_indices().collect();
    let n = cs.len();
    let byte = |i: usize| if i < n { cs[i].0 } else { s.len() };
    let mut toks: Vec<Tok<'_>> = Vec::with_capacity(n / 5 + 4);
    let mut ws = Ws::None;
    let mut i = 0;
    while i < n {
        let c = cs[i].1;
        if c.is_whitespace() {
            let start = i;
            while i < n && cs[i].1.is_whitespace() {
                i += 1;
            }
            ws = if i - start == 1 && is_hard_space(c) { Ws::Hard(c) } else { Ws::Space };
            continue;
        }
        if is_punct_char(c) {
            let mut j = i + 1;
            match c {
                '.' => {
                    while j < n && cs[j].1 == '.' {
                        j += 1;
                    }
                    if j - i == 2 {
                        // ".." is a typo for ".": two adjacent full stops, collapsed by the repair pass.
                        toks.push(Tok::borrowed(&s[byte(i)..byte(i + 1)], ws, Kind::Punct));
                        toks.push(Tok::borrowed(&s[byte(i + 1)..byte(j)], Ws::None, Kind::Punct));
                        ws = Ws::None;
                        i = j;
                        continue;
                    }
                }
                '-' => {
                    while j < n && cs[j].1 == '-' {
                        j += 1;
                    }
                }
                _ => {}
            }
            toks.push(Tok::borrowed(&s[byte(i)..byte(j)], ws, Kind::Punct));
            ws = Ws::None;
            i = j;
            continue;
        }
        // Word run: everything up to whitespace or punctuation, with inner connectors kept
        // ("don't", "well-known", "3.5", "node.js", "10:30", "1,000").
        let mut j = i + 1;
        while j < n {
            let d = cs[j].1;
            if d.is_whitespace() {
                break;
            }
            if is_punct_char(d) {
                let prev = cs[j - 1].1;
                let next = cs.get(j + 1).map(|p| p.1);
                let inner = match d {
                    '\'' | '’' | '-' | '‑' | '.' => prev.is_alphanumeric() && next.is_some_and(char::is_alphanumeric),
                    ',' | ':' => prev.is_ascii_digit() && next.is_some_and(|c| c.is_ascii_digit()),
                    _ => false,
                };
                if !inner {
                    break;
                }
            }
            j += 1;
        }
        toks.push(Tok::borrowed(&s[byte(i)..byte(j)], ws, Kind::Word));
        ws = Ws::None;
        i = j;
    }
    Doc { toks, trailing_nl: 0 }
}

impl<'a> Doc<'a> {
    pub fn render(&self) -> String {
        let mut out = String::with_capacity(self.toks.iter().map(|t| t.text.len() + 1).sum::<usize>() + 2);
        for (i, t) in self.toks.iter().enumerate() {
            if i > 0 {
                match t.ws {
                    Ws::None => {}
                    Ws::Space => out.push(' '),
                    Ws::Hard(c) => out.push(c),
                    Ws::Newlines(n) => out.extend(std::iter::repeat_n('\n', n as usize)),
                }
            }
            out.push_str(&t.text);
        }
        out.extend(std::iter::repeat_n('\n', self.trailing_nl as usize));
        out
    }

    /// Drop flagged tokens. A word glued to a word by the removal gets its space back.
    pub fn remove(&mut self, del: &[bool]) {
        if !del.iter().any(|&d| d) {
            return;
        }
        let old = std::mem::take(&mut self.toks);
        let mut out: Vec<Tok<'a>> = Vec::with_capacity(old.len());
        let mut gap = false;
        for (i, mut t) in old.into_iter().enumerate() {
            if del.get(i).copied().unwrap_or(false) {
                gap = true;
                continue;
            }
            match out.last() {
                None => t.ws = Ws::None,
                Some(prev) if gap => {
                    // After an opening quote or bracket the next word hugs it.
                    let after_opener = prev.is_opening()
                        || (matches!(&*prev.text, "\"" | "'" | "“" | "‘") && (out.len() == 1 || prev.ws != Ws::None));
                    if after_opener {
                        t.ws = Ws::None;
                    }
                    let glued = t.ws == Ws::None
                        && t.is_word()
                        && prev.is_word()
                        && prev.last().is_some_and(|c| !is_spaceless(c))
                        && t.first().is_some_and(|c| !is_spaceless(c));
                    if glued {
                        t.ws = Ws::Space;
                    }
                }
                Some(_) => {}
            }
            gap = false;
            out.push(t);
        }
        self.toks = out;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rt(s: &str) -> String {
        tokenize(s).render()
    }

    #[test]
    fn round_trips_ordinary_text() {
        for s in [
            "Hello, world.",
            "It's well-known that 3.5 > 3, e.g. node.js at 10:30 costs $1,000.",
            "He said (quietly): \"no way\"... really?!",
            "日本語、テスト。",
            "C++ and @home #1 50% off",
            "a -- b - c — d",
        ] {
            assert_eq!(rt(s), s);
        }
    }

    #[test]
    fn normalises_whitespace_but_keeps_hard_spaces() {
        assert_eq!(rt("  a \t b\n\nc  "), "a b c");
        assert_eq!(rt("Bonjour\u{202F}!"), "Bonjour\u{202F}!");
    }

    #[test]
    fn splits_words_and_punctuation() {
        let d = tokenize("Don't stop, e.g. U.S. now...");
        let words: Vec<&str> = d.toks.iter().map(|t| &*t.text).collect();
        assert_eq!(words, ["Don't", "stop", ",", "e.g", ".", "U.S", ".", "now", "..."]);
        assert!(d.toks[8].is_ellipsis());
        assert!(d.toks[2].is_comma() && d.toks[4].is_sentence_end());
    }

    #[test]
    fn double_dot_is_two_full_stops() {
        let d = tokenize("hi..");
        assert_eq!(d.toks.len(), 3);
        assert!(d.toks[1].is_sentence_end() && d.toks[2].is_sentence_end());
    }

    #[test]
    fn remove_restores_missing_space() {
        let mut d = tokenize("think,uh,we go");
        d.remove(&[false, true, true, true, false, false]);
        assert_eq!(d.render(), "think we go");
        let mut d = tokenize("a uh, b");
        d.remove(&[false, true, true, false]);
        assert_eq!(d.render(), "a b");
    }
}
