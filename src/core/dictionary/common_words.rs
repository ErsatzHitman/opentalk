//! Common English words (4+ letters) that the fuzzy dictionary pass must never rewrite.
use std::collections::HashSet;
use std::sync::OnceLock;

const WORDS: &str = "\
able about above accept access across action active actual added after again against agree ahead \
allow almost alone along already also always among amount and another answer anyone anything anyway \
appear apply area around asked aside available away back ball bank base basic because become been \
before began begin behind being believe below best better between beyond bill bird bit black blue \
board boat body book both bring brought build built business call came can't care carry case cause \
center certain chance change check child children choose city class clean clear close cloud code \
cold color come common company complete consider contact content control cost could country \
course cover create current data date days deal dear decide deep design detail did didn't different \
direct does doesn't dollar done door down draw drive drop during each early earth easy edge effect \
either else email enough entire even event ever every exact example expect eyes face fact fall \
family fast feel feet felt few field figure file fill final find fine finish fire first fish five \
follow food foot force form forward found four free friday friend from front full game gave general \
gets getting girl give given goes going gold gone good great green ground group grow guess hand \
happen happy hard have haven't having head hear heard heart heavy help here high hold home hope \
hour house however human hundred idea important include info inside instead into issue item its \
job join just keep kept kind knew know known land language large last later lead learn least leave \
left less let's letter level life light like line list little live local long look lost love made \
main make many mark matter maybe mean meet meeting member mention message might mind mine minute \
miss money month monday more morning most mother move much music must name near need never next \
nice night nine none normal north note nothing notice number object office often okay once only \
open order other our out over own page paper part party past pay people perhaps person phone pick \
picture piece place plan plant play please point possible power present press pretty price problem \
product project provide public pull put question quick quite rain reach read ready real really \
reason receive record red remember report rest result return right river road room round rule \
run said same saturday save says school screen second see seem sell send sense sent seven several \
shall shape share she should show side simple since sit six size sleep slow small some someone \
something sometimes soon sorry sound south speak special speed spend spot stand start state stay \
step still stop store story street strong student study such sunday sure system table take talk \
team tell ten than thank thanks that that's their them then there there's these they thing things \
think third this those though thought three through thursday time today together told tomorrow \
tonight took top toward town travel tree tried true try tuesday turn two type under understand \
until upon use used user using usually value very video view visit voice wait walk want warm was \
wasn't watch water way we're wednesday week weekend well went were what what's when where whether \
which while white who whole whose why wide will window wish with within without woman women word \
work world would write wrong year yes yesterday yet you're young your yours yourself \
january february march april june july august september october november december \
about almost better bigger bother brother butter center daughter father feather gather higher \
hundred lighter longer matter mother number order other paper power rather remember river sister \
slower smaller summer together under water weather whether winter wonder wooden writer \
rust rest must dust gust just bust lust fast past vast cast last mast \
open whisper whispers listen listens parrot perfect carrot kernel colonel \
";

fn set() -> &'static HashSet<&'static str> {
    static SET: OnceLock<HashSet<&'static str>> = OnceLock::new();
    SET.get_or_init(|| WORDS.split_whitespace().collect())
}

/// Short function words (1-3 letters) that never belong inside a fuzzily rejoined span.
pub fn is_function_word(lower: &str) -> bool {
    matches!(
        lower,
        "a" | "i" | "am" | "an" | "as" | "at" | "be" | "by" | "do" | "go" | "he" | "if" | "in" | "is" | "it" | "me" | "my"
            | "no" | "of" | "on" | "or" | "so" | "to" | "up" | "us" | "we" | "the" | "and" | "for" | "are" | "was"
            | "but" | "not" | "you" | "all" | "any" | "can" | "had" | "her" | "his" | "him" | "how" | "its" | "our"
            | "out" | "she" | "who" | "why" | "yes" | "has" | "did" | "get" | "got" | "let" | "may" | "now" | "off"
            | "one" | "two" | "too" | "use" | "way"
    )
}

/// True for a lower-cased common English word.
pub fn is_common_english(lower: &str) -> bool {
    set().contains(lower)
}

#[cfg(test)]
mod tests {
    use super::is_common_english;

    #[test]
    fn knows_common_words() {
        for w in ["their", "there", "friday", "rust", "listen", "open"] {
            assert!(is_common_english(w), "{w}");
        }
        assert!(!is_common_english("openwhispr"));
        assert!(!is_common_english("kubernetes"));
    }
}
