//! Word lists and small predicates for the cleanup stages. All inputs are lower-cased.
use std::collections::HashSet;
use std::hash::{BuildHasherDefault, Hasher};
use std::sync::OnceLock;

/// FxHash: the lists below are probed once per word, so SipHash would dominate the pipeline.
#[derive(Default)]
struct Fx(u64);

impl Hasher for Fx {
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 = (self.0.rotate_left(5) ^ u64::from(b)).wrapping_mul(0x517c_c1b7_2722_0a95);
        }
    }

    fn finish(&self) -> u64 {
        self.0
    }
}

type WordSet = HashSet<&'static str, BuildHasherDefault<Fx>>;

/// `pub fn name(lc: &str) -> bool` backed by a lazily built hash set.
macro_rules! word_set {
    ($(#[$m:meta])* $name:ident: $($w:literal)|+) => {
        $(#[$m])*
        pub fn $name(lc: &str) -> bool {
            static SET: OnceLock<WordSet> = OnceLock::new();
            SET.get_or_init(|| [$($w),+].into_iter().collect()).contains(lc)
        }
    };
}

/// Hesitation sounds, collapsed so elongations match ("ummm" -> "um", "hmmm" -> "hm").
/// `en` enables the sounds that are real words in other languages (German "um", "er").
pub fn is_hesitation(lc: &str, en: bool) -> bool {
    if lc.len() > 16 {
        return false;
    }
    let mut buf = [' '; 8];
    let (mut n, mut k) = (0usize, 0usize);
    for c in lc.chars() {
        n += 1;
        if n > 8 {
            return false;
        }
        if k == 0 || buf[k - 1] != c {
            buf[k] = c;
            k += 1;
        }
    }
    if n < 2 {
        return false;
    }
    match &buf[..k] {
        // Universal. A lone "m"/"э" needs the original to be doubled ("mm" is a unit after a number;
        // the caller checks that).
        ['u', 'h'] | ['u', 'h', 'm'] | ['e', 'r', 'm'] | ['h', 'm'] | ['ä', 'h'] | ['ä', 'h', 'm'] | ['ö', 'h']
        | ['ö', 'h', 'm'] | ['e', 'h', 'm'] | ['e', 'u', 'h'] | ['h', 'e', 'u'] | ['э'] | ['э', 'м'] | ['m'] => true,
        // "um" and "er" are words in German, Portuguese, Dutch, Scandinavian languages.
        ['u', 'm'] => en,
        ['e', 'r'] => en && lc == "er",
        _ => false,
    }
}

/// Back-channel openers that make a neighbouring hesitation sound meaningful ("uh huh", "mm hmm", "uh oh").
pub fn backchannel_pair(first_collapsed: &str, next_lc: &str) -> bool {
    match first_collapsed {
        "uh" => matches!(next_lc, "huh" | "oh"),
        "m" => matches!(next_lc, "hm" | "hmm" | "hmmm" | "mm" | "mmm"),
        _ => false,
    }
}

pub fn collapse(lc: &str) -> String {
    let mut out = String::with_capacity(lc.len());
    for c in lc.chars() {
        if !out.ends_with(c) {
            out.push(c);
        }
    }
    out
}

/// Japanese hesitations, longest first.
pub const JA_FILLERS: &[&str] = &["えーっと", "えーと", "ええと", "えっと", "あのー", "あのう"];
pub const ZH_FILLERS: &[&str] = &["呃"];

word_set! {
    /// Function words whose immediate repetition is a stutter. Left out on purpose: "had", "that",
    /// "very" (legitimate doubles), "no", "so", "can", "will", "do" and numbers.
    is_stutter_word: "i"
        | "we'd"
        | "we'll"
        | "we've"
        | "you'd"
        | "you'll"
        | "you've"
        | "they'd"
        | "they'll"
        | "they've"
        | "he'd"
        | "he'll"
        | "she'd"
        | "she'll"
        | "it'll"
        | "i've"
        | "i'll"
        | "i'd"
        | "i'm"
        | "we're"
        | "you're"
        | "they're"
        | "he's"
        | "she's"
        | "it's"
        | "that's"
        | "there's"
        | "we"
        | "you"
        | "he"
        | "she"
        | "they"
        | "it"
        | "the"
        | "a"
        | "an"
        | "and"
        | "but"
        | "or"
        | "to"
        | "of"
        | "in"
        | "on"
        | "at"
        | "for"
        | "with"
        | "by"
        | "from"
        | "as"
        | "if"
        | "my"
        | "your"
        | "our"
        | "his"
        | "her"
        | "their"
        | "its"
        | "is"
        | "are"
        | "was"
        | "were"
        | "be"
        | "have"
        | "has"
        | "would"
        | "should"
        | "could"
        | "this"
        | "there"
        | "because"
}

/// Words that may repeat across a comma ("I, I think", "the, the") and still be a stutter.
pub fn is_comma_stutter_word(lc: &str) -> bool {
    matches!(
        lc,
        "i" | "we" | "they" | "he" | "she" | "the" | "a" | "an" | "and" | "but" | "to" | "of" | "it" | "is" | "was" | "are"
            | "my" | "that's" | "it's" | "i'm" | "i've" | "i'll" | "i'd" | "we're" | "they're" | "he's" | "she's" | "we'd"
            | "we'll" | "we've" | "they'd" | "they'll" | "they've" | "he'd" | "she'd"
    )
}

/// After ", uh," one comma survives when one of these opens the next clause ("I don't know, uh,
/// maybe Tuesday" -> "I don't know, maybe Tuesday"); before anything else both commas were pause marks.
pub fn is_comma_keeper(lc: &str) -> bool {
    matches!(
        lc,
        "so" | "then" | "maybe" | "perhaps" | "anyway" | "however" | "well" | "now" | "okay" | "ok" | "yes" | "no"
            | "but" | "yet" | "honestly" | "actually" | "first" | "finally" | "also" | "next" | "still" | "instead"
            | "otherwise" | "right" | "sure"
    )
}

/// A comma kept when a filler is removed after one of these (", uh," -> ",").
pub fn is_discourse_opener(lc: &str) -> bool {
    matches!(
        lc,
        "well" | "so" | "yes" | "no" | "okay" | "ok" | "right" | "anyway" | "oh" | "hi" | "hello" | "now" | "look"
            | "listen" | "sure" | "yeah" | "yep" | "alright" | "honestly" | "however" | "also" | "then" | "first"
            | "finally" | "actually" | "basically" | "hey" | "thanks" | "sorry" | "please" | "wait"
    )
}

/// Modals and infinitive markers after which "like" + a bare verb is a filler ("should like ship").
pub fn is_modal_or_to(lc: &str) -> bool {
    matches!(
        lc,
        "should" | "could" | "would" | "can" | "will" | "shall" | "must" | "might" | "may" | "to" | "gonna" | "gotta"
            | "wanna" | "can't" | "won't" | "shouldn't" | "couldn't" | "wouldn't" | "cannot"
    )
}

/// Bare verbs that follow a filler "like" after a modal. Homographs that are common objects of the
/// real verb "like" (use, work, play, order, show, ...) are deliberately absent.
pub fn is_bare_verb(lc: &str) -> bool {
    matches!(
        lc,
        "ship" | "go" | "get" | "make" | "send" | "take" | "put" | "think" | "see" | "add" | "say" | "write" | "read"
            | "build" | "deploy" | "merge" | "fix" | "update" | "create" | "open" | "close" | "keep" | "give" | "tell"
            | "ask" | "find" | "bring" | "meet" | "discuss" | "figure" | "sort" | "wrap" | "kick" | "hold" | "wait"
            | "come" | "leave" | "begin" | "stop" | "pick" | "grab" | "share" | "upload" | "download" | "install"
            | "schedule" | "buy" | "sell" | "pay" | "cancel" | "remove" | "delete" | "edit" | "save" | "launch"
            | "publish" | "submit" | "approve" | "move" | "talk" | "look" | "check" | "start" | "finish" | "review"
            | "test" | "run" | "reach" | "set" | "hit" | "do" | "be" | "have" | "let" | "try" | "ping" | "loop" | "sync"
            | "circle" | "touch" | "dive" | "jump" | "push" | "pull" | "walk" | "sit" | "stand" | "call"
    )
}

pub fn is_copula(lc: &str) -> bool {
    matches!(lc, "is" | "are" | "was" | "were" | "am" | "be" | "been")
        || lc.ends_with("'s")
        || lc.ends_with("’s")
        || lc.ends_with("'re")
        || lc.ends_with("’re")
        || lc.ends_with("'m")
        || lc.ends_with("’m")
}

/// Adverbs after which "like" is a filler, not a comparison.
pub fn is_intensifier(lc: &str) -> bool {
    matches!(
        lc,
        "totally" | "really" | "super" | "completely" | "absolutely" | "extremely" | "literally" | "basically"
            | "actually" | "definitely" | "seriously" | "honestly" | "obviously" | "kinda" | "pretty" | "almost"
            | "probably" | "maybe" | "just" | "so" | "very" | "quite" | "about" | "around" | "approximately"
    )
}

/// Words that start a new clause (subject pronouns, conjunctions): a filler before them is a pause.
pub fn is_clause_start(lc: &str) -> bool {
    matches!(
        lc,
        "i" | "we" | "they" | "he" | "she" | "there" | "so" | "and" | "but" | "because" | "if" | "when" | "then"
            | "just" | "really" | "totally" | "actually" | "basically" | "literally" | "it's" | "i'm" | "i'd" | "i'll"
            | "i've" | "we're" | "they're" | "that's" | "he's" | "she's" | "there's" | "let's"
    )
}

/// Function words: after one of these at the start of a sentence, ", uh," was a pause mark inside
/// the clause ("That, uh, was the plan"), not a vocative or opener.
pub fn is_function_start(lc: &str) -> bool {
    is_clause_start(lc)
        || is_stutter_word(lc)
        || matches!(
            lc,
            "that" | "this" | "these" | "those" | "who" | "what" | "which" | "here" | "all" | "one" | "do" | "does"
                | "did" | "can" | "will" | "may" | "might" | "must" | "not" | "has" | "had" | "been" | "am" | "when"
                | "where" | "how" | "why" | "no" | "some" | "any" | "every" | "each"
        )
}

pub fn is_conjunction(lc: &str) -> bool {
    matches!(lc, "and" | "but" | "so" | "then" | "because" | "or")
}

/// Abbreviations whose full stop does not end a sentence ("etc" and "no" usually do).
pub fn is_abbreviation(lc: &str) -> bool {
    matches!(
        lc,
        "mr" | "mrs" | "ms" | "dr" | "prof" | "sr" | "jr" | "vs" | "e.g" | "i.e" | "st" | "mt" | "approx" | "inc" | "ltd"
            | "corp" | "fig" | "cf" | "al" | "u.s" | "u.k" | "a.m" | "p.m"
    )
}

word_set! {
    /// Words that mark text as English for the lone-"i" fix (positive evidence; words that other
    /// languages also use, such as "to", "is", "so" or "okay", are left out).
    is_english_marker: "the"
        | "you"
        | "your"
        | "with"
        | "would"
        | "should"
        | "could"
        | "have"
        | "has"
        | "are"
        | "my"
        | "it's"
        | "i'm"
        | "i'll"
        | "i've"
        | "i'd"
        | "don't"
        | "can't"
        | "won't"
        | "that's"
        | "there's"
        | "what's"
        | "please"
        | "because"
        | "about"
        | "really"
        | "going"
        | "think"
        | "this"
        | "they"
        | "them"
        | "then"
        | "than"
        | "been"
        | "were"
        | "from"
        | "which"
        | "there"
        | "here"
        | "thanks"
        | "and"
        | "thought"
        | "believe"
        | "wish"
        | "did"
}

word_set! {
    /// Words that mark text as another language, for which "um" and "er" are ordinary words.
    is_non_english_marker: "ich"
        // German
        | "und"
        | "nicht"
        | "ist"
        | "das"
        | "der"
        | "die"
        | "ein"
        | "eine"
        | "mit"
        | "auf"
        | "für"
        | "wir"
        | "sie"
        | "zu"
        | "nein"
        | "aber"
        | "auch"
        | "wenn"
        | "dass"
        | "noch"
        | "bitte"
        | "danke"
        | "uhr"
        | "sind"
        | "habe"
        | "haben"
        | "wird"
        | "kann"
        | "wie"
        | "ja"
        | "dem"
        | "den"
        | "im"
        | "es"
        | "von" // Portuguese
        | "não"
        | "nao"
        | "uma"
        | "você"
        | "voce"
        | "com"
        | "para"
        | "obrigado"
        | "obrigada"
        | "sim"
        | "está"
        | "estou"
        | "isso"
        | "mais"
        | "muito"
        | "também"
        | "são"
        | "foi"
        | "os"
        | "as"
        | "dos" // Dutch
        | "het"
        | "een"
        | "niet"
        | "ik"
        | "dat"
        | "je"
        | "ze"
        | "maar"
        | "ook"
        | "dank"
        | "nee"
        | "zijn"
        | "van"
        | "voor"
        | "met"
        | "ben"
        | "wij" // Scandinavian
        | "og"
        | "jeg"
        | "ikke"
        | "det"
        | "att"
        | "och"
        | "är"
        | "inte"
        | "för"
        | "med"
        | "som"
        | "har"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hesitation_shapes() {
        for w in ["uh", "uhh", "uhm", "erm", "hmm", "hmmm", "hm", "mm", "mmm", "äh", "ähm", "öhm", "euh", "ehm", "heu"] {
            assert!(is_hesitation(w, false), "{w}");
        }
        for w in ["um", "umm", "ummm", "er"] {
            assert!(is_hesitation(w, true), "{w}");
            assert!(!is_hesitation(w, false), "{w} non-en");
        }
        for w in ["err", "am", "m", "ahem", "hum", "uhhuh", "mhm", "eh", "ah", "oh", "emm", "humm", "ur"] {
            assert!(!is_hesitation(w, true), "{w}");
        }
    }

    #[test]
    fn abbreviations() {
        assert!(is_abbreviation("dr") && is_abbreviation("e.g"));
        assert!(!is_abbreviation("no") && !is_abbreviation("etc"));
    }
}
