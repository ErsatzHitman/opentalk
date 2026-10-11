//! The text pipeline between "audio captured" and "text ready to paste":
//! transcription (streaming session or batch) -> deterministic cleanup -> either the voice
//! rewrite (when the dictation ends with the rewrite phrase) or the optional AI cleanup (never
//! fatal) -> snippets. Shared by the daemon worker and `--transcribe-file`.
use crate::core::cleanup::{self, CleanupCtx};
use crate::core::config::Config;
use crate::core::dictionary;
use crate::core::rewrite::{self, Detect, Trigger};
use crate::core::timing::now_ms;
use crate::llm::LlmError;
use crate::stt::{self, Audio, StreamingSession, SttError, SttErrorCode, Transcript};
use crate::{log_info, log_warn};

/// What the voice rewrite did for one dictation.
#[derive(Clone, Debug, Default, PartialEq)]
pub enum RewriteState {
    /// The dictation did not end with the rewrite phrase (or the feature is off).
    #[default]
    None,
    /// The AI provider's concise rewrite replaced the text.
    Done,
    /// The body is only a few words: pasted as spoken, no network call.
    Skipped,
    /// The call failed or its answer was rejected: the deterministic body was pasted instead.
    Failed(LlmError),
    /// The dictation was only the phrase: nothing to paste.
    NothingToRewrite,
}

impl RewriteState {
    /// Short stable name for logs and `--transcribe-file` output.
    pub fn label(&self) -> &'static str {
        match self {
            RewriteState::None => "none",
            RewriteState::Done => "done",
            RewriteState::Skipped => "skipped",
            RewriteState::Failed(_) => "failed",
            RewriteState::NothingToRewrite => "nothing",
        }
    }
}

/// Result of the text stages.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Cleaned {
    /// Recognizer output, untouched.
    pub raw: String,
    /// Final text ("" when nothing but fillers / hallucination remained, or only the phrase).
    pub text: String,
    pub ai_cleaned: bool,
    /// AI cleanup was asked for and failed (error, timeout or an unusable answer): the on-device
    /// text of the transcript was pasted instead. The reason, for logs and `--transcribe-file`.
    pub cleanup_error: Option<String>,
    /// Includes the rewrite call.
    pub cleanup_ms: f64,
    pub rewrite: RewriteState,
    /// Time spent in the rewrite call (0 when none was made).
    pub rewrite_ms: f64,
    /// For [`RewriteState::Done`]: the deterministic text (phrase removed) that was rewritten.
    pub original: Option<String>,
}

/// `(body, extra instructions)` -> the rewrite.
pub type RewriteCall<'a> = dyn Fn(&str, Option<&str>) -> Result<String, LlmError> + 'a;

/// The two LLM calls the text stage can make (injected in tests).
pub struct LlmCalls<'a> {
    /// Polish the deterministic text (`ai_cleanup.enabled`).
    pub cleanup: &'a dyn Fn(&str) -> Result<String, LlmError>,
    /// Rewrite the body of a dictation that contained the phrase, concisely, following the
    /// speaker's extra instructions when they said any after the phrase.
    pub rewrite: &'a RewriteCall<'a>,
}

/// Bodies of up to this many words are pasted as spoken: a rewrite cannot make them shorter.
/// Extra instructions lift the limit ("translate it to Spanish" applies to two words too).
pub const REWRITE_MIN_WORDS: usize = 4;

/// STT stage. A streaming session that fails (or returns nothing) falls back to the batch
/// provider with the recorded audio; `StreamingOnly` models report the streaming error instead.
pub fn transcribe(cfg: &Config, audio: &Audio, stream: Option<Box<dyn StreamingSession>>) -> Result<Transcript, SttError> {
    transcribe_with(stream, || stt::transcribe(cfg, audio))
}

fn transcribe_with(
    stream: Option<Box<dyn StreamingSession>>,
    batch: impl FnOnce() -> Result<Transcript, SttError>,
) -> Result<Transcript, SttError> {
    let Some(session) = stream else { return batch() };
    let streamed = session.finish();
    match streamed {
        Ok(t) if !t.text.trim().is_empty() => Ok(t),
        Ok(_) => match batch() {
            Err(e) if e.code == SttErrorCode::StreamingOnly => Err(stt::errors::no_speech()),
            other => other,
        },
        Err(e) if matches!(e.code, SttErrorCode::NoSpeech | SttErrorCode::Cancelled) => Err(e),
        Err(e) => {
            log_warn!("streaming transcription failed ({:?}: {}); falling back to batch", e.code, e.detail);
            match batch() {
                Err(b) if b.code == SttErrorCode::StreamingOnly => Err(e),
                other => other,
            }
        }
    }
}

/// Cleanup stages with the real LLM provider.
pub fn clean(cfg: &Config, transcript: &Transcript, speech_ms: u64) -> Cleaned {
    // Dictionary words AND snippet triggers: the rewrite must keep a trigger word intact, the
    // snippet is expanded after it.
    let hints = dictionary::hint_words(&cfg.dictionary, &cfg.snippets);
    clean_with_calls(
        cfg,
        transcript,
        speech_ms,
        &LlmCalls {
            cleanup: &|text| crate::llm::cleanup(cfg, text, &cfg.dictionary),
            rewrite: &|text, instructions| crate::llm::rewrite(cfg, text, instructions, &hints),
        },
    )
}

/// [`clean`] with one LLM call injected for both purposes (tests).
#[cfg(test)]
pub fn clean_with(
    cfg: &Config,
    transcript: &Transcript,
    speech_ms: u64,
    llm: &dyn Fn(&str) -> Result<String, LlmError>,
) -> Cleaned {
    clean_with_calls(cfg, transcript, speech_ms, &LlmCalls { cleanup: llm, rewrite: &|text, _| llm(text) })
}

/// [`clean`] with both LLM calls injected (tests).
pub fn clean_with_calls(cfg: &Config, transcript: &Transcript, speech_ms: u64, calls: &LlmCalls) -> Cleaned {
    let t0 = now_ms();
    let raw = transcript.text.clone();
    let language = transcript.detected_language.as_deref().filter(|_| cfg.language.trim().eq_ignore_ascii_case("auto")).unwrap_or(&cfg.language);
    let ctx = CleanupCtx {
        language,
        filler_mode: cfg.filler_mode,
        dictionary: &cfg.dictionary,
        snippets: &cfg.snippets,
        speech_ms: Some(speech_ms),
        fuzzy_dictionary: cfg.fuzzy_dictionary,
        spoken_line_breaks: cfg.spoken_line_breaks,
    };
    let mut text = cleanup::process_pre_llm(&raw, &ctx);
    let mut ai_cleaned = false;
    let mut cleanup_error = None;
    let mut state = RewriteState::None;
    let mut original = None;
    let mut rewrite_ms = 0.0;
    // Voice rewrite: detected on the deterministic text, before any LLM and before snippets.
    let detected = if text.is_empty() { Detect::NoMatch } else { detect_trigger(cfg, &ctx, &text) };
    match detected {
        Detect::NoMatch => {
            // Only-filler text is already empty here, so the model is never asked to polish nothing.
            if !text.is_empty() && cfg.ai_cleanup.enabled {
                let t = now_ms();
                let reply = (calls.cleanup)(&text);
                let ms = now_ms() - t;
                let failure = match &reply {
                    Ok(_) => {
                        log_info!("ai cleanup answered in {ms:.0} ms");
                        "the model returned nothing".to_string()
                    }
                    Err(e) => {
                        log_warn!("ai cleanup failed after {ms:.0} ms ({e}); keeping the transcript");
                        e.to_string()
                    }
                };
                // never fatal: without a usable answer the transcript (after on-device cleanup) is pasted
                match choose_ai_text(&text, reply) {
                    Some(polished) => {
                        text = polished;
                        ai_cleaned = true;
                    }
                    None => cleanup_error = Some(failure),
                }
            }
        }
        Detect::OnlyPhrase => {
            log_info!("rewrite phrase with nothing to rewrite");
            state = RewriteState::NothingToRewrite;
            text.clear();
        }
        Detect::Match { body, instructions } => {
            let words = rewrite::word_count(&body);
            match &instructions {
                Some(i) => log_info!("rewrite matched; body {words} words, instructions {} words", rewrite::word_count(i)),
                None => log_info!("rewrite matched; body {words} words"),
            }
            // The rewrite replaces the cleanup call: one network round trip, and the rewrite prompt
            // already covers grammar and fillers. A failed rewrite does NOT fall back to cleanup (that
            // could add seconds after a long wait); the deterministic body is pasted.
            if words < REWRITE_MIN_WORDS && instructions.is_none() {
                text = body;
                state = RewriteState::Skipped;
            } else {
                let t = now_ms();
                let reply = (calls.rewrite)(&body, instructions.as_deref());
                rewrite_ms = now_ms() - t;
                match choose_rewrite(reply) {
                    Ok(concise) => {
                        log_info!("rewrite answered in {rewrite_ms:.0} ms");
                        original = Some(body);
                        text = concise;
                        state = RewriteState::Done;
                    }
                    Err(e) => {
                        log_warn!("rewrite failed after {rewrite_ms:.0} ms ({e}); pasting the text as dictated");
                        text = body;
                        state = RewriteState::Failed(e);
                    }
                }
            }
        }
    }
    if !text.is_empty() {
        text = dictionary::expand_snippets(&text, &cfg.snippets);
    }
    Cleaned { raw, text, ai_cleaned, cleanup_error, cleanup_ms: now_ms() - t0, rewrite: state, rewrite_ms, original }
}

/// Look for the rewrite phrase at the end of `text`. The phrase is cleaned like the dictation (same
/// filler mode and language, no dictionary / fuzzy / line-break stages) so "rewrite, um, this
/// concisely" matches, and a phrase that cleaning reduces to nothing usable switches the feature off.
fn detect_trigger(cfg: &Config, ctx: &CleanupCtx, text: &str) -> Detect {
    let Some(phrase) = cfg.rewrite_trigger_effective() else { return Detect::NoMatch };
    let plain = CleanupCtx { dictionary: &[], snippets: &[], speech_ms: None, fuzzy_dictionary: false, spoken_line_breaks: false, ..ctx.clone() };
    let Some(trigger) = Trigger::new(phrase, |p| cleanup::process_pre_llm(p, &plain)) else {
        log_warn!("the rewrite phrase {phrase:?} is not usable (needs two words that survive filler removal); voice rewrite is off");
        return Detect::NoMatch;
    };
    rewrite::detect(text, &trigger)
}

/// The rewrite reply to paste, or the reason to paste the deterministic body instead. An empty
/// answer is a failure here (unlike cleanup, where it keeps the text): the text must have shrunk.
fn choose_rewrite(reply: Result<String, LlmError>) -> Result<String, LlmError> {
    match reply {
        Ok(s) if !s.trim().is_empty() => Ok(s.trim().to_string()),
        Ok(_) => Err(LlmError::InvalidOutput("empty rewrite".into())),
        Err(e) => Err(e),
    }
}

/// Pill card (title, description) for a rewrite that fell back to the text as dictated.
pub fn rewrite_card(e: &LlmError) -> (&'static str, String) {
    const PASTED: &str = "Pasted your text as dictated.";
    match e {
        LlmError::NotConfigured => ("Add an AI key to rewrite", "Pasted your text as dictated. Add a key in Settings.".into()),
        LlmError::Timeout => ("Rewrite timed out", PASTED.into()),
        LlmError::Http { status: 401 | 403, .. } => ("Check your AI key", PASTED.into()),
        LlmError::Http { status: 429, .. } => ("AI provider is rate limiting", PASTED.into()),
        LlmError::Http { .. } | LlmError::Network(_) => ("Couldn't reach the AI provider", PASTED.into()),
        LlmError::InvalidOutput(_) => ("Rewrite didn't look right", PASTED.into()),
    }
}

/// Pill card for a dictation that was only the rewrite phrase.
pub fn nothing_to_rewrite_card(cfg: &Config) -> (&'static str, String) {
    let phrase = cfg.rewrite_trigger.trim();
    let body = if phrase.is_empty() {
        "Say your message first, then the rewrite phrase.".to_string()
    } else {
        format!("Say your message first, then \"{phrase}\".")
    };
    ("Nothing to rewrite", body)
}

/// The AI cleanup reply to paste instead of the deterministic `text`, or None to keep `text`.
///
/// Never fails the paste: an error keeps the deterministic text, and so does an empty reply
/// (`llm::validate_output` allows "" for short inputs, which would otherwise make a one- or
/// two-word dictation such as "ok thanks" disappear). `text` is never empty here.
pub fn choose_ai_text(text: &str, reply: Result<String, LlmError>) -> Option<String> {
    match reply {
        Ok(polished) if !polished.trim().is_empty() => Some(polished.trim().to_string()),
        Ok(_) => {
            log_warn!("ai cleanup returned nothing for {} chars; keeping the deterministic text", text.len());
            None
        }
        Err(_) => None,
    }
}

/// Dictations shorter than this much detected speech are treated as "nothing said".
pub const MIN_SPEECH_MS: u64 = 120;

/// True when the VAD found no usable speech in the recording.
pub fn is_silent(info: &crate::daemon::audio::vad::VadInfo) -> bool {
    info.first_speech_ms.is_none() || info.speech_ms < MIN_SPEECH_MS
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::config::Snippet;

    fn t(text: &str) -> Transcript {
        Transcript { text: text.into(), detected_language: None }
    }

    fn ok_llm(out: &'static str) -> impl Fn(&str) -> Result<String, LlmError> {
        move |_| Ok(out.to_string())
    }

    #[test]
    fn filler_fixture_is_cleaned_without_llm() {
        let cfg = Config::default();
        let c = clean_with(&cfg, &t("Um, so I think, uh, we should like ship the new release on Friday, you know, because the the client is waiting."), 4000, &ok_llm("unused"));
        assert!(!c.ai_cleaned);
        assert_eq!(c.text, "So I think we should ship the new release on Friday because the client is waiting.");
        assert!(c.raw.starts_with("Um,"));
    }

    #[test]
    fn llm_output_replaces_text_only_when_enabled_and_non_empty() {
        let mut cfg = Config::default();
        let c = clean_with(&cfg, &t("um hello there"), 4000, &ok_llm("Polished."));
        assert!(!c.ai_cleaned, "disabled");
        cfg.ai_cleanup.enabled = true;
        let c = clean_with(&cfg, &t("um hello there"), 4000, &ok_llm("  Polished.\n"));
        assert_eq!((c.text.as_str(), c.ai_cleaned), ("Polished.", true));
        let c = clean_with(&cfg, &t("um hello there"), 4000, &ok_llm("   "));
        assert_eq!((c.text.as_str(), c.ai_cleaned), ("Hello there", false), "empty output keeps the deterministic text");
    }

    #[test]
    fn llm_failure_never_fails_the_dictation() {
        let mut cfg = Config::default();
        cfg.ai_cleanup.enabled = true;
        for err in [LlmError::Timeout, LlmError::NotConfigured, LlmError::Network("x".into()), LlmError::InvalidOutput("dup".into())] {
            let c = clean_with(&cfg, &t("um hello there"), 4000, &move |_| Err(err.clone()));
            assert_eq!((c.text.as_str(), c.ai_cleaned), ("Hello there", false));
        }
    }

    #[test]
    fn snippets_expand_last_and_after_the_llm() {
        let mut cfg = Config::default();
        cfg.snippets = vec![Snippet { trigger: "my sig".into(), replacement: "Best regards, Sam".into() }];
        let c = clean_with(&cfg, &t("thanks my sig"), 4000, &ok_llm("unused"));
        assert!(c.text.contains("Best regards, Sam"), "{}", c.text);
        cfg.ai_cleanup.enabled = true;
        let c = clean_with(&cfg, &t("thanks my sig"), 4000, &ok_llm("Thanks, my sig"));
        assert!(c.text.contains("Best regards, Sam") && c.ai_cleaned, "{}", c.text);
    }

    #[test]
    fn only_fillers_leaves_nothing_and_skips_the_llm() {
        let mut cfg = Config::default();
        cfg.ai_cleanup.enabled = true;
        let c = clean_with(&cfg, &t("um uh"), 500, &|_| panic!("the LLM must not be called for empty text"));
        assert_eq!(c.text, "");
    }

    #[test]
    fn hallucination_dropped_only_for_near_silence() {
        let cfg = Config::default();
        let near = clean_with(&cfg, &t("Thank you for watching!"), 200, &ok_llm("x"));
        assert_eq!(near.text, "");
        let real = clean_with(&cfg, &t("Thank you for watching!"), 3000, &ok_llm("x"));
        assert_eq!(real.text, "Thank you for watching!");
    }

    // ---- voice rewrite ----------------------------------------------------------------------

    use std::cell::RefCell;

    /// Records every call; answers with fixed replies.
    struct Calls {
        cleanup_seen: RefCell<Vec<String>>,
        rewrite_seen: RefCell<Vec<String>>,
        instructions_seen: RefCell<Vec<Option<String>>>,
        cleanup_reply: Result<String, LlmError>,
        rewrite_reply: Result<String, LlmError>,
    }

    impl Calls {
        fn new(rewrite_reply: Result<String, LlmError>) -> Calls {
            Calls { cleanup_seen: RefCell::default(), rewrite_seen: RefCell::default(), instructions_seen: RefCell::default(), cleanup_reply: Ok("CLEANUP".into()), rewrite_reply }
        }
        fn run(&self, cfg: &Config, spoken: &str) -> Cleaned {
            let cleanup = |t: &str| {
                self.cleanup_seen.borrow_mut().push(t.to_string());
                self.cleanup_reply.clone()
            };
            let rewrite = |t: &str, i: Option<&str>| {
                self.rewrite_seen.borrow_mut().push(t.to_string());
                self.instructions_seen.borrow_mut().push(i.map(str::to_string));
                self.rewrite_reply.clone()
            };
            clean_with_calls(cfg, &t(spoken), 4000, &LlmCalls { cleanup: &cleanup, rewrite: &rewrite })
        }
    }

    const LONG: &str = "so I was thinking that maybe we could move the meeting from Thursday to Friday afternoon if that works for everyone";
    const SHORT: &str = "Move the meeting to Friday.";

    #[test]
    fn phrase_sends_the_body_to_the_rewrite_and_skips_cleanup() {
        let mut cfg = Config::default();
        cfg.ai_cleanup.enabled = true; // even then the cleanup call is replaced
        let calls = Calls::new(Ok(format!(" {SHORT}\n")));
        let c = calls.run(&cfg, &format!("{LONG}. Rewrite this concisely."));
        assert_eq!(c.text, SHORT);
        assert_eq!(c.rewrite, RewriteState::Done);
        assert!(!c.ai_cleaned, "a rewrite is not an AI cleanup");
        assert!(calls.cleanup_seen.borrow().is_empty());
        let seen = calls.rewrite_seen.borrow();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0], "So I was thinking that maybe we could move the meeting from Thursday to Friday afternoon if that works for everyone.");
        assert!(!seen[0].to_lowercase().contains("rewrite"), "the BODY is sent, not the phrase");
        assert_eq!(c.original.as_deref(), Some(seen[0].as_str()));
        assert!(c.raw.ends_with("Rewrite this concisely."), "raw keeps the full STT text: {}", c.raw);
    }

    #[test]
    fn rewrite_works_with_ai_cleanup_off_and_never_without_the_phrase() {
        let cfg = Config::default();
        assert!(!cfg.ai_cleanup.enabled);
        let calls = Calls::new(Ok(SHORT.into()));
        let c = calls.run(&cfg, &format!("{LONG} rewrite this concisely"));
        assert_eq!((c.text.as_str(), &c.rewrite), (SHORT, &RewriteState::Done));
        // No phrase: nothing is sent anywhere with cleanup off, and cleanup runs as before when on.
        let calls = Calls::new(Ok(SHORT.into()));
        let c = calls.run(&cfg, LONG);
        assert_eq!(c.rewrite, RewriteState::None);
        assert!(calls.rewrite_seen.borrow().is_empty() && calls.cleanup_seen.borrow().is_empty());
        let mut on = cfg.clone();
        on.ai_cleanup.enabled = true;
        let calls = Calls::new(Ok(SHORT.into()));
        let c = calls.run(&on, LONG);
        assert_eq!((c.text.as_str(), c.ai_cleaned, &c.rewrite), ("CLEANUP", true, &RewriteState::None));
        assert!(calls.rewrite_seen.borrow().is_empty());
        assert_eq!(c.original, None);
    }

    #[test]
    fn a_failed_rewrite_pastes_the_deterministic_body() {
        let mut cfg = Config::default();
        cfg.ai_cleanup.enabled = true;
        for err in [
            LlmError::NotConfigured,
            LlmError::Timeout,
            LlmError::Http { status: 500, detail: "x".into() },
            LlmError::Http { status: 401, detail: "x".into() },
            LlmError::Network("down".into()),
            LlmError::InvalidOutput("refusal".into()),
        ] {
            let calls = Calls::new(Err(err.clone()));
            let c = calls.run(&cfg, &format!("{LONG}, and then rewrite this concisely."));
            assert_eq!(c.text, "So I was thinking that maybe we could move the meeting from Thursday to Friday afternoon if that works for everyone");
            assert_eq!(c.rewrite, RewriteState::Failed(err.clone()));
            assert!(!c.ai_cleaned && c.original.is_none());
            assert!(calls.cleanup_seen.borrow().is_empty(), "no second call after a failed rewrite");
            assert_eq!(calls.rewrite_seen.borrow().len(), 1, "no retries");
            let (title, desc) = rewrite_card(&err);
            assert!(!title.is_empty() && desc.starts_with("Pasted your text as dictated"), "{title}: {desc}");
        }
    }

    #[test]
    fn an_empty_rewrite_reply_is_a_failure_not_an_empty_paste() {
        let cfg = Config::default();
        let c = Calls::new(Ok("  \n".into())).run(&cfg, &format!("{LONG} rewrite this concisely"));
        assert!(matches!(c.rewrite, RewriteState::Failed(LlmError::InvalidOutput(_))));
        assert!(c.text.starts_with("So I was thinking"), "{}", c.text);
    }

    #[test]
    fn only_the_phrase_means_nothing_to_rewrite() {
        let mut cfg = Config::default();
        cfg.ai_cleanup.enabled = true;
        for spoken in ["Rewrite this concisely.", "Please rewrite this concisely", "um rewrite this concisely thanks"] {
            let calls = Calls::new(Ok("unused".into()));
            let c = calls.run(&cfg, spoken);
            assert_eq!((c.text.as_str(), &c.rewrite), ("", &RewriteState::NothingToRewrite), "{spoken}");
            assert!(calls.cleanup_seen.borrow().is_empty() && calls.rewrite_seen.borrow().is_empty());
        }
        let (title, body) = nothing_to_rewrite_card(&cfg);
        assert_eq!(title, "Nothing to rewrite");
        assert_eq!(body, "Say your message first, then \"rewrite this concisely\".");
    }

    #[test]
    fn words_after_the_phrase_go_to_the_model_as_instructions() {
        let cfg = Config::default();
        let calls = Calls::new(Ok("Hola equipo.".into()));
        let c = calls.run(&cfg, &format!("{LONG}. Rewrite this concisely and translate it to Spanish."));
        assert_eq!((c.text.as_str(), &c.rewrite), ("Hola equipo.", &RewriteState::Done));
        let sent = calls.rewrite_seen.borrow()[0].to_lowercase();
        assert!(sent.ends_with("everyone.") && !sent.contains("rewrite") && !sent.contains("spanish"), "only the body is the source text: {sent}");
        assert_eq!(calls.instructions_seen.borrow()[0].as_deref(), Some("translate it to Spanish."));
        assert!(!c.original.as_deref().unwrap().to_lowercase().contains("spanish"));
        // A custom phrase works the same way.
        let cfg = Config { rewrite_trigger: "fix this up".into(), ..Config::default() };
        let calls = Calls::new(Ok("Hi.".into()));
        assert_eq!(calls.run(&cfg, "hey there fix this up as a formal email").rewrite, RewriteState::Done);
        assert_eq!(calls.rewrite_seen.borrow()[0], "Hey there", "instructions lift the short-body limit");
        assert_eq!(calls.instructions_seen.borrow()[0].as_deref(), Some("as a formal email"));
        // Without extra words nothing changes.
        let calls = Calls::new(Ok(SHORT.into()));
        calls.run(&Config::default(), &format!("{LONG} rewrite this concisely, thanks"));
        assert_eq!(calls.instructions_seen.borrow()[0], None);
    }

    #[test]
    fn a_short_body_is_pasted_without_a_call() {
        let cfg = Config::default();
        for (spoken, want) in [("ok rewrite this concisely", "Ok"), ("Send it Friday, rewrite this concisely.", "Send it Friday"), ("Yes do that. Rewrite this concisely", "Yes do that.")] {
            let calls = Calls::new(Err(LlmError::Timeout));
            let c = calls.run(&cfg, spoken);
            assert_eq!((c.text.as_str(), &c.rewrite), (want, &RewriteState::Skipped), "{spoken}");
            assert!(calls.rewrite_seen.borrow().is_empty() && calls.cleanup_seen.borrow().is_empty());
        }
        // Four words is the first size that goes out.
        let calls = Calls::new(Ok("Ship it Friday morning.".into()));
        let c = calls.run(&cfg, "ship it on friday rewrite this concisely");
        assert_eq!((c.text.as_str(), &c.rewrite), ("Ship it Friday morning.", &RewriteState::Done));
    }

    #[test]
    fn disabled_or_empty_phrase_leaves_the_text_alone() {
        let mut cfg = Config::default();
        cfg.ai_cleanup.enabled = true;
        let spoken = format!("{LONG}. Rewrite this concisely.");
        let mut off = cfg.clone();
        off.ai_cleanup.rewrite_enabled = false;
        let calls = Calls::new(Ok("unused".into()));
        let c = calls.run(&off, &spoken);
        assert_eq!(c.rewrite, RewriteState::None);
        assert_eq!(c.text, "CLEANUP", "cleanup runs as before and sees the phrase");
        assert!(calls.cleanup_seen.borrow()[0].ends_with("Rewrite this concisely."));
        let mut empty = cfg.clone();
        empty.rewrite_trigger = String::new();
        let c = Calls::new(Ok("unused".into())).run(&empty, &spoken);
        assert_eq!(c.rewrite, RewriteState::None);
        // A one-word phrase is unusable: the feature is off for the dictation, text untouched.
        let one = Config { rewrite_trigger: "concisely".into(), ..Config::default() };
        let calls = Calls::new(Ok("unused".into()));
        let c = calls.run(&one, &spoken);
        assert_eq!(c.rewrite, RewriteState::None);
        assert!(c.text.ends_with("Rewrite this concisely."), "{}", c.text);
        assert!(calls.rewrite_seen.borrow().is_empty());
    }

    #[test]
    fn a_custom_phrase_and_a_phrase_with_a_filler_inside() {
        let mut cfg = Config { rewrite_trigger: "make it short".into(), ..Config::default() };
        let c = Calls::new(Ok(SHORT.into())).run(&cfg, &format!("{LONG}. Make it short."));
        assert_eq!(c.rewrite, RewriteState::Done);
        let c = Calls::new(Ok(SHORT.into())).run(&Config::default(), &format!("{LONG}. Rewrite, um, this concisely."));
        assert_eq!(c.rewrite, RewriteState::Done, "the filler inside the spoken phrase is removed first");
        // The configured phrase carries a filler too: cleaned the same way, still matches.
        cfg.rewrite_trigger = "rewrite, um, this concisely".into();
        let c = Calls::new(Ok(SHORT.into())).run(&cfg, &format!("{LONG}. Rewrite this concisely."));
        assert_eq!(c.rewrite, RewriteState::Done);
    }

    #[test]
    fn snippets_expand_after_the_rewrite_and_after_a_fallback() {
        let cfg = Config { snippets: vec![Snippet { trigger: "my sig".into(), replacement: "Best regards, Sam".into() }], ..Config::default() };
        // The model never sees the replacement, only the trigger words.
        let calls = Calls::new(Ok("Move the meeting to Friday, my sig".into()));
        let c = calls.run(&cfg, &format!("{LONG} and thanks my sig. Rewrite this concisely."));
        assert!(c.text.ends_with("Best regards, Sam"), "{}", c.text);
        assert!(!calls.rewrite_seen.borrow()[0].contains("Best regards"));
        assert_eq!(c.original.as_deref().map(|o| o.contains("my sig")), Some(true), "history keeps the unexpanded text");
        let c = Calls::new(Err(LlmError::Timeout)).run(&cfg, &format!("{LONG} and thanks my sig. Rewrite this concisely."));
        assert!(c.text.contains("Best regards, Sam") && !c.text.contains("my sig") && matches!(c.rewrite, RewriteState::Failed(_)), "{}", c.text);
    }

    #[test]
    fn spoken_line_breaks_in_the_body_reach_the_rewrite() {
        let cfg = Config { spoken_line_breaks: true, ..Config::default() };
        let calls = Calls::new(Ok("Hi Sam,\nMove it to Friday.".into()));
        let c = calls.run(&cfg, "hi Sam new line could we please move the weekly meeting to Friday afternoon rewrite this concisely");
        assert_eq!(c.rewrite, RewriteState::Done);
        assert!(calls.rewrite_seen.borrow()[0].contains('\n'), "{:?}", calls.rewrite_seen.borrow()[0]);
        assert_eq!(c.text, "Hi Sam,\nMove it to Friday.");
    }

    #[test]
    fn dictionary_words_are_fixed_before_the_body_goes_out() {
        let cfg = Config { dictionary: vec!["Kubernetes".into()], ..Config::default() };
        let calls = Calls::new(Ok(SHORT.into()));
        calls.run(&cfg, "please restart the kubernetes cluster before the release on friday morning rewrite this concisely");
        assert!(calls.rewrite_seen.borrow()[0].contains("Kubernetes"));
    }

    #[test]
    fn silence_and_fillers_never_reach_the_rewrite() {
        let cfg = Config::default();
        let calls = Calls::new(Ok("x".into()));
        let c = calls.run(&cfg, "um uh");
        assert_eq!((c.text.as_str(), &c.rewrite), ("", &RewriteState::None));
        let c = calls.run(&cfg, "");
        assert_eq!(c.rewrite, RewriteState::None);
        assert!(calls.rewrite_seen.borrow().is_empty());
    }

    #[test]
    fn clean_with_uses_one_closure_for_both_calls() {
        let cfg = Config::default();
        let c = clean_with(&cfg, &t(&format!("{LONG} rewrite this concisely")), 4000, &ok_llm("Short."));
        assert_eq!((c.text.as_str(), &c.rewrite), ("Short.", &RewriteState::Done));
    }

    #[test]
    fn rewrite_state_labels_and_cards_cover_every_error() {
        assert_eq!(RewriteState::None.label(), "none");
        assert_eq!(RewriteState::Failed(LlmError::Timeout).label(), "failed");
        assert_eq!(RewriteState::NothingToRewrite.label(), "nothing");
        let titles: Vec<&str> = [
            LlmError::NotConfigured,
            LlmError::Timeout,
            LlmError::Http { status: 401, detail: String::new() },
            LlmError::Http { status: 403, detail: String::new() },
            LlmError::Http { status: 429, detail: String::new() },
            LlmError::Http { status: 500, detail: String::new() },
            LlmError::Network(String::new()),
            LlmError::InvalidOutput(String::new()),
        ]
        .iter()
        .map(|e| rewrite_card(e).0)
        .collect();
        assert_eq!(
            titles,
            [
                "Add an AI key to rewrite",
                "Rewrite timed out",
                "Check your AI key",
                "Check your AI key",
                "AI provider is rate limiting",
                "Couldn't reach the AI provider",
                "Couldn't reach the AI provider",
                "Rewrite didn't look right"
            ]
        );
        assert!(rewrite_card(&LlmError::NotConfigured).1.contains("Add a key in Settings"));
        let cfg = Config { rewrite_trigger: String::new(), ..Config::default() };
        assert!(nothing_to_rewrite_card(&cfg).1.contains("rewrite phrase"));
        for (title, body) in [rewrite_card(&LlmError::Timeout), ("Nothing to rewrite", nothing_to_rewrite_card(&Config::default()).1)] {
            assert!(auto_dismiss_ok(title, &body));
        }
    }

    fn auto_dismiss_ok(title: &str, body: &str) -> bool {
        title.chars().count() <= 40 && body.chars().count() <= 100
    }

    struct Fake(Result<Transcript, SttError>);
    impl StreamingSession for Fake {
        fn push(&mut self, _: &[i16]) {}
        fn finish(self: Box<Self>) -> Result<Transcript, SttError> {
            self.0
        }
        fn cancel(self: Box<Self>) {}
    }

    #[test]
    fn stream_result_is_used_without_calling_batch() {
        let r = transcribe_with(Some(Box::new(Fake(Ok(t("streamed"))))), || panic!("no batch needed"));
        assert_eq!(r.unwrap().text, "streamed");
    }

    #[test]
    fn stream_failure_falls_back_to_batch() {
        let err = SttError::new(SttErrorCode::Unreachable, "down");
        let r = transcribe_with(Some(Box::new(Fake(Err(err)))), || Ok(t("batch")));
        assert_eq!(r.unwrap().text, "batch");
        let empty = transcribe_with(Some(Box::new(Fake(Ok(t("  "))))), || Ok(t("batch")));
        assert_eq!(empty.unwrap().text, "batch");
    }

    #[test]
    fn streaming_only_models_report_the_stream_error() {
        let err = SttError::new(SttErrorCode::AuthFailed, "bad key");
        let r = transcribe_with(Some(Box::new(Fake(Err(err)))), || Err(SttError::new(SttErrorCode::StreamingOnly, "streaming only")));
        assert_eq!(r.unwrap_err().code, SttErrorCode::AuthFailed);
        let r = transcribe_with(Some(Box::new(Fake(Ok(t(""))))), || Err(SttError::new(SttErrorCode::StreamingOnly, "streaming only")));
        assert_eq!(r.unwrap_err().code, SttErrorCode::NoSpeech);
    }

    #[test]
    fn no_speech_and_cancel_do_not_retry() {
        for code in [SttErrorCode::NoSpeech, SttErrorCode::Cancelled] {
            let r = transcribe_with(Some(Box::new(Fake(Err(SttError::new(code, "x"))))), || panic!("must not retry"));
            assert_eq!(r.unwrap_err().code, code);
        }
    }

    #[test]
    fn empty_ai_reply_keeps_short_dictations() {
        assert_eq!(choose_ai_text("Ok thanks", Ok(String::new())), None);
        assert_eq!(choose_ai_text("Ok thanks", Ok(" \n ".into())), None);
        assert_eq!(choose_ai_text("Ok thanks", Err(LlmError::Timeout)), None);
        assert_eq!(choose_ai_text("ok thanks", Ok(" Okay, thanks.\n".into())).as_deref(), Some("Okay, thanks."));
        let mut cfg = Config::default();
        cfg.ai_cleanup.enabled = true;
        let c = clean_with(&cfg, &t("ok thanks"), 900, &|_| Ok(String::new()));
        assert_eq!((c.text.as_str(), c.ai_cleaned), ("Ok thanks", false));
    }

    #[test]
    fn dropped_stream_falls_back_to_a_real_batch_request() {
        // A stream that lost its socket mid-recording must not commit a truncated transcript:
        // the full recording goes to the batch endpoint (here the in-repo mock server).
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", l.local_addr().unwrap());
        let text = std::sync::Arc::new("the whole sentence".to_string());
        std::thread::spawn(move || super::super::cli_cmds::serve_forever(l, std::time::Duration::ZERO, text));
        let audio = Audio { samples: vec![500; 16_000], sample_rate: 16_000 };
        let opts = stt::SttOptions { model: "whisper-1".into(), timeout_ms: 5_000, ..stt::SttOptions::default() };
        let creds = stt::Credentials { api_key: Some("test-key-123456".into()), base_url: Some(base), extra: vec![] };
        let lost = SttError::new(SttErrorCode::Unreachable, "connection lost while recording");
        let r = transcribe_with(Some(Box::new(Fake(Err(lost)))), || {
            stt::transcribe_with(crate::core::config::SttProviderId::Openai, &audio, &opts, &creds)
        });
        assert_eq!(r.unwrap().text, "the whole sentence");
    }

    #[test]
    fn vad_gate() {
        use crate::daemon::audio::vad::VadInfo;
        assert!(is_silent(&VadInfo::default()));
        assert!(is_silent(&VadInfo { speech_ms: 60, first_speech_ms: Some(0), last_speech_ms: Some(60), peak_rms: 0.1 }));
        assert!(!is_silent(&VadInfo { speech_ms: 400, first_speech_ms: Some(0), last_speech_ms: Some(400), peak_rms: 0.1 }));
    }
}
