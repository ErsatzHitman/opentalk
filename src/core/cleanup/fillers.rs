//! Stages 4-6: hesitation fillers, context fillers ("like", "you know") and the aggressive extras,
//! plus the comma bookkeeping that keeps the remaining text well punctuated.
use super::lists::*;
use super::tokens::{lower, Doc, Tok, Ws};

pub struct Opts<'a> {
    /// "um" / "er" are fillers (English text).
    pub en: bool,
    /// Strip Japanese / Chinese hesitation strings from unspaced runs.
    pub ja: bool,
    pub zh: bool,
    pub aggressive: bool,
    /// The user's dictionary: a term that looks like a filler must survive ("Hmm" the band).
    pub dictionary: &'a [String],
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CandKind {
    Like,
    YouKnow,
    Aggressive,
}

struct Cand {
    start: usize,
    end: usize,
    kind: CandKind,
}

fn contiguous_word(toks: &[Tok<'_>], i: usize) -> bool {
    toks.get(i).is_some_and(|t| t.is_word() && t.ws != Ws::None)
}

fn find_candidates(toks: &[Tok<'_>], aggressive: bool) -> Vec<Cand> {
    let mut out = Vec::new();
    for (i, t) in toks.iter().enumerate() {
        if !t.is_word() {
            continue;
        }
        let next_lc = || toks.get(i + 1).filter(|_| contiguous_word(toks, i + 1)).map(|n| &*n.lc);
        let cand = match &*t.lc {
            "like" => Some((1, CandKind::Like)),
            "y'know" | "y’know" => Some((1, CandKind::YouKnow)),
            "you" | "ya" if next_lc() == Some("know") => Some((2, CandKind::YouKnow)),
            "basically" | "actually" | "literally" if aggressive => Some((1, CandKind::Aggressive)),
            "i" if aggressive && next_lc() == Some("mean") => Some((2, CandKind::Aggressive)),
            "sort" | "kind" if aggressive && next_lc() == Some("of") => Some((2, CandKind::Aggressive)),
            _ => None,
        };
        if let Some((len, kind)) = cand {
            out.push(Cand { start: i, end: i + len, kind });
        }
    }
    out
}

/// A flat sentence ending that leaves "you know" / "basically" a trailing tag, not a question.
fn ends_flat(t: &Tok<'_>) -> bool {
    matches!(&*t.text, "." | "。" | "!" | "！") || t.is_ellipsis()
}

/// Everything before `s` in its sentence has been removed (or there is nothing).
fn sentence_start_eff(toks: &[Tok<'_>], removed: &[bool], s: usize) -> bool {
    let mut j = s;
    // Bounded walk: a pathological run of fillers must not make this quadratic.
    let mut steps = 0;
    while j > 0 && (removed[j - 1] || (toks[j - 1].is_comma() && j >= 2 && removed[j - 2])) {
        j -= 1;
        steps += 1;
        if steps > 64 {
            return false;
        }
    }
    while j > 0 && toks[j - 1].is_opening() {
        j -= 1;
    }
    j == 0 || toks[j - 1].is_sentence_end()
}

fn eval(c: &Cand, toks: &[Tok<'_>], removed: &[bool]) -> bool {
    let prev = c.start.checked_sub(1).map(|p| &toks[p]);
    let next = toks.get(c.end);
    let comma_before = prev.is_some_and(Tok::is_comma);
    let comma_after = next.is_some_and(Tok::is_comma);
    let prev_word = prev.filter(|t| t.is_word()).map(|t| &*t.lc);
    let next_word = next.filter(|t| t.is_word()).map(|t| &*t.lc);

    // A: comma-bounded (", like,", ", you know," and, for tags, a flat end ", you know.").
    let closes = match c.kind {
        CandKind::Like => comma_after,
        _ => comma_after || next.is_none() || next.is_some_and(ends_flat),
    };
    if comma_before && closes {
        return true;
    }
    // B: sentence-initial before a comma ("Like, I don't know.").
    if comma_after && sentence_start_eff(toks, removed, c.start) {
        return true;
    }
    if c.kind == CandKind::Like {
        // A2: "going to like, check" / "and like, I": a pause after a modal or conjunction.
        if comma_after && prev_word.is_some_and(|w| is_modal_or_to(w) || is_conjunction(w)) {
            return true;
        }
        // D: modal / "to" + like + bare verb ("should like ship"). "would like to ..." stays.
        if prev_word.is_some_and(is_modal_or_to) && next_word.is_some_and(is_bare_verb) {
            return true;
        }
        // E: copula + like + intensifier or number ("was like totally", "is like 30 minutes").
        if prev_word.is_some_and(is_copula)
            && next.is_some_and(|n| n.is_word() && (is_intensifier(&n.lc) || n.first().is_some_and(|f| f.is_ascii_digit())))
        {
            return true;
        }
    }
    if c.kind != CandKind::Aggressive {
        // C: next to another filler, as long as the far side reads as a pause.
        let left_filler = {
            let mut j = c.start;
            if j > 0 && toks[j - 1].is_comma() {
                j -= 1;
            }
            j > 0 && removed[j - 1]
        };
        let right_filler = {
            let mut k = c.end;
            if toks.get(k).is_some_and(Tok::is_comma) {
                k += 1;
            }
            k < toks.len() && removed[k]
        };
        let right_ok = next.is_none_or(|n| n.is_comma() || n.is_sentence_end() || (n.is_word() && is_clause_start(&n.lc)));
        let left_ok = prev.is_none_or(|p| p.is_comma() || p.is_sentence_end() || removed[c.start - 1])
            || prev_word.is_some_and(is_conjunction);
        if (left_filler && right_ok) || (right_filler && left_ok) {
            return true;
        }
    }
    false
}

/// Is `idx` the first word of its sentence (ignoring deleted tokens and opening quotes)?
fn sentence_initial(toks: &[Tok<'_>], del: &[bool], idx: usize) -> bool {
    let mut j = idx;
    while j > 0 {
        j -= 1;
        if del[j] {
            continue;
        }
        let t = &toks[j];
        if t.is_sentence_end() {
            return true;
        }
        if t.is_opening() || t.text == "\"" {
            continue;
        }
        return false;
    }
    true
}

pub fn remove_fillers(doc: &mut Doc<'_>, o: &Opts<'_>) {
    let n = doc.toks.len();
    if n == 0 {
        return;
    }
    let mut removed = vec![false; n];

    // Unspaced scripts: strip hesitation strings out of the run.
    if o.ja || o.zh {
        for (i, t) in doc.toks.iter_mut().enumerate() {
            if !t.is_word() || t.text.is_ascii() {
                continue;
            }
            let mut text = t.text.to_string();
            let mut changed = false;
            let sets: [(&[&str], bool); 2] = [(JA_FILLERS, o.ja), (ZH_FILLERS, o.zh)];
            for (set, on) in sets {
                if !on {
                    continue;
                }
                for f in set {
                    if text.contains(f) {
                        text = text.replace(f, "");
                        changed = true;
                    }
                }
            }
            if changed {
                if text.is_empty() {
                    removed[i] = true;
                } else {
                    t.set_text(text);
                }
            }
        }
    }

    // Stage 4: hesitation sounds, minus back-channels ("uh-huh" is one token and never matches;
    // "uh huh", "mm hmm" and "uh oh" are pairs).
    for i in 0..n {
        let t = &doc.toks[i];
        if !t.is_word() || !is_hesitation(&t.lc, o.en) {
            continue;
        }
        let c = collapse(&t.lc);
        let next_pair = doc
            .toks
            .get(i + 1)
            .is_some_and(|nx| nx.is_word() && nx.ws != Ws::None && backchannel_pair(&c, &nx.lc));
        let prev_pair = c == "hm"
            && i > 0
            && doc.toks[i - 1].is_word()
            && collapse(&doc.toks[i - 1].lc) == "m"
            && t.ws != Ws::None;
        let unit = c == "m" && i > 0 && doc.toks[i - 1].is_numeric();
        // A word that is being mentioned, not said: `I said "um" and left.`
        let is_quote = |k: Option<&Tok<'_>>| k.is_some_and(|q| matches!(&*q.text, "\"" | "“" | "”" | "'" | "‘" | "’"));
        let quoted = t.ws == Ws::None
            && is_quote(i.checked_sub(1).map(|p| &doc.toks[p]))
            && is_quote(doc.toks.get(i + 1))
            && doc.toks[i + 1].ws == Ws::None;
        let protected = || {
            o.dictionary.iter().any(|w| {
                let w = w.trim();
                w.len() == t.lc.len() && (w.eq_ignore_ascii_case(&t.lc) || (!w.is_ascii() && *lower(w) == *t.lc))
            })
        };
        if !(next_pair || prev_pair || unit || quoted || protected()) {
            removed[i] = true;
        }
    }

    // Stages 5-6: context fillers decided to a fixpoint, since "um, like, you know" chains.
    let mut cands = find_candidates(&doc.toks, o.aggressive);
    if !cands.is_empty() {
        for _ in 0..4 {
            let mut changed = false;
            cands.retain(|c| {
                if eval(c, &doc.toks, &removed) {
                    removed[c.start..c.end].fill(true);
                    changed = true;
                    false
                } else {
                    true
                }
            });
            if !changed {
                break;
            }
        }
    }
    if !removed.iter().any(|&r| r) {
        return;
    }

    // Delete each run of removed tokens together with the commas it leaves orphaned.
    let toks = &doc.toks;
    let mut del = vec![false; n];
    let mut i = 0;
    while i < n {
        if !removed[i] {
            i += 1;
            continue;
        }
        let a = i;
        let mut b = i;
        while b < n && removed[b] {
            b += 1;
        }
        // "uh..." / "um—": an attached pause mark goes with its filler.
        while b < n && (toks[b].is_ellipsis() || toks[b].is_dash()) && toks[b].ws == Ws::None {
            b += 1;
        }
        del[a..b].fill(true);
        let dash_before = a > 0 && toks[a - 1].is_dash() && !del[a - 1];
        let absorbed_dash = b > a && toks[b - 1].is_dash() && !removed[b - 1];
        let next = toks.get(b);
        if dash_before && (next.is_some_and(Tok::is_dash) || absorbed_dash) {
            // "- uh -" / "—um—": a parenthetical that held only a filler goes with its dashes.
            del[a - 1] = true;
            if let Some(d) = next.filter(|t| t.is_dash()).map(|_| b) {
                del[d] = true;
                b += 1;
            }
            i = b;
            continue;
        }
        let prev_comma = a > 0 && toks[a - 1].is_comma() && !del[a - 1];
        let next = toks.get(b);
        let next_comma = next.is_some_and(Tok::is_comma);
        let next_period = next.is_some_and(|t| matches!(&*t.text, "." | "。"));
        if next_comma {
            del[b] = true;
            if prev_comma {
                let pp = (0..a - 1).rev().find(|&k| !del[k]);
                // ", uh," after an opener keeps one comma ("So, uh, I" -> "So, I", "John, um, I" ->
                // "John, I"), and so does one before a word that wants it ("know, uh, maybe");
                // after an ordinary clause both commas were pause marks around the filler.
                let after = toks.get(b + 1).filter(|t| t.is_word());
                let keep_prev = pp.is_some_and(|p| {
                    let w = &toks[p];
                    (w.is_numeric() && after.is_some_and(Tok::is_numeric))
                        || (w.is_word()
                            && (is_discourse_opener(&w.lc)
                                || (sentence_initial(toks, &del, p) && !is_function_start(&w.lc))))
                }) || after.is_some_and(|t| is_comma_keeper(&t.lc));
                if !keep_prev {
                    del[a - 1] = true;
                }
            }
        } else if prev_comma && (next_period || next.is_none()) {
            del[a - 1] = true;
        } else if !prev_comma && next_period && sentence_initial(toks, &del, a) {
            // A sentence that was only a filler: "Um."
            del[b] = true;
        }
        i = b;
    }
    doc.remove(&del);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::cleanup::tokens::tokenize;

    fn run(s: &str, en: bool, aggressive: bool) -> String {
        let mut d = tokenize(s);
        remove_fillers(&mut d, &Opts { en, ja: true, zh: true, aggressive, dictionary: &[] });
        d.render()
    }

    #[test]
    fn removes_hesitations_with_commas() {
        assert_eq!(run("Um, so I think, uh, we go", true, false), "so I think we go");
        assert_eq!(run("It was, um, great.", true, false), "It was great.");
        assert_eq!(run("So, uh, I think", true, false), "So, I think");
        assert_eq!(run("Okay. Um. Right.", true, false), "Okay. Right.");
        assert_eq!(run("I think so, um.", true, false), "I think so.");
    }

    #[test]
    fn preserves_backchannels() {
        for s in ["uh-huh, sure", "Uh huh.", "mm-hmm", "Mm hmm, okay", "uh oh, that broke", "uh-oh"] {
            assert_eq!(run(s, true, false), s, "{s}");
        }
    }

    #[test]
    fn like_only_in_filler_positions() {
        assert_eq!(run("It was, like, amazing", true, false), "It was amazing");
        assert_eq!(run("Like, I don't know", true, false), "I don't know");
        assert_eq!(run("we should like ship it", true, false), "we should ship it");
        assert_eq!(run("it was like totally broken", true, false), "it was totally broken");
        for s in ["I like it", "it looks like rain", "I would like a coffee", "like a boss", "I would like to go", "Things like this happen", "I was like, no way", "you might like pizza"] {
            assert_eq!(run(s, true, false), s, "{s}");
        }
    }

    #[test]
    fn you_know_only_as_a_tag() {
        assert_eq!(run("It is, you know, hard", true, false), "It is hard");
        assert_eq!(run("You know, I think so", true, false), "I think so");
        assert_eq!(run("It is complicated, you know.", true, false), "It is complicated.");
        for s in ["do you know", "you know what I mean?", "Do you know him?", "as you know, it ends", "It's hard, you know?", "you know the answer"] {
            assert_eq!(run(s, true, false), s, "{s}");
        }
    }

    #[test]
    fn aggressive_extras() {
        assert_eq!(run("It is, basically, done", true, true), "It is done");
        assert_eq!(run("Actually, it works", true, true), "it works");
        assert_eq!(run("So, I mean, we go", true, true), "So, we go");
        assert_eq!(run("It is, kind of, hard", true, true), "It is hard");
        assert_eq!(run("It is, kind of, hard", true, false), "It is, kind of, hard");
        assert_eq!(run("I mean it", true, true), "I mean it");
        assert_eq!(run("This is kind of nice", true, true), "This is kind of nice");
    }

    #[test]
    fn language_gates_um_and_er() {
        assert_eq!(run("um das ist er", false, false), "um das ist er");
        assert_eq!(run("Ähm, das ist euh gut", false, false), "das ist gut");
    }

    #[test]
    fn mm_after_a_number_is_a_unit() {
        assert_eq!(run("a 5 mm bolt", true, false), "a 5 mm bolt");
        assert_eq!(run("a mm bolt", true, false), "a bolt");
    }

    #[test]
    fn cjk_fillers() {
        assert_eq!(run("えーと、今日は晴れです", false, false), "今日は晴れです");
        assert_eq!(run("今日はえっと晴れです", false, false), "今日は晴れです");
        assert_eq!(run("呃，我觉得可以", false, false), "我觉得可以");
    }

    #[test]
    fn protected_dictionary_words_survive() {
        let mut d = tokenize("we love Hmm the band, um");
        let dict = vec!["hmm".to_string()];
        remove_fillers(&mut d, &Opts { en: true, ja: false, zh: false, aggressive: false, dictionary: &dict });
        assert_eq!(d.render(), "we love Hmm the band");
    }
}
