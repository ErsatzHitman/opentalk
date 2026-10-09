//! The text pipeline between "audio captured" and "text ready to paste":
//! transcription (streaming session or batch) -> deterministic cleanup -> optional AI cleanup
//! (never fatal) -> snippets. Shared by the daemon worker and `--transcribe-file`.
use crate::core::cleanup::{self, CleanupCtx};
use crate::core::config::Config;
use crate::core::dictionary;
use crate::core::timing::now_ms;
use crate::llm::LlmError;
use crate::stt::{self, Audio, StreamingSession, SttError, SttErrorCode, Transcript};
use crate::{log_info, log_warn};

/// Result of the text stages.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Cleaned {
    /// Recognizer output, untouched.
    pub raw: String,
    /// Final text ("" when nothing but fillers / hallucination remained).
    pub text: String,
    pub ai_cleaned: bool,
    pub cleanup_ms: f64,
}

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
    clean_with(cfg, transcript, speech_ms, &|text| crate::llm::cleanup(cfg, text, &cfg.dictionary))
}

/// [`clean`] with the LLM call injected (tests).
pub fn clean_with(
    cfg: &Config,
    transcript: &Transcript,
    speech_ms: u64,
    llm: &dyn Fn(&str) -> Result<String, LlmError>,
) -> Cleaned {
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
    // Only-filler text is already empty here, so the model is never asked to polish nothing.
    if !text.is_empty() && cfg.ai_cleanup.enabled {
        let t = now_ms();
        let reply = llm(&text);
        let ms = now_ms() - t;
        match &reply {
            Ok(_) => log_info!("ai cleanup answered in {ms:.0} ms"),
            Err(e) => log_warn!("ai cleanup failed after {ms:.0} ms ({e}); keeping the deterministic text"),
        }
        if let Some(polished) = choose_ai_text(&text, reply) {
            text = polished;
            ai_cleaned = true;
        }
    }
    if !text.is_empty() {
        text = dictionary::expand_snippets(&text, &cfg.snippets);
    }
    Cleaned { raw, text, ai_cleaned, cleanup_ms: now_ms() - t0 }
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
