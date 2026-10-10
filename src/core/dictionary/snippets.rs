//! Snippet expansion: a port of OpenWhispr's `src/utils/snippets.ts` (longest trigger first,
//! Unicode word boundaries, case-insensitive with Turkish i/İ/ı handling) minus its
//! trailing-punctuation quirk (see [`trailing_skip`]).
use super::nfc::nfc;
use crate::core::config::Snippet;
use std::collections::HashMap;
use std::ops::Range;
use std::sync::{Arc, Mutex, PoisonError};

/// One trigger character. `i` matches i/I/İ, `ı` matches ı/I/i (the Turkish readings), anything
/// else matches by simple case folding.
#[derive(Clone, Copy)]
enum Pat {
    Dotted,
    Dotless,
    Lit(char),
}

struct Entry {
    pat: Vec<Pat>,
    len: usize,
}

struct Matcher {
    entries: Vec<Entry>,
    /// Entry indices (longest first) by the folded ASCII char they can start with; a text char
    /// outside ASCII tries every entry.
    by_first: Vec<Vec<usize>>,
    /// Lower-cased trigger -> replacement.
    replacements: HashMap<String, String>,
}

/// `İ` has no simple case folding; folding it to a plain `i` gives keys a canonical form.
fn fold_capital_i_dot(s: &str) -> String {
    s.replace('İ', "i")
}

/// Simple case folding, close enough to the JS `/iu` canonicalisation for trigger matching.
fn fold(c: char) -> char {
    if c.is_ascii() {
        return c.to_ascii_lowercase();
    }
    match c {
        'ı' | 'İ' => c,
        _ => {
            let mut up = c.to_uppercase();
            let u = match (up.next(), up.next()) {
                (Some(u), None) => u,
                _ => return c,
            };
            let mut low = u.to_lowercase();
            match (low.next(), low.next()) {
                (Some(l), None) => l,
                _ => c,
            }
        }
    }
}

fn pat_matches(p: Pat, t: char) -> bool {
    match p {
        Pat::Dotted => matches!(fold(t), 'i' | 'İ'),
        Pat::Dotless => matches!(fold(t), 'ı' | 'i'),
        Pat::Lit(c) => fold(t) == fold(c),
    }
}

fn is_mark(c: char) -> bool {
    matches!(c as u32, 0x300..=0x36F | 0x1AB0..=0x1AFF | 0x1DC0..=0x1DFF | 0x20D0..=0x20FF | 0xFE20..=0xFE2F)
}

fn is_format(c: char) -> bool {
    matches!(c as u32, 0x200B..=0x200F | 0x202A..=0x202E | 0x2060..=0x2064 | 0xFE00..=0xFE0F)
}

/// JS `[\s\p{P}\p{S}]`: whitespace, punctuation and symbols. Letters, numbers, marks and
/// invisible format characters are not boundaries.
fn is_boundary(c: char) -> bool {
    c.is_whitespace()
        || c == '\u{FEFF}'
        || !(c.is_alphanumeric() || is_mark(c) || c.is_control() || is_format(c))
}

fn build(snippets: &[Snippet]) -> Option<Matcher> {
    let mut order: Vec<String> = Vec::new();
    let mut replacements: HashMap<String, String> = HashMap::new();
    for s in snippets {
        let composed = nfc(s.trigger.trim());
        let folded = fold_capital_i_dot(&composed);
        let key = folded.to_lowercase();
        if key.is_empty() {
            continue;
        }
        if replacements.insert(key.clone(), s.replacement.clone()).is_none() {
            order.push(key);
        }
        // An uppercase I may mean Turkish ı as well as English i: register both readings;
        // an explicit trigger wins over the variant.
        let dotless = folded.replace('I', "ı").to_lowercase();
        if !replacements.contains_key(&dotless) {
            replacements.insert(dotless.clone(), s.replacement.clone());
            order.push(dotless);
        }
    }
    if order.is_empty() {
        return None;
    }
    let mut entries: Vec<Entry> = order
        .iter()
        .map(|key| {
            let pat: Vec<Pat> = key
                .chars()
                .map(|c| match c {
                    'i' => Pat::Dotted,
                    'ı' => Pat::Dotless,
                    c => Pat::Lit(c),
                })
                .collect();
            let len = key.encode_utf16().count();
            Entry { pat, len }
        })
        .collect();
    // Longest first so "investor ask" wins over a shorter "ask"; stable for equal lengths.
    entries.sort_by(|a, b| b.len.cmp(&a.len));
    let mut by_first: Vec<Vec<usize>> = vec![Vec::new(); 128];
    for (i, e) in entries.iter().enumerate() {
        let f = match e.pat.first() {
            Some(Pat::Dotted | Pat::Dotless) => 'i',
            Some(Pat::Lit(c)) => fold(*c),
            None => continue,
        };
        if f.is_ascii() {
            by_first[f as usize].push(i);
        }
    }
    Some(Matcher { entries, by_first, replacements })
}

/// The matcher for `snippets`, rebuilt only when the list changed since the last call.
fn matcher(snippets: &[Snippet]) -> Option<Arc<Matcher>> {
    static CACHE: Mutex<Option<(Vec<Snippet>, Option<Arc<Matcher>>)>> = Mutex::new(None);
    let mut cache = CACHE.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some((list, m)) = cache.as_ref()
        && list.as_slice() == snippets
    {
        return m.clone();
    }
    let m = build(snippets).map(Arc::new);
    *cache = Some((snippets.to_vec(), m.clone()));
    m
}

/// Scan `chars` left to right; matches never overlap and lookbehind sees the original text.
/// A match is not proof of a replacement: the pattern case-folds the Unicode way while the keys
/// are `to_lowercase`d, so `[ıI]mza` matches a plain "imza", which is no key at all.
fn scan(m: &Matcher, chars: &[char]) -> Vec<(usize, usize, Option<String>)> {
    let mut out = Vec::new();
    let mut p = 0;
    while p < chars.len() {
        if p > 0 && !is_boundary(chars[p - 1]) {
            p += 1;
            continue;
        }
        let all: Vec<usize>;
        let candidates: &[usize] = if chars[p].is_ascii() {
            &m.by_first[fold(chars[p]) as usize]
        } else {
            all = (0..m.entries.len()).collect();
            &all
        };
        let mut found = None;
        for &ei in candidates {
            let e = &m.entries[ei];
            let q = p + e.pat.len();
            if q > chars.len() {
                continue;
            }
            if !e.pat.iter().zip(&chars[p..q]).all(|(&pt, &t)| pat_matches(pt, t)) {
                continue;
            }
            if q < chars.len() && !is_boundary(chars[q]) {
                continue;
            }
            found = Some(q);
            break;
        }
        match found {
            Some(q) => {
                let matched: String = chars[p..q].iter().collect();
                let folded = fold_capital_i_dot(&matched);
                let rep = m
                    .replacements
                    .get(&folded.to_lowercase())
                    .or_else(|| m.replacements.get(&folded.replace('I', "ı").to_lowercase()))
                    .cloned();
                out.push((p, q, rep));
                p = q;
            }
            None => p += 1,
        }
    }
    out
}

/// True for a replacement that is a URL, e-mail address or path: a sentence period glued to the
/// end would corrupt it.
fn looks_like_token(rep: &str) -> bool {
    if rep.is_empty() || rep.chars().any(char::is_whitespace) {
        return false;
    }
    if rep.contains("://") || rep.contains('@') || rep.contains('/') {
        return true;
    }
    let chars: Vec<char> = rep.chars().collect();
    chars.windows(3).any(|w| w[1] == '.' && w[0].is_alphanumeric() && w[2].is_alphanumeric())
}

/// Punctuation right after a trigger that Whisper added as sentence punctuation and that must not
/// follow the expansion: OpenWhispr appended it verbatim, giving "Regards,\nUmut." for a signature,
/// "Thanks!." or a URL with a full stop glued on. Returns how many chars of `after` to drop (0 or 1).
fn trailing_skip(rep: &str, after: &[char]) -> usize {
    let Some(&next) = after.first() else { return 0 };
    // Leave ellipses and runs of punctuation alone.
    if after.get(1).is_some_and(|c| ".,;:!?…".contains(*c)) {
        return 0;
    }
    let last = rep.trim_end().chars().next_back();
    let ends_terminal = last.is_some_and(|c| ".!?…".contains(c));
    let ends_pause = last.is_some_and(|c| ",;:".contains(c));
    if ends_terminal && ".,;".contains(next) {
        return 1;
    }
    if ends_pause && ".,;:".contains(next) {
        return 1;
    }
    if next == '.' && (rep.contains('\n') || looks_like_token(rep.trim())) {
        return 1;
    }
    0
}

/// Expand snippet triggers (longest first, Unicode word boundaries, case-insensitive, Turkish i).
pub fn expand_snippets(text: &str, snippets: &[Snippet]) -> String {
    if text.is_empty() || snippets.is_empty() {
        return text.to_string();
    }
    let Some(matcher) = matcher(snippets) else {
        return text.to_string();
    };
    // NFC so a decomposed "I" + U+0307 in the transcript recombines into İ.
    let composed = nfc(text);
    let chars: Vec<char> = composed.chars().collect();
    let hits = scan(&matcher, &chars);
    if hits.iter().all(|h| h.2.is_none()) {
        return composed.into_owned();
    }
    let mut out = String::with_capacity(composed.len() + 32);
    let mut cursor = 0;
    for (start, end, rep) in hits {
        let Some(rep) = rep else { continue };
        out.extend(&chars[cursor..start]);
        out.push_str(&rep);
        cursor = end + trailing_skip(&rep, &chars[end..]);
    }
    out.extend(&chars[cursor..]);
    out
}

/// Byte ranges of every trigger occurrence that [`expand_snippets`] would replace, matched against
/// `text` exactly as given. (Wake-word style callers must ignore text that only opens a trigger.)
#[allow(dead_code)] // wake-word API kept for callers; pinned by tests
pub fn trigger_ranges(text: &str, snippets: &[Snippet]) -> Vec<Range<usize>> {
    if text.is_empty() {
        return Vec::new();
    }
    let Some(matcher) = matcher(snippets) else {
        return Vec::new();
    };
    let offsets: Vec<usize> = text.char_indices().map(|(i, _)| i).chain([text.len()]).collect();
    let chars: Vec<char> = text.chars().collect();
    scan(&matcher, &chars)
        .into_iter()
        .filter(|h| h.2.is_some())
        .map(|(s, e, _)| offsets[s]..offsets[e])
        .collect()
}
