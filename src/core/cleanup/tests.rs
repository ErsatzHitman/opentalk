//! End-to-end fixtures for the deterministic cleanup pipeline.
use super::*;
use crate::core::config::{FillerMode, Snippet};

fn ctx<'a>(lang: &'a str, mode: FillerMode) -> CleanupCtx<'a> {
    CleanupCtx {
        language: lang,
        filler_mode: mode,
        dictionary: &[],
        snippets: &[],
        speech_ms: None,
        fuzzy_dictionary: true,
        spoken_line_breaks: false,
    }
}

fn std_en(raw: &str) -> String {
    process(raw, &ctx("en", FillerMode::Standard))
}

fn agg_en(raw: &str) -> String {
    process(raw, &ctx("en", FillerMode::Aggressive))
}

fn strs(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

/// The Windows TTS sample (tests/fixtures/tts_fillers.wav, raw Whisper output).
#[test]
fn tts_sample() {
    assert_eq!(
        std_en("Um, so I think, uh, we should like ship the new release on Friday, you know, because the the client is waiting."),
        "So I think we should ship the new release on Friday because the client is waiting."
    );
    // Same sentence for an unknown language and the default (Standard) mode.
    assert_eq!(
        process(
            "Um, so I think, uh, we should like ship the new release on Friday, you know, because the the client is waiting.",
            &ctx("auto", FillerMode::Standard)
        ),
        "So I think we should ship the new release on Friday because the client is waiting."
    );
}

/// Standard mode, English: (raw, expected).
const STANDARD: &[(&str, &str)] = &[
    // --- hesitation fillers ---
    ("um", ""),
    ("Uh.", ""),
    ("Um, uh, hmm.", ""),
    ("um hello", "Hello"),
    ("Um, hello there.", "Hello there."),
    ("So, um, yeah.", "So, yeah."),
    ("OK, um, so, uh, yes?", "OK, so, yes?"),
    ("Yes. Uh, no.", "Yes. No."),
    ("I went to the store, uh, and bought milk.", "I went to the store and bought milk."),
    ("We need to, um, talk about it.", "We need to talk about it."),
    ("It was umm really good.", "It was really good."),
    ("uhh I don't know", "I don't know"),
    ("Ummmm, okay.", "Okay."),
    ("Hmm, that's interesting.", "That's interesting."),
    ("Well, hmm, okay.", "Well, okay."),
    ("erm, let me think", "Let me think"),
    ("Mm. Okay.", "Okay."),
    ("Okay. Hmm. Let me check.", "Okay. Let me check."),
    ("so the plan is, um, to ship.", "So the plan is to ship."),
    ("I think... uh... we should go.", "I think... we should go."),
    ("I think so, um.", "I think so."),
    ("ship it, uh", "Ship it"),
    ("The thing is uh, we can't.", "The thing is we can't."),
    ("UM, HELLO", "HELLO"),
    ("um [BLANK_AUDIO] uh", ""),
    ("I don't know, uh, maybe Tuesday? Or, um, Wednesday.", "I don't know, maybe Tuesday? Or Wednesday."),
    ("\"Um, hello,\" he said. \"Uh, yes?\"", "\"Hello,\" he said. \"Yes?\""),
    ("Is it - uh - working? Yes — um — it is.", "Is it working? Yes it is."),
    ("That, uh, was the plan.", "That was the plan."),
    ("Version 2.0, uh, is out.", "Version 2.0 is out."),
    ("I'll see you at 5, um, 5:30.", "I'll see you at 5, 5:30."),
    ("The meeting is on, um, Monday... no wait, Tuesday.", "The meeting is on Monday... no wait, Tuesday."),
    ("So yeah, I was gonna, uh, tell you about it, but, um, I forgot.", "So yeah, I was gonna tell you about it, but I forgot."),
    ("Total: $5.00 uh plus tax.", "Total: $5.00 plus tax."),
    ("U.S. officials said, um, that it's fine. Then they left.", "U.S. officials said that it's fine. Then they left."),
    ("HELLO UM WORLD", "HELLO WORLD"),
    // --- back-channels and units are preserved ---
    ("Uh-huh, I see.", "Uh-huh, I see."),
    ("Mm-hmm. Right.", "Mm-hmm. Right."),
    ("Uh oh, that broke.", "Uh oh, that broke."),
    ("Uh huh.", "Uh huh."),
    ("Mm hmm, okay.", "Mm hmm, okay."),
    ("I measured 5 mm and 7 mm.", "I measured 5 mm and 7 mm."),
    // --- context fillers ---
    ("It's, like, really hard.", "It's really hard."),
    ("Like, I don't even know.", "I don't even know."),
    ("We need to like check the logs.", "We need to check the logs."),
    ("it was like totally broken", "It was totally broken"),
    ("So, like, what now?", "So, what now?"),
    ("You know, it's fine.", "It's fine."),
    ("It's hard, you know.", "It's hard."),
    ("It's complicated, you know, with the client.", "It's complicated with the client."),
    ("Um, like, you know, it's fine.", "It's fine."),
    // --- stutters ---
    ("The the cat sat.", "The cat sat."),
    ("I I I think so.", "I think so."),
    ("I- I think so.", "I think so."),
    ("wh- what happened?", "What happened?"),
    ("We, we, we should go.", "We should go."),
    ("it was go- going well", "It was going well"),
    ("b-b-but why not", "But why not"),
    ("and and then we left", "And then we left"),
    ("You know, you know, you know what? Never mind.", "You know what? Never mind."),
    ("I've, I've been there. They, they said so.", "I've been there. They said so."),
    ("I\u{2019}ve, I\u{2019}ve been there.", "I\u{2019}ve been there."),
    ("It was kind of like a, a, a weird day.", "It was kind of like a weird day."),
    ("she said she'd uh she'd be there", "She said she'd be there"),
    ("I think that, uh, that is, is a good point.", "I think that that is a good point."),
    ("to to be honest", "To be honest"),
    // --- punctuation + capitalization ---
    ("hello world", "Hello world"),
    ("hello , world", "Hello, world"),
    ("Wait , what ?", "Wait, what?"),
    ("hello. how are you? fine! ok", "Hello. How are you? Fine! Ok"),
    ("i think i can do it", "I think I can do it"),
    ("so i'm sure i'll go", "So I'm sure I'll go"),
    ("...um so yeah", "So yeah"),
    // --- loops and tags ---
    ("the the the the the", "The"),
    ("Okay okay okay okay okay.", "Okay."),
    ("Thank you. Thank you. Thank you. Thank you.", "Thank you."),
    ("[BLANK_AUDIO]", ""),
    ("Hello [Music] world", "Hello world"),
    ("(silence)", ""),
    ("♪ la la la ♪", ""),
    ("*applause* thank you all", "Thank you all"),
];

#[test]
fn standard_fixtures() {
    let mut failures = Vec::new();
    for &(raw, want) in STANDARD {
        let got = std_en(raw);
        if got != want {
            failures.push(format!("  {raw:?}\n    want {want:?}\n    got  {got:?}"));
        }
    }
    assert!(failures.is_empty(), "{} failures:\n{}", failures.len(), failures.join("\n"));
}

/// Text that must come through untouched in Standard mode (already well formed).
const UNCHANGED: &[&str] = &[
    "Hello world.",
    "I like it.",
    "It looks like rain.",
    "I would like a coffee.",
    "I would like to go home.",
    "She looks like her mother.",
    "Things like this happen.",
    "Use tools like hammers and saws.",
    "Do you like pizza?",
    "You might like pizza.",
    "I was like, what?",
    "He's like, so tired.",
    "Do you know where it is?",
    "Do you know him?",
    "You know what I mean?",
    "It's hard, you know?",
    "As you know, we ship Friday.",
    "You know the answer.",
    "I know you know.",
    "He had had enough.",
    "I know that that works.",
    "It is very very good.",
    "Call 555 555 now.",
    "Wait, wait, wait.",
    "I think, I think we should go.",
    "No no no.",
    "So so.",
    "Hey you, you there.",
    "I measured 5 mm.",
    "Mm-hmm.",
    "Uh-uh, no way.",
    "The re-read of the e-mail was fine.",
    "Pre- and post-war housing.",
    "Version 3.5 costs $1,000.",
    "Visit example.com or mail me@example.com.",
    "Dr. Smith arrived at 10:30 a.m. and left.",
    "5 5 5 5 5",
    "OK.",
    "iPhone and eBay are brands.",
    "He said, \"Hello,\" and left.",
    "I said \"um\" and left.",
    "The word \"like\" is overused.",
    "He goes like, no way.",
    "Hello, hello, hello.",
    "It is what it is.",
    "OK so basically what happened is, the server crashed.",
    "I like to move it, move it.",
    "Would you like some tea?",
    "We'd like to go.",
    "It's just like old times.",
    "He ran like the wind.",
    "She's like her mother.",
    "It's like a dream.",
    "The baby looks like him.",
    "Feel like going out?",
    "You can like and subscribe.",
    "I'm going to like my new job.",
    "They will like it.",
    "People like you and me.",
    "If you like, we can go.",
    "Let me know.",
    "I know, right?",
    "You know him, right?",
    "Wow, you know what, that is great.",
    "I don't know, you know?",
    "She said she would call back.",
    "Okay, so here is the plan.",
    "Yes, I think so, too.",
    "There, there, it's fine.",
    "Hi, this is a test, and it works.",
    "Is it actually working?",
    "It is basically done.",
    "It's kind of hard.",
    "I mean it.",
];

#[test]
fn unchanged_fixtures() {
    let mut failures = Vec::new();
    for &raw in UNCHANGED {
        let got = std_en(raw);
        if got != raw {
            failures.push(format!("  {raw:?}\n    got  {got:?}"));
        }
    }
    assert!(failures.is_empty(), "{} failures:\n{}", failures.len(), failures.join("\n"));
}

#[test]
fn outputs_are_stable_when_processed_again() {
    let mut failures = Vec::new();
    for &(raw, _) in STANDARD {
        let once = std_en(raw);
        let twice = std_en(&once);
        if once != twice {
            failures.push(format!("  {raw:?}\n    once  {once:?}\n    twice {twice:?}"));
        }
    }
    assert!(failures.is_empty(), "{} failures:\n{}", failures.len(), failures.join("\n"));
}

#[test]
fn aggressive_mode_adds_comma_bounded_extras() {
    let cases = [
        ("It is, basically, done.", "It is done."),
        ("Actually, I think so.", "I think so."),
        ("I mean, it works.", "It works."),
        ("It is, literally, the best.", "It is the best."),
        ("It's, kind of, hard.", "It's hard."),
        ("The test is, sort of, done.", "The test is done."),
        ("So, I mean, we go.", "So, we go."),
        ("Uh, like, I mean, you know, it's fine.", "It's fine."),
        ("Basically, we ship Friday.", "We ship Friday."),
        // Not comma-bounded: untouched even in aggressive mode.
        ("It is basically done.", "It is basically done."),
        ("I mean it.", "I mean it."),
        ("Is it actually working?", "Is it actually working?"),
        ("It's kind of hard.", "It's kind of hard."),
        ("What kind of dog is it?", "What kind of dog is it?"),
        ("I literally laughed.", "I literally laughed."),
    ];
    for (raw, want) in cases {
        assert_eq!(agg_en(raw), want, "{raw:?}");
    }
    // The same text in Standard mode keeps the extras.
    assert_eq!(std_en("It is, basically, done."), "It is, basically, done.");
    assert_eq!(std_en("I mean, it works."), "I mean, it works.");
    assert_eq!(std_en("It's, kind of, hard."), "It's, kind of, hard.");
}

#[test]
fn off_mode_is_verbatim() {
    let off = ctx("en", FillerMode::Off);
    for raw in [
        "Um, so I think, uh, we go",
        "hello world",
        "the the cat",
        "i think so",
        "It was, like, hard",
    ] {
        assert_eq!(process(raw, &off), raw);
    }
    // Still drops recognizer garbage.
    assert_eq!(process("hello [BLANK_AUDIO] world", &off), "hello world");
    assert_eq!(process("[BLANK_AUDIO]", &off), "");
    assert_eq!(process("go go go go go go", &off), "go");
}

#[test]
fn language_specific_fillers() {
    let cases = [
        ("de", "Ähm, ich glaube, äh, wir gehen um fünf Uhr.", "Ich glaube wir gehen um fünf Uhr."),
        ("de", "Er sagt, um drei sind wir da.", "Er sagt, um drei sind wir da."),
        ("fr", "Euh, je pense, euh, que oui.", "Je pense que oui."),
        ("it", "Ehm, allora andiamo.", "Allora andiamo."),
        ("ja", "えーと、今日は晴れです。", "今日は晴れです。"),
        ("ja", "今日はえっと晴れです。", "今日は晴れです。"),
        ("zh", "呃，我觉得可以。", "我觉得可以。"),
        ("pt", "Um dia eu vou.", "Um dia eu vou."),
        ("en-US", "Um, send it.", "Send it."),
        ("en_GB", "Er, send it.", "Send it."),
        ("de", "Um, hallo.", "Um, hallo."),
    ];
    for (lang, raw, want) in cases {
        assert_eq!(process(raw, &ctx(lang, FillerMode::Standard)), want, "{lang}: {raw:?}");
    }
}

#[test]
fn auto_language_does_not_eat_um_in_other_languages() {
    let auto = ctx("auto", FillerMode::Standard);
    assert_eq!(process("Um fünf Uhr gehen wir.", &auto), "Um fünf Uhr gehen wir.");
    assert_eq!(process("Um dia não foi suficiente.", &auto), "Um dia não foi suficiente.");
    assert_eq!(process("Um, send it to John.", &auto), "Send it to John.");
    assert_eq!(process("Äh, wir gehen.", &auto), "Wir gehen.");
    assert_eq!(process("Mama i tata.", &auto), "Mama i tata.");
    assert_eq!(process("well i think the plan is good", &auto), "Well I think the plan is good");
}

#[test]
fn french_spacing_is_kept() {
    assert_eq!(process("Bonjour !", &ctx("fr", FillerMode::Standard)), "Bonjour !");
    assert_eq!(process("Merci beaucoup , vous ?", &ctx("auto", FillerMode::Standard)), "Merci beaucoup, vous ?");
    assert_eq!(process("Bonjour\u{202F}!", &ctx("fr", FillerMode::Standard)), "Bonjour\u{202F}!");
}

#[test]
fn hallucination_guard_needs_little_speech() {
    let mut c = ctx("en", FillerMode::Standard);
    for text in ["Thank you for watching!", "Please subscribe.", "Продолжение следует...", "thanks for watching"] {
        c.speech_ms = Some(0);
        assert_eq!(process(text, &c), "", "{text}");
        c.speech_ms = Some(599);
        assert_eq!(process(text, &c), "", "{text}");
        c.speech_ms = Some(600);
        assert_ne!(process(text, &c), "", "{text}");
        c.speech_ms = None;
        assert_ne!(process(text, &c), "", "{text}");
    }
    c.speech_ms = Some(100);
    assert_eq!(process("Thank you.", &c), "Thank you.");
    assert_eq!(process("Thank you for watching the kids.", &c), "Thank you for watching the kids.");
    c.speech_ms = Some(5000);
    assert_eq!(process("Thank you for watching!", &c), "Thank you for watching!");
}

#[test]
fn dictionary_case_fix_and_fuzzy_run_after_filler_removal() {
    let dict = strs(&["OpenWhispr", "Kubernetes", "iPhone"]);
    let mut c = ctx("en", FillerMode::Standard);
    c.dictionary = &dict;
    assert_eq!(
        process("um i use open whisper and cuberneties daily.", &c),
        "I use OpenWhispr and Kubernetes daily."
    );
    assert_eq!(process("iphone sales. iphone is great", &c), "iPhone sales. iPhone is great");
    assert_eq!(process("i use openwhispr", &c), "I use OpenWhispr");
    c.fuzzy_dictionary = false;
    assert_eq!(process("i use open whisper daily", &c), "I use open whisper daily");
    assert_eq!(process("i use openwhispr daily", &c), "I use OpenWhispr daily");
}

#[test]
fn dictionary_words_that_look_like_fillers_survive() {
    let dict = strs(&["Hmm"]);
    let mut c = ctx("en", FillerMode::Standard);
    c.dictionary = &dict;
    assert_eq!(process("we love Hmm the band, um", &c), "We love Hmm the band");
}

#[test]
fn snippets_expand_last() {
    let snippets = vec![
        Snippet { trigger: "my cal".into(), replacement: "cal.com/me".into() },
        Snippet { trigger: "signature".into(), replacement: "Best,\nUmut".into() },
        Snippet { trigger: "um".into(), replacement: "NEVER".into() },
    ];
    let mut c = ctx("en", FillerMode::Standard);
    c.snippets = &snippets;
    assert_eq!(process("um, send my cal.", &c), "Send cal.com/me");
    assert_eq!(process("Thanks, uh, signature.", &c), "Thanks, Best,\nUmut");
    // The pre-LLM pass leaves triggers alone.
    assert_eq!(process_pre_llm("send my cal", &c), "Send my cal");
    // Fillers are removed before snippet matching, so a trigger named "um" cannot fire.
    assert_eq!(process("um", &c), "");
}

#[test]
fn spoken_line_breaks_are_opt_in() {
    let mut c = ctx("en", FillerMode::Standard);
    assert_eq!(process("Hello. New line. How are you?", &c), "Hello. New line. How are you?");
    c.spoken_line_breaks = true;
    assert_eq!(process("Hello. New line. How are you?", &c), "Hello.\nHow are you?");
    assert_eq!(process("Dear John, new line, um thanks for the note", &c), "Dear John,\nThanks for the note");
    assert_eq!(process("Point one. New paragraph. Point two.", &c), "Point one.\n\nPoint two.");
    assert_eq!(process("I added a new line of products.", &c), "I added a new line of products.");
}

#[test]
fn whitespace_is_normalised() {
    // OpenWhispr transcriptionResultParsing: newlines become spaces, blank markers are silence.
    assert_eq!(std_en("hello\nworld "), "Hello world");
    assert_eq!(process("hello\nworld ", &ctx("en", FillerMode::Off)), "hello world");
    assert_eq!(std_en(" [BLANK_AUDIO] "), "");
    assert_eq!(std_en("  so   many \t spaces\r\nhere  "), "So many spaces here");
}

#[test]
fn pathological_filler_runs_stay_fast() {
    let input = "um like ".repeat(20_000);
    let t = std::time::Instant::now();
    let out = std_en(&input);
    assert!(t.elapsed() < std::time::Duration::from_secs(5), "{:?}", t.elapsed());
    assert!(out.len() < input.len());
}

#[test]
fn empty_and_degenerate_input() {
    let c = ctx("en", FillerMode::Standard);
    for raw in ["", " ", "\n\t", "...", ",", ". . .", "um, uh, er", "[BLANK_AUDIO] [Music]", "—", "\"\""] {
        assert_eq!(process(raw, &c), "", "{raw:?}");
    }
    assert_eq!(process("a", &c), "A");
    assert_eq!(process("42", &c), "42");
}

#[test]
fn never_panics_and_never_pads() {
    // A deterministic walk over awkward token mixes, including multi-byte and combining text.
    let atoms = [
        "um", "uh", ",", ".", "...", "…", "like", "you", "know", "the", "the", "I", "i", "-", "—", "wh-", "b-b-but",
        "new", "line", "paragraph", "\"", "(", ")", "日本語", "えーと", "呃", "İmza", "cafe\u{301}", "👍", "mm", "hmm",
        "5", "mm", "basically", "kind", "of", "?", "!", "'", "’", "\n", "\t", "  ", "Ünï", "ß", "i'm", "[Music]", "♪",
    ];
    let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut next = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    let dict = strs(&["OpenWhispr", "Kubernetes", "日本語", "C++"]);
    let snippets = vec![Snippet { trigger: "new line".into(), replacement: "NL".into() }];
    for mode in [FillerMode::Off, FillerMode::Standard, FillerMode::Aggressive] {
        for lang in ["auto", "en", "de", "ja", "tr", "fr"] {
            for _ in 0..300 {
                let len = (next() % 14) as usize;
                let mut s = String::new();
                for _ in 0..len {
                    s.push_str(atoms[(next() % atoms.len() as u64) as usize]);
                    if next() % 4 != 0 {
                        s.push(' ');
                    }
                }
                let c = CleanupCtx {
                    language: lang,
                    filler_mode: mode,
                    dictionary: &dict,
                    snippets: &snippets,
                    speech_ms: Some(next() % 1500),
                    fuzzy_dictionary: next() % 2 == 0,
                    spoken_line_breaks: next() % 2 == 0,
                };
                let out = process(&s, &c);
                assert!(!out.starts_with(' ') && !out.ends_with(' '), "padded {out:?} from {s:?}");
                let _ = process_pre_llm(&s, &c);
            }
        }
    }
}

/// 1000 words of dictation with fillers, a dictionary, snippets and every option on.
fn thousand_words() -> String {
    let para = "Um, so I think, uh, we should like ship the new release on Friday, you know, because the the client is waiting. \
                I- I mean, the open whisper team said that the build is, basically, done and we can deploy to cuberneties. \
                Hmm, it was, like, really hard to test. Thank you for the update, new line, see you at five p.m. ";
    let mut s = String::new();
    while s.split_whitespace().count() < 1000 {
        s.push_str(para);
    }
    s
}

#[test]
fn process_1000_words_is_fast() {
    let input = thousand_words();
    let words = input.split_whitespace().count();
    assert!((1000..1100).contains(&words), "{words}");
    let dict = strs(&["OpenWhispr", "Kubernetes", "Parakeet", "Alcahest", "TypeScript", "Electron", "Chromium", "GitHub"]);
    let snippets = vec![
        Snippet { trigger: "my cal".into(), replacement: "cal.com/me".into() },
        Snippet { trigger: "zoom link".into(), replacement: "zoom.us/j/123".into() },
    ];
    let c = CleanupCtx {
        language: "en",
        filler_mode: FillerMode::Aggressive,
        dictionary: &dict,
        snippets: &snippets,
        speech_ms: Some(60_000),
        fuzzy_dictionary: true,
        spoken_line_breaks: true,
    };
    let want = process(&input, &c);
    assert!(want.contains("OpenWhispr") && want.contains("Kubernetes") && want.contains('\n'), "{want}");
    assert!(!want.contains("Um") && !want.contains(" uh"), "{want}");

    // Best of repeated runs, so a scheduler hiccup or a busy CI machine cannot fail the test. The
    // product target is under 1 ms in release; debug builds (cargo test default) are far slower,
    // so allow generous room there.
    let mut samples = Vec::new();
    for _ in 0..60 {
        let t = std::time::Instant::now();
        let out = process(std::hint::black_box(&input), std::hint::black_box(&c));
        samples.push(t.elapsed());
        assert_eq!(out, want);
    }
    samples.sort();
    let median = samples[0];
    eprintln!("cleanup::process on {words} words: best {median:?}, median {:?}", samples[samples.len() / 2]);
    let limit = if cfg!(debug_assertions) { std::time::Duration::from_millis(60) } else { std::time::Duration::from_millis(3) };
    // (Measured ~0.35 ms alone; 3 ms keeps the test stable while 24 other test threads compete.)
    assert!(median < limit, "process() on {words} words took {median:?} at best (limit {limit:?})");

    // A big personal dictionary and snippet list must not change that: indexes are cached per list.
    // Spread over the alphabet like a real list, not 400 terms sharing one initial.
    let initial = |i: usize| char::from(b'A' + (i * 7 % 26) as u8);
    let big_dict: Vec<String> = (0..400).map(|i| format!("{}orbex{i}Tool{}", initial(i), i * 7)).chain(dict.iter().cloned()).collect();
    let big_snippets: Vec<Snippet> = (0..80)
        .map(|i| Snippet { trigger: format!("{}nippet trigger {i}", initial(i).to_ascii_lowercase()), replacement: format!("expansion number {i}") })
        .chain(snippets.iter().cloned())
        .collect();
    let big = CleanupCtx { dictionary: &big_dict, snippets: &big_snippets, ..c.clone() };
    assert_eq!(process(&input, &big), want);
    let mut best = std::time::Duration::MAX;
    for _ in 0..60 {
        let t = std::time::Instant::now();
        std::hint::black_box(process(std::hint::black_box(&input), std::hint::black_box(&big)));
        best = best.min(t.elapsed());
    }
    eprintln!("cleanup::process on {words} words, 408 terms + 82 snippets: best {best:?}");
    assert!(best < limit, "process() with a big dictionary took {best:?} at best (limit {limit:?})");
}

#[test]
#[ignore]
fn bench_stages() {
    use std::time::Instant;
    fn med<F: FnMut()>(name: &str, mut f: F) {
        let mut v = Vec::new();
        for _ in 0..400 {
            let t = Instant::now();
            f();
            v.push(t.elapsed());
        }
        v.sort();
        eprintln!("{name:>14}: min {:>8.1?}  median {:>8.1?}", v[0], v[v.len() / 2]);
    }
    let input = thousand_words();
    let dict = strs(&["OpenWhispr", "Kubernetes", "Parakeet", "Alcahest", "TypeScript", "Electron", "Chromium", "GitHub"]);
    let snippets = vec![
        Snippet { trigger: "my cal".into(), replacement: "cal.com/me".into() },
        Snippet { trigger: "zoom link".into(), replacement: "zoom.us/j/123".into() },
    ];
    med("strip_nonspeech", || { std::hint::black_box(prep::strip_nonspeech(&input)); });
    med("tokenize", || { std::hint::black_box(tokens::tokenize(&input)); });
    med("tok+loops", || { let mut d = tokens::tokenize(&input); prep::collapse_loops(&mut d); });
    med("+fillers", || {
        let mut d = tokens::tokenize(&input); prep::collapse_loops(&mut d);
        fillers::remove_fillers(&mut d, &fillers::Opts { en: true, um_at_pause_only: false, ja: false, zh: false, aggressive: true, dictionary: &[] });
    });
    med("+stutter", || {
        let mut d = tokens::tokenize(&input); prep::collapse_loops(&mut d);
        fillers::remove_fillers(&mut d, &fillers::Opts { en: true, um_at_pause_only: false, ja: false, zh: false, aggressive: true, dictionary: &[] });
        stutter::collapse_stutters(&mut d);
    });
    med("+repair+render", || {
        let mut d = tokens::tokenize(&input); prep::collapse_loops(&mut d);
        fillers::remove_fillers(&mut d, &fillers::Opts { en: true, um_at_pause_only: false, ja: false, zh: false, aggressive: true, dictionary: &[] });
        stutter::collapse_stutters(&mut d);
        repair::repair(&mut d, &repair::RepairOpts { turkish: false, english: true, french: false, auto: false });
        std::hint::black_box(d.render());
    });
    let cleaned = {
        let mut d = tokens::tokenize(&input); prep::collapse_loops(&mut d);
        fillers::remove_fillers(&mut d, &fillers::Opts { en: true, um_at_pause_only: false, ja: false, zh: false, aggressive: true, dictionary: &[] });
        stutter::collapse_stutters(&mut d);
        repair::repair(&mut d, &repair::RepairOpts { turkish: false, english: true, french: false, auto: false });
        d.render()
    };
    med("case_fix", || { std::hint::black_box(dictionary::case_fix(&cleaned, &dict)); });
    med("fuzzy_fix", || { std::hint::black_box(dictionary::fuzzy_fix(&cleaned, &dict)); });
    med("line_breaks", || { let mut d = tokens::tokenize(&cleaned); repair::apply_line_breaks(&mut d, false); std::hint::black_box(d.render()); });
    med("snippets", || { std::hint::black_box(dictionary::expand_snippets(&cleaned, &snippets)); });
    let initial = |i: usize| char::from(b'A' + (i * 7 % 26) as u8);
    let big_dict: Vec<String> = (0..400).map(|i| format!("{}orbex{i}Tool{}", initial(i), i * 7)).chain(dict.iter().cloned()).collect();
    let big_snippets: Vec<Snippet> = (0..80)
        .map(|i| Snippet { trigger: format!("{}nippet trigger {i}", initial(i).to_ascii_lowercase()), replacement: format!("expansion number {i}") })
        .chain(snippets.iter().cloned())
        .collect();
    med("big case_fix", || { std::hint::black_box(dictionary::case_fix(&cleaned, &big_dict)); });
    med("big fuzzy_fix", || { std::hint::black_box(dictionary::fuzzy_fix(&cleaned, &big_dict)); });
    med("big snippets", || { std::hint::black_box(dictionary::expand_snippets(&cleaned, &big_snippets)); });
    let c = CleanupCtx { language: "en", filler_mode: FillerMode::Aggressive, dictionary: &dict, snippets: &snippets, speech_ms: Some(60_000), fuzzy_dictionary: true, spoken_line_breaks: true };
    med("process(all)", || { std::hint::black_box(process(&input, &c)); });
    let c2 = CleanupCtx { dictionary: &[], snippets: &[], fuzzy_dictionary: false, spoken_line_breaks: false, ..c.clone() };
    med("process(bare)", || { std::hint::black_box(process(&input, &c2)); });
}

/// Unknown language: "um" between two words is a Portuguese article, not a hesitation.
#[test]
fn auto_language_keeps_um_article_between_words() {
    let auto = |s: &str| process(s, &ctx("auto", FillerMode::Standard));
    assert_eq!(auto("Um, olá, eu quero um café"), "Olá, eu quero um café");
    assert_eq!(auto("Eu tenho um carro e um gato"), "Eu tenho um carro e um gato");
    assert_eq!(auto("Um, hello there"), "Hello there");
    assert_eq!(auto("we went to um the store"), "We went to the store");
    assert_eq!(auto("I was thinking, um, that we go"), "I was thinking that we go");
}

/// A leading spoken break leaves no newline, but the text after it still starts a line.
#[test]
fn leading_line_break_capitalizes_next_word() {
    let mut c = ctx("en", FillerMode::Standard);
    c.spoken_line_breaks = true;
    assert_eq!(process("new line hello new paragraph world", &c), "Hello

World");
    assert_eq!(process("hello new line world", &c), "Hello
World");
}

/// A plural / possessive of a dictionary term is left alone by the fuzzy pass.
#[test]
fn fuzzy_keeps_plural_of_term() {
    let dict = strs(&["Mahendra", "Kubernetes"]);
    let mut c = ctx("en", FillerMode::Standard);
    c.dictionary = &dict;
    assert_eq!(process("Mahendras car is here", &c), "Mahendras car is here");
}
