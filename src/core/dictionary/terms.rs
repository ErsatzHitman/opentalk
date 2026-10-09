//! Dictionary-driven text fixes: exact case-insensitive casing ([`case_fix`]) and conservative
//! fuzzy correction of 1-3 word spans ([`fuzzy_fix`]).
//!
//! Both work from a [`Index`] built once per distinct dictionary and cached, so a dictation pays
//! for matching, not for re-reading hundreds of terms.
use super::common_words::{is_common_english, is_function_word};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, PoisonError};

/// Lower-case one char when it maps to exactly one char (keeps text and term aligned 1:1).
fn fold1(c: char) -> char {
    if c.is_ascii() {
        return c.to_ascii_lowercase();
    }
    let mut it = c.to_lowercase();
    match (it.next(), it.next()) {
        (Some(l), None) => l,
        _ => c,
    }
}

/// True when the char before `start` glues the span to a larger token (path, e-mail, domain,
/// hyphenated compound): such text is left alone.
fn attached_left(chars: &[char], start: usize) -> bool {
    let Some(&prev) = start.checked_sub(1).and_then(|i| chars.get(i)) else {
        return false;
    };
    match prev {
        '.' | '-' | '\u{2011}' | ':' | '\'' | '’' => start >= 2 && chars[start - 2].is_alphanumeric(),
        '/' | '@' | '_' | '\\' | '#' | '$' | '%' | '&' | '+' | '=' | '~' | '*' | '<' | '>' => true,
        _ => false,
    }
}

/// Mirror of [`attached_left`] for the char after `end`; a possessive `'s` does not attach.
fn attached_right(chars: &[char], end: usize) -> bool {
    let Some(&next) = chars.get(end) else { return false };
    let at = |i: usize| chars.get(end + i).copied();
    match next {
        '\'' | '’' => {
            if matches!(at(1), Some('s' | 'S')) && !at(2).is_some_and(char::is_alphanumeric) {
                false
            } else {
                at(1).is_some_and(char::is_alphanumeric)
            }
        }
        '.' | '-' | '\u{2011}' | ':' => at(1).is_some_and(char::is_alphanumeric),
        '/' | '@' | '_' | '\\' | '#' | '$' | '%' | '&' | '+' | '=' | '~' | '*' | '<' | '>' => true,
        _ => false,
    }
}

struct Term {
    text: String,
    folded: Vec<char>,
    /// Second folded char ('\0' for one-char terms): rejects most candidates without a loop.
    second: char,
}

struct FuzzyTerm {
    text: String,
    /// Lower-cased with whitespace removed.
    compact: Vec<char>,
    /// Count of each a-z letter in `compact`, for the bag-distance prefilter.
    bag: [u8; 26],
}

/// Terms longer than this (spaces removed) are not fuzzily matched.
const MAX_TERM_LEN: usize = 48;

/// Everything derived from a dictionary that matching needs.
struct Index {
    terms: Vec<Term>,
    /// Term indices by folded first char, ASCII (index = char) and the rest.
    by_first_ascii: Vec<Vec<usize>>,
    by_first_other: HashMap<char, Vec<usize>>,
    fuzzy: Vec<FuzzyTerm>,
    /// Fuzzy term indices by (first-letter class, compact length): `class * (MAX_TERM_LEN + 1) + len`.
    ascii_buckets: Vec<Vec<usize>>,
    other_buckets: HashMap<(char, usize), Vec<usize>>,
    min_len: usize,
    max_len: usize,
    /// Single-word terms (lower-cased): never "corrected" toward another term.
    known_single: HashSet<String>,
    /// Every term, lower-cased with single spaces.
    known_full: HashSet<String>,
}

/// First letters that STT confuses (homophones) share a class: c/k/q, s/z. `None` for chars that
/// are not a-z.
fn letter_class(c: char) -> Option<usize> {
    let c = match c {
        'c' | 'q' => 'k',
        'z' => 's',
        c => c,
    };
    c.is_ascii_lowercase().then(|| (c as u8 - b'a') as usize)
}

fn bag_of(chars: &[char]) -> [u8; 26] {
    let mut bag = [0u8; 26];
    for &c in chars {
        if c.is_ascii_lowercase() {
            bag[(c as u8 - b'a') as usize] = bag[(c as u8 - b'a') as usize].saturating_add(1);
        }
    }
    bag
}

fn build_index(dictionary: &[String]) -> Index {
    let mut seen: HashSet<Vec<char>> = HashSet::new();
    let mut terms = Vec::new();
    for raw in dictionary {
        let text = raw.trim();
        if text.is_empty() {
            continue;
        }
        let folded: Vec<char> = text.chars().map(fold1).collect();
        if seen.insert(folded.clone()) {
            let second = folded.get(1).copied().unwrap_or('\0');
            terms.push(Term { text: text.to_string(), folded, second });
        }
    }
    // Longest first so "Open AI Codex" wins over "Open AI".
    terms.sort_by(|a, b| b.folded.len().cmp(&a.folded.len()));

    let mut by_first_ascii: Vec<Vec<usize>> = vec![Vec::new(); 128];
    let mut by_first_other: HashMap<char, Vec<usize>> = HashMap::new();
    let mut fuzzy: Vec<FuzzyTerm> = Vec::new();
    let mut known_single = HashSet::new();
    let mut known_full = HashSet::new();
    for (i, t) in terms.iter().enumerate() {
        let f = t.folded[0];
        if f.is_ascii() {
            by_first_ascii[f as usize].push(i);
        } else {
            by_first_other.entry(f).or_default().push(i);
        }

        let spaced = t.folded.iter().collect::<String>().split_whitespace().collect::<Vec<_>>().join(" ");
        if !spaced.contains(' ') {
            known_single.insert(spaced.clone());
        }
        known_full.insert(spaced);
        let compact: Vec<char> = t.folded.iter().copied().filter(|c| !c.is_whitespace()).collect();
        if (4..=MAX_TERM_LEN).contains(&compact.len()) {
            let bag = bag_of(&compact);
            fuzzy.push(FuzzyTerm { text: t.text.clone(), compact, bag });
        }
    }
    let mut ascii_buckets: Vec<Vec<usize>> = vec![Vec::new(); 26 * (MAX_TERM_LEN + 1)];
    let mut other_buckets: HashMap<(char, usize), Vec<usize>> = HashMap::new();
    let (mut min_len, mut max_len) = (usize::MAX, 0);
    for (i, t) in fuzzy.iter().enumerate() {
        let len = t.compact.len();
        min_len = min_len.min(len);
        max_len = max_len.max(len);
        match letter_class(t.compact[0]) {
            Some(class) => ascii_buckets[class * (MAX_TERM_LEN + 1) + len].push(i),
            None => other_buckets.entry((t.compact[0], len)).or_default().push(i),
        }
    }
    Index {
        terms,
        by_first_ascii,
        by_first_other,
        fuzzy,
        ascii_buckets,
        other_buckets,
        min_len,
        max_len,
        known_single,
        known_full,
    }
}

/// The index for `dictionary`, rebuilt only when the dictionary changed since the last call.
fn index(dictionary: &[String]) -> Arc<Index> {
    static CACHE: Mutex<Option<(Vec<String>, Arc<Index>)>> = Mutex::new(None);
    let mut cache = CACHE.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some((words, idx)) = cache.as_ref()
        && words.as_slice() == dictionary
    {
        return Arc::clone(idx);
    }
    let idx = Arc::new(build_index(dictionary));
    *cache = Some((dictionary.to_vec(), Arc::clone(&idx)));
    idx
}

/// Exact case-insensitive matches take the dictionary's casing ("openai" -> "OpenAI").
pub fn case_fix(text: &str, dictionary: &[String]) -> String {
    if dictionary.is_empty() || text.is_empty() {
        return text.to_string();
    }
    let idx = index(dictionary);
    if idx.terms.is_empty() {
        return text.to_string();
    }
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::new();
    let mut copied = 0; // chars[..copied] are already in `out`
    let mut changed = false;
    let mut p = 0;
    while p < chars.len() {
        let f = fold1(chars[p]);
        let cands: &[usize] =
            if f.is_ascii() { &idx.by_first_ascii[f as usize] } else { idx.by_first_other.get(&f).map_or(&[], Vec::as_slice) };
        if cands.is_empty() || (p > 0 && chars[p - 1].is_alphanumeric()) {
            p += 1;
            continue;
        }
        let f2 = chars.get(p + 1).map_or('\0', |&c| fold1(c));
        let hit = Some(cands).and_then(|cands| {
            cands.iter().map(|&i| &idx.terms[i]).filter(|t| t.second == '\0' || t.second == f2).find(|t| {
                let q = p + t.folded.len();
                q <= chars.len()
                    && chars[p..q].iter().zip(&t.folded).all(|(&c, &tc)| fold1(c) == tc)
                    && !chars.get(q).is_some_and(|c| c.is_alphanumeric())
                    && !attached_left(&chars, p)
                    && !attached_right(&chars, q)
            })
        });
        match hit {
            Some(t) => {
                let q = p + t.folded.len();
                if chars[p..q].iter().copied().ne(t.text.chars()) {
                    out.extend(&chars[copied..p]);
                    out.push_str(&t.text);
                    copied = q;
                    changed = true;
                }
                p = q;
            }
            None => p += 1,
        }
    }
    if !changed {
        return text.to_string();
    }
    out.extend(&chars[copied..]);
    out
}

/// Levenshtein distance, giving up (None) once it must exceed `max`. Inputs are at most
/// [`MAX_TERM_LEN`] + 2 chars, so the two DP rows live on the stack.
fn lev_within(a: &[char], b: &[char], max: usize) -> Option<usize> {
    if a.len().abs_diff(b.len()) > max || b.len() > MAX_TERM_LEN + 2 {
        return None;
    }
    let mut prev = [0usize; MAX_TERM_LEN + 3];
    let mut cur = [0usize; MAX_TERM_LEN + 3];
    for (j, p) in prev.iter_mut().enumerate().take(b.len() + 1) {
        *p = j;
    }
    for (i, &ca) in a.iter().enumerate() {
        cur[0] = i + 1;
        let mut row_min = cur[0];
        for (j, &cb) in b.iter().enumerate() {
            let cost = usize::from(ca != cb);
            cur[j + 1] = (prev[j] + cost).min(prev[j + 1] + 1).min(cur[j] + 1);
            row_min = row_min.min(cur[j + 1]);
        }
        if row_min > max {
            return None;
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    (prev[b.len()] <= max).then_some(prev[b.len()])
}

/// Conservative fuzzy correction of 1-3 token spans toward dictionary terms.
///
/// A span is 1-3 consecutive words separated only by whitespace. It is replaced when its letters,
/// joined, are within edit distance 1 (2 for terms of 9+ characters) of a term with the spaces
/// removed. Guards: single common English words are never touched, nor anything that already is
/// (or contains) another dictionary term; very short words and terms are skipped; the first letter
/// must agree (c/k/q and s/z count as one); ties between terms change nothing.
pub fn fuzzy_fix(text: &str, dictionary: &[String]) -> String {
    if dictionary.is_empty() || text.is_empty() {
        return text.to_string();
    }
    let idx = index(dictionary);
    if idx.fuzzy.is_empty() {
        return text.to_string();
    }
    let chars: Vec<char> = text.chars().collect();
    // Word pieces: maximal alphanumeric runs, as char ranges.
    let mut words: Vec<(usize, usize)> = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i].is_alphanumeric() {
            let s = i;
            while i < chars.len() && chars[i].is_alphanumeric() {
                i += 1;
            }
            words.push((s, i));
        } else {
            i += 1;
        }
    }
    // Words that can never be part of a rejoined multi-word span ("is", "the", "and", ...).
    let func: Vec<bool> = words
        .iter()
        .map(|&(a, b)| {
            let n = b - a;
            if n >= 4 || !chars[a..b].iter().all(char::is_ascii) {
                return false;
            }
            let mut buf = [0u8; 3];
            for (k, c) in chars[a..b].iter().enumerate() {
                buf[k] = c.to_ascii_lowercase() as u8;
            }
            std::str::from_utf8(&buf[..n]).is_ok_and(is_function_word)
        })
        .collect();
    let gap_is_space = |a_end: usize, b_start: usize| {
        b_start > a_end && chars[a_end..b_start].iter().all(|c| c.is_whitespace() && *c != '\n' && *c != '\r')
    };

    let mut edits: Vec<(usize, usize, usize)> = Vec::new(); // (char start, char end, term index)
    let mut compact: Vec<char> = Vec::with_capacity(MAX_TERM_LEN + 4);
    let mut w = 0;
    while w < words.len() {
        let mut best: Option<(usize, usize, usize)> = None; // (dist, span words, term)
        for span in (1..=3).rev() {
            if w + span > words.len() {
                continue;
            }
            let (start, end) = (words[w].0, words[w + span - 1].1);
            let single = span == 1;
            let total: usize = (0..span).map(|k| words[w + k].1 - words[w + k].0).sum();
            if total < if single { 4 } else { 5 } || total + 2 < idx.min_len || total > idx.max_len + 2 {
                continue;
            }
            if !single
                && ((0..span).any(|k| func[w + k] || words[w + k].1 - words[w + k].0 < 3)
                    || (1..span).any(|k| !gap_is_space(words[w + k - 1].1, words[w + k].0)))
            {
                continue;
            }
            if attached_left(&chars, start) || attached_right(&chars, end) {
                continue;
            }
            compact.clear();
            for k in 0..span {
                compact.extend(chars[words[w + k].0..words[w + k].1].iter().map(|&c| fold1(c)));
            }
            if compact.iter().all(|c| c.is_ascii_digit()) {
                continue;
            }
            let bag = bag_of(&compact);
            let mut found: Option<(usize, usize, bool)> = None; // (dist, term, tie)
            let first = compact[0];
            let class = letter_class(first);
            for len in compact.len().saturating_sub(2)..=(compact.len() + 2).min(MAX_TERM_LEN) {
                // Candidates share the first letter (class): STT rarely gets that wrong, and it
                // keeps the search small for big dictionaries.
                let bucket: &[usize] = match class {
                    Some(c) => &idx.ascii_buckets[c * (MAX_TERM_LEN + 1) + len],
                    None => idx.other_buckets.get(&(first, len)).map_or(&[], Vec::as_slice),
                };
                for &ti in bucket {
                    let t = &idx.fuzzy[ti];
                    let max_d = if t.compact.len() >= 9 { 2 } else { 1 };
                    // Edit distance <= d implies the letter counts differ by at most 2d in total.
                    let bag_dist: usize = bag.iter().zip(&t.bag).map(|(&a, &b)| usize::from(a.abs_diff(b))).sum();
                    if bag_dist > 2 * max_d {
                        continue;
                    }
                    let Some(d) = lev_within(&compact, &t.compact, max_d) else { continue };
                    if d == 0 && (single || chars[start..end].iter().copied().eq(t.text.chars())) {
                        continue;
                    }
                    match found {
                        Some((fd, ft, _)) if d > fd || (d == fd && ft == ti) => {}
                        Some((fd, _, _)) if d == fd => found = Some((d, ti, true)),
                        _ => found = Some((d, ti, false)),
                    }
                }
            }
            let Some((d, ti, false)) = found else { continue };
            // Guards run only for the rare span that has a candidate.
            let piece =
                |k: usize| -> String { chars[words[w + k].0..words[w + k].1].iter().map(|&c| fold1(c)).collect() };
            if single {
                let p = piece(0);
                if is_common_english(&p) || idx.known_single.contains(&p) {
                    continue;
                }
            } else {
                let pieces: Vec<String> = (0..span).map(piece).collect();
                // "rust is" is two ordinary words, not a split "Rusts": no function words in a span.
                if pieces.iter().any(|p| is_function_word(p) || idx.known_single.contains(p))
                    || idx.known_full.contains(&pieces.join(" "))
                {
                    continue;
                }
            }
            if best.is_none_or(|(bd, ..)| d < bd) {
                best = Some((d, span, ti));
            }
        }
        if let Some((_, span, ti)) = best {
            edits.push((words[w].0, words[w + span - 1].1, ti));
            w += span;
        } else {
            w += 1;
        }
    }
    if edits.is_empty() {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len() + 16);
    let mut cursor = 0;
    for (s, e, ti) in edits {
        out.extend(&chars[cursor..s]);
        out.push_str(&idx.fuzzy[ti].text);
        cursor = e;
    }
    out.extend(&chars[cursor..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(words: &[&str]) -> Vec<String> {
        words.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn case_fix_uses_dictionary_casing() {
        let dict = d(&["OpenAI", "iPhone", "Node.js", "New York"]);
        assert_eq!(case_fix("i use openai and IPHONE daily", &dict), "i use OpenAI and iPhone daily");
        assert_eq!(case_fix("openai's api, new york, node.js.", &dict), "OpenAI's api, New York, Node.js.");
    }

    #[test]
    fn case_fix_respects_boundaries_and_compounds() {
        let dict = d(&["OpenAI", "Rust"]);
        assert_eq!(case_fix("trusty openaiish rusty", &dict), "trusty openaiish rusty");
        assert_eq!(case_fix("see openai.com/docs or me@openai.com", &dict), "see openai.com/docs or me@openai.com");
        assert_eq!(case_fix("the open-ai-compatible rust-like", &dict), "the open-ai-compatible rust-like");
        assert_eq!(case_fix("RUST!", &dict), "Rust!");
    }

    #[test]
    fn case_fix_single_letter_and_two_letter_terms() {
        let dict = d(&["R", "AI", "C++"]);
        assert_eq!(case_fix("i use r and ai, not c++ today", &dict), "i use R and AI, not C++ today");
        assert_eq!(case_fix("rain said air", &dict), "rain said air");
        assert_eq!(case_fix("ai", &dict), "AI");
    }

    #[test]
    fn case_fix_noop_cases() {
        assert_eq!(case_fix("hello", &[]), "hello");
        assert_eq!(case_fix("hello", &d(&["", "  "])), "hello");
        assert_eq!(case_fix("", &d(&["A"])), "");
    }

    #[test]
    fn case_fix_longest_term_wins() {
        let dict = d(&["Open", "Open AI Codex"]);
        assert_eq!(case_fix("try open ai codex now", &dict), "try Open AI Codex now");
    }

    #[test]
    fn fuzzy_fixes_close_spellings() {
        let dict = d(&["OpenWhispr", "Kubernetes", "Parakeet"]);
        assert_eq!(fuzzy_fix("I use open whisper daily", &dict), "I use OpenWhispr daily");
        assert_eq!(fuzzy_fix("deploy to cuberneties now", &dict), "deploy to Kubernetes now");
        assert_eq!(fuzzy_fix("the parakeat model", &dict), "the Parakeet model");
        assert_eq!(fuzzy_fix("openwhisper is great", &dict), "OpenWhispr is great");
    }

    #[test]
    fn fuzzy_joins_split_terms() {
        let dict = d(&["GitHub", "OpenWhispr"]);
        assert_eq!(fuzzy_fix("push to git hub please", &dict), "push to GitHub please");
        assert_eq!(fuzzy_fix("open whispr works", &dict), "OpenWhispr works");
    }

    #[test]
    fn fuzzy_leaves_common_words_and_unrelated_text() {
        let dict = d(&["Rusts", "Tier", "Parakeet"]);
        assert_eq!(fuzzy_fix("their rust is best", &dict), "their rust is best");
        assert_eq!(fuzzy_fix("a perfect parrot", &dict), "a perfect parrot");
        assert_eq!(fuzzy_fix("they said hello", &dict), "they said hello");
    }

    #[test]
    fn fuzzy_never_touches_other_dictionary_terms() {
        let dict = d(&["Parakeet", "Parakeets", "Whisper"]);
        // "parakeets" is itself a term, so it must not be pulled toward "Parakeet".
        assert_eq!(fuzzy_fix("two parakeets", &dict), "two parakeets");
        // Whisper is a term; "whisper" alone is left as is (case_fix owns casing).
        assert_eq!(fuzzy_fix("a whisper", &dict), "a whisper");
    }

    #[test]
    fn fuzzy_ties_change_nothing() {
        let dict = d(&["Mellow", "Mallow"]);
        assert_eq!(fuzzy_fix("a mxllow sound", &dict), "a mxllow sound");
        assert_eq!(fuzzy_fix("a mellaw sound", &dict), "a Mellow sound");
    }

    #[test]
    fn fuzzy_skips_short_and_attached_text() {
        let dict = d(&["Rust", "OpenWhispr"]);
        assert_eq!(fuzzy_fix("a rest area", &dict), "a rest area");
        assert_eq!(fuzzy_fix("see open-whisper/docs and open.whisper", &dict), "see open-whisper/docs and open.whisper");
    }

    #[test]
    fn fuzzy_does_not_cross_punctuation() {
        let dict = d(&["OpenWhispr"]);
        assert_eq!(fuzzy_fix("open, whisper", &dict), "open, whisper");
    }

    #[test]
    fn fuzzy_keeps_possessive() {
        let dict = d(&["OpenWhispr"]);
        assert_eq!(fuzzy_fix("openwhisper's roadmap", &dict), "OpenWhispr's roadmap");
    }
}
