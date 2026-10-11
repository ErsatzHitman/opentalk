//! Cloud speech-to-text. Batch providers implement `BatchProvider`; realtime ones
//! implement `StreamingProvider`. `transcribe()` dispatches on the active config.
//! TASK stt-batch owns batch providers + errors + dispatch; TASK stt-stream owns streaming + Corti.
pub mod errors;
pub mod local;
pub mod providers;
pub mod registry;
pub mod streaming;

use crate::core::config::{Config, SttProviderId};
#[allow(unused_imports)]
pub use errors::{SttError, SttErrorCode};

/// 16-bit PCM mono audio.
#[derive(Clone, Debug, Default)]
pub struct Audio {
    pub samples: Vec<i16>,
    pub sample_rate: u32,
}

impl Audio {
    pub fn duration_ms(&self) -> u64 {
        if self.sample_rate == 0 { 0 } else { self.samples.len() as u64 * 1000 / self.sample_rate as u64 }
    }

    /// Canonical 44-byte-header PCM WAV.
    pub fn to_wav(&self) -> Vec<u8> {
        let data_len = (self.samples.len() * 2) as u32;
        let mut v = Vec::with_capacity(44 + data_len as usize);
        v.extend_from_slice(b"RIFF");
        v.extend_from_slice(&(36 + data_len).to_le_bytes());
        v.extend_from_slice(b"WAVEfmt ");
        v.extend_from_slice(&16u32.to_le_bytes());
        v.extend_from_slice(&1u16.to_le_bytes()); // PCM
        v.extend_from_slice(&1u16.to_le_bytes()); // mono
        v.extend_from_slice(&self.sample_rate.to_le_bytes());
        v.extend_from_slice(&(self.sample_rate * 2).to_le_bytes());
        v.extend_from_slice(&2u16.to_le_bytes());
        v.extend_from_slice(&16u16.to_le_bytes());
        v.extend_from_slice(b"data");
        v.extend_from_slice(&data_len.to_le_bytes());
        for s in &self.samples {
            v.extend_from_slice(&s.to_le_bytes());
        }
        v
    }

    /// Parse a PCM16 WAV (multi-channel is averaged to mono). Used by `--transcribe-file` and tests.
    pub fn from_wav(bytes: &[u8]) -> Option<Audio> {
        if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
            return None;
        }
        let (mut pos, mut rate, mut channels, mut bits) = (12usize, 0u32, 1u16, 16u16);
        while pos + 8 <= bytes.len() {
            let id = &bytes[pos..pos + 4];
            let len = u32::from_le_bytes(bytes[pos + 4..pos + 8].try_into().ok()?) as usize;
            let body = pos + 8;
            if id == b"fmt " && body + 16 <= bytes.len() {
                channels = u16::from_le_bytes([bytes[body + 2], bytes[body + 3]]);
                rate = u32::from_le_bytes(bytes[body + 4..body + 8].try_into().ok()?);
                bits = u16::from_le_bytes([bytes[body + 14], bytes[body + 15]]);
            } else if id == b"data" {
                if bits != 16 || channels == 0 {
                    return None;
                }
                let end = (body + len).min(bytes.len());
                let frames: Vec<i16> = bytes[body..end]
                    .chunks_exact(2 * channels as usize)
                    .map(|f| {
                        let sum: i32 = f.chunks_exact(2).map(|c| i16::from_le_bytes([c[0], c[1]]) as i32).sum();
                        (sum / channels as i32) as i16
                    })
                    .collect();
                return Some(Audio { samples: frames, sample_rate: rate });
            }
            pos = body + len + (len & 1);
        }
        None
    }
}

/// Per-request options derived from config.
#[derive(Clone, Debug, Default)]
pub struct SttOptions {
    pub model: String,
    /// None = auto-detect.
    pub language: Option<String>,
    /// Dictionary + snippet trigger hints (raw words; providers apply their own budgets).
    pub hints: Vec<String>,
    pub timeout_ms: u64,
}

#[derive(Clone, Debug, Default)]
pub struct Transcript {
    pub text: String,
    pub detected_language: Option<String>,
}

/// Credentials resolved from `secrets` + config for one provider.
#[derive(Clone, Debug, Default)]
pub struct Credentials {
    pub api_key: Option<String>,
    pub base_url: Option<String>,
    /// Provider-specific extras (e.g. corti client_secret/environment/tenant).
    pub extra: Vec<(String, String)>,
}

pub trait BatchProvider: Send + Sync {
    #[allow(dead_code)] // implemented by every provider and used by tests; dispatch keys on `SttProviderId` directly
    fn id(&self) -> SttProviderId;
    /// URL whose origin should be pre-warmed when recording starts.
    fn prewarm_url(&self, creds: &Credentials) -> Option<String>;
    fn transcribe(&self, audio: &Audio, opts: &SttOptions, creds: &Credentials) -> Result<Transcript, SttError>;
}

/// A live streaming session: feed 16 kHz mono PCM while recording, then finish.
pub trait StreamingSession: Send {
    fn push(&mut self, pcm: &[i16]);
    fn finish(self: Box<Self>) -> Result<Transcript, SttError>;
    fn cancel(self: Box<Self>);
}

pub trait StreamingProvider: Send + Sync {
    #[allow(dead_code)] // implemented by every provider and used by tests; dispatch keys on `SttProviderId` directly
    fn id(&self) -> SttProviderId;
    fn start(&self, opts: &SttOptions, creds: &Credentials) -> Result<Box<dyn StreamingSession>, SttError>;
}

/// Test hook: points every provider at this base URL (an in-process mock server).
pub const ENV_BASE_URL: &str = "OPENTALK_STT_BASE_URL";

/// Base URL override: `OPENTALK_STT_BASE_URL`, else the configured endpoint for Custom.
fn base_override(cfg: &Config) -> Option<String> {
    let from_env = std::env::var(ENV_BASE_URL).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
    from_env.or_else(|| {
        (cfg.stt.provider == SttProviderId::Custom)
            .then(|| cfg.stt.custom_base_url.trim().to_string())
            .filter(|v| !v.is_empty())
    })
}

/// Corti settings that are not secrets.
fn corti_target(cfg: &Config) -> Vec<(String, String)> {
    vec![
        ("environment".into(), cfg.stt.corti_environment.trim().to_ascii_lowercase()),
        ("tenant".into(), cfg.stt.corti_tenant.trim().to_string()),
    ]
}

/// Resolve credentials for the active provider from secrets + config.
pub fn credentials(cfg: &Config) -> Credentials {
    use crate::core::secrets::{self, SecretName};
    let id = cfg.stt.provider;
    let read = |name: &str| secrets::get(name).map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
    let mut extra = Vec::new();
    if id == SttProviderId::Corti {
        extra = corti_target(cfg);
        if let Some(secret) = read(SecretName::CORTI_CLIENT_SECRET) {
            extra.push(("client_secret".into(), secret));
        }
    }
    Credentials { api_key: read(registry::provider(id).secret), base_url: base_override(cfg), extra }
}

pub fn options(cfg: &Config) -> SttOptions {
    let language = cfg.language.trim();
    SttOptions {
        model: cfg.stt_model(),
        language: (!language.is_empty() && !language.eq_ignore_ascii_case("auto")).then(|| language.to_string()),
        hints: crate::core::dictionary::hint_words(&cfg.dictionary, &cfg.snippets),
        timeout_ms: cfg.stt.timeout_ms,
    }
}

/// Batch transcription with the active provider (streaming-only providers return
/// `SttErrorCode::StreamingOnly`). Applies the dictionary-echo filter.
pub fn transcribe(cfg: &Config, audio: &Audio) -> Result<Transcript, SttError> {
    if local::is_local(cfg) {
        return local::transcribe(cfg, audio);
    }
    transcribe_with(cfg.stt.provider, audio, &options(cfg), &credentials(cfg))
}

/// `transcribe` with explicit options and credentials.
pub fn transcribe_with(id: SttProviderId, audio: &Audio, opts: &SttOptions, creds: &Credentials) -> Result<Transcript, SttError> {
    let info = registry::provider(id);
    if registry::model(id, &opts.model).is_some_and(|m| m.streaming_only) {
        return Err(errors::streaming_only(info.name, &opts.model));
    }
    if audio.samples.is_empty() {
        return Err(errors::no_speech());
    }
    let started = crate::core::timing::now_ms();
    let mut transcript = provider_for(id).transcribe(audio, opts, creds)?;
    crate::log_debug!(
        "stt {} {} -> {} chars in {:.0} ms",
        info.name,
        opts.model,
        transcript.text.len(),
        crate::core::timing::now_ms() - started
    );

    let text = transcript.text.trim();
    if text.is_empty() {
        return Err(errors::no_speech());
    }
    // Whisper-family models sometimes answer with the dictionary prompt itself on near-silence.
    // Corti is sent no dictionary, so it cannot echo one.
    if id != SttProviderId::Corti && !opts.hints.is_empty() {
        let prompt = crate::core::dictionary::build_prompt(&opts.hints, providers::prompt_limit(id == SttProviderId::Groq, &opts.model));
        if crate::core::dictionary::is_prompt_echo(text, &prompt) {
            return Err(errors::no_speech());
        }
    }
    transcript.text = text.to_string();
    Ok(transcript)
}

/// Start a streaming session if the active provider/model streams and the config prefers it.
pub fn start_stream(cfg: &Config) -> Option<Result<Box<dyn StreamingSession>, SttError>> {
    if local::is_local(cfg) {
        return local::start_stream(cfg);
    }
    streaming::start(cfg)
}

/// Pre-warm the active provider's connection (called at hotkey press). Never decrypts
/// secrets: only the endpoint matters.
pub fn prewarm(cfg: &Config) {
    local::prewarm(cfg);
    if local::is_local(cfg) {
        return;
    }
    let id = cfg.stt.provider;
    let streams = registry::model(id, &cfg.stt_model()).is_some_and(|m| m.streaming_only || (m.streaming && cfg.stt.prefer_streaming));
    if streams {
        return; // the streaming session opens its own connection
    }
    let extra = if id == SttProviderId::Corti { corti_target(cfg) } else { Vec::new() };
    let creds = Credentials { api_key: None, base_url: base_override(cfg), extra };
    if let Some(url) = provider_for(id).prewarm_url(&creds) {
        crate::core::http::prewarm(&url);
    }
}

pub fn provider_for(id: SttProviderId) -> &'static dyn BatchProvider {
    providers::get(id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use providers::testutil::{self, Parts, Resp};

    #[test]
    fn dispatch_trims_the_transcript() {
        let mock = testutil::serve(vec![Resp::json(200, r#"{"text":" Hello world. "}"#)]);
        let creds = testutil::creds(&mock, "gsk_abcdef123456");
        let t = transcribe_with(SttProviderId::Groq, &testutil::audio(), &testutil::opts("whisper-large-v3"), &creds).unwrap();
        assert_eq!(t.text, "Hello world.");
        assert_eq!(mock.one().path, "/audio/transcriptions");
    }

    #[test]
    fn empty_text_and_empty_audio_are_no_speech() {
        let mock = testutil::serve(vec![Resp::json(200, r#"{"text":"  "}"#)]);
        let creds = testutil::creds(&mock, "gsk_abcdef123456");
        let e = transcribe_with(SttProviderId::Groq, &testutil::audio(), &testutil::opts("m"), &creds).unwrap_err();
        assert_eq!(e.code, SttErrorCode::NoSpeech);
        let e = transcribe_with(SttProviderId::Groq, &Audio::default(), &testutil::opts("m"), &creds).unwrap_err();
        assert_eq!(e.code, SttErrorCode::NoSpeech);
        assert_eq!(mock.requests().len(), 1, "empty audio never reaches the network");
    }

    #[test]
    fn streaming_only_models_never_hit_the_network() {
        let creds = Credentials { api_key: Some("k-12345".into()), base_url: Some("http://127.0.0.1:1".into()), extra: vec![] };
        for (id, model) in [
            (SttProviderId::Gemini, "gemini-3.5-transcribe-live"),
            (SttProviderId::Assemblyai, "universal-3-5-pro"),
            (SttProviderId::Assemblyai, "anything"),
        ] {
            let e = transcribe_with(id, &testutil::audio(), &testutil::opts(model), &creds).unwrap_err();
            assert_eq!(e.code, SttErrorCode::StreamingOnly, "{id:?} {model}");
        }
    }

    #[test]
    fn missing_key_is_not_configured_for_every_keyed_provider() {
        for p in registry::PROVIDERS.iter().filter(|p| p.id != SttProviderId::Custom && p.id != SttProviderId::Assemblyai) {
            let e = transcribe_with(p.id, &testutil::audio(), &testutil::opts(registry::default_model(p.id)), &Credentials::default()).unwrap_err();
            assert_eq!(e.code, SttErrorCode::NotConfigured, "{}", p.name);
            assert!(e.message.contains(p.name), "{}", e.message);
        }
    }

    #[test]
    fn every_batch_provider_parses_a_realistic_reply() {
        // (provider, model, reply body, expected text)
        let cases: [(SttProviderId, &str, &str, &str); 8] = [
            (SttProviderId::Openai, "whisper-1", r#"{"text":"one"}"#, "one"),
            (SttProviderId::Groq, "whisper-large-v3", r#"{"text":"two"}"#, "two"),
            (SttProviderId::Xai, "grok-stt", r#"{"text":"three"}"#, "three"),
            (SttProviderId::Mistral, "voxtral-mini-latest", r#"{"text":"four"}"#, "four"),
            (SttProviderId::Gemini, "gemini-3.5-transcribe", r#"{"status":"completed","output_text":"five"}"#, "five"),
            (SttProviderId::Deepgram, "nova-3", r#"{"results":{"channels":[{"alternatives":[{"transcript":"six"}]}]}}"#, "six"),
            (SttProviderId::Tinfoil, "voxtral-small-24b", r#"{"text":"seven"}"#, "seven"),
            (SttProviderId::Openrouter, "m:free", r#"{"choices":[{"message":{"content":"eight"}}]}"#, "eight"),
        ];
        for (id, model, body, want) in cases {
            let mock = testutil::serve(vec![Resp::json(200, body)]);
            let t = transcribe_with(id, &testutil::audio(), &testutil::opts(model), &testutil::creds(&mock, "key-1234567")).unwrap();
            assert_eq!(t.text, want, "{id:?}");
        }
    }

    #[test]
    fn wav_roundtrip() {
        let a = Audio { samples: vec![0, 1000, -1000, i16::MAX, i16::MIN], sample_rate: 16000 };
        let b = Audio::from_wav(&a.to_wav()).unwrap();
        assert_eq!(b.samples, a.samples);
        assert_eq!(b.sample_rate, 16000);
    }

    /// Serializes tests that touch process-global env vars.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn with_env<R>(vars: &[(&str, Option<&str>)], f: impl FnOnce() -> R) -> R {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let saved: Vec<_> = vars.iter().map(|(k, _)| (*k, std::env::var(k).ok())).collect();
        // SAFETY: serialized by ENV_LOCK; only STT tests touch these variables.
        unsafe {
            for (k, v) in vars {
                match v {
                    Some(v) => std::env::set_var(k, v),
                    None => std::env::remove_var(k),
                }
            }
        }
        let out = f();
        // SAFETY: as above.
        unsafe {
            for (k, v) in saved {
                match v {
                    Some(v) => std::env::set_var(k, v),
                    None => std::env::remove_var(k),
                }
            }
        }
        out
    }

    #[test]
    fn credentials_trim_the_secret_and_resolve_base_overrides() {
        let mut cfg = Config::default();
        cfg.stt.provider = SttProviderId::Groq;
        with_env(&[("OPENTALK_SECRET_GROQ", Some("  gsk_abcdef123456
")), (ENV_BASE_URL, None)], || {
            let c = credentials(&cfg);
            assert_eq!(c.api_key.as_deref(), Some("gsk_abcdef123456"));
            assert_eq!(c.base_url, None);
            assert!(c.extra.is_empty());
        });
        with_env(&[("OPENTALK_SECRET_GROQ", Some("gsk_abcdef123456")), (ENV_BASE_URL, Some(" http://127.0.0.1:9 "))], || {
            assert_eq!(credentials(&cfg).base_url.as_deref(), Some("http://127.0.0.1:9"));
        });
        // Custom takes its endpoint from the config; the test env var still wins.
        cfg.stt.provider = SttProviderId::Custom;
        cfg.stt.custom_base_url = " https://stt.example.com/v1 ".into();
        with_env(&[(ENV_BASE_URL, None)], || {
            assert_eq!(credentials(&cfg).base_url.as_deref(), Some("https://stt.example.com/v1"));
        });
        with_env(&[(ENV_BASE_URL, Some("http://127.0.0.1:7"))], || {
            assert_eq!(credentials(&cfg).base_url.as_deref(), Some("http://127.0.0.1:7"));
        });
        // Other providers ignore the Custom endpoint.
        cfg.stt.provider = SttProviderId::Openai;
        with_env(&[(ENV_BASE_URL, None)], || assert_eq!(credentials(&cfg).base_url, None));
    }

    #[test]
    fn corti_credentials_carry_target_and_secret() {
        let mut cfg = Config::default();
        cfg.stt.provider = SttProviderId::Corti;
        cfg.stt.corti_environment = " EU ".into();
        cfg.stt.corti_tenant = " acme ".into();
        let c = with_env(
            &[("OPENTALK_SECRET_CORTI_CLIENT_ID", Some("cid")), ("OPENTALK_SECRET_CORTI_CLIENT_SECRET", Some("csecret")), (ENV_BASE_URL, None)],
            || credentials(&cfg),
        );
        assert_eq!(c.api_key.as_deref(), Some("cid"));
        let get = |k: &str| c.extra.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
        assert_eq!((get("environment"), get("tenant"), get("client_secret")), (Some("eu"), Some("acme"), Some("csecret")));
    }

    #[test]
    fn options_follow_config() {
        let mut cfg = Config::default();
        cfg.stt.provider = SttProviderId::Groq;
        cfg.stt.models.insert(SttProviderId::Groq, "whisper-large-v3".into());
        cfg.stt.timeout_ms = 1234;
        cfg.language = "auto".into();
        cfg.dictionary = vec!["Priya".into(), "priya".into(), " OpenTalk ".into()];
        cfg.snippets = vec![crate::core::config::Snippet { trigger: "my sig".into(), replacement: "Regards".into() }];
        let o = options(&cfg);
        assert_eq!((o.model.as_str(), o.language.clone(), o.timeout_ms), ("whisper-large-v3", None, 1234));
        assert_eq!(o.hints, ["Priya", "OpenTalk", "my sig"]);
        cfg.language = "en-US".into();
        assert_eq!(options(&cfg).language.as_deref(), Some("en-US"));
        cfg.language = "  ".into();
        assert_eq!(options(&cfg).language, None);
    }

    #[test]
    fn transcribe_dispatches_through_config_and_env() {
        let mock = testutil::serve(vec![Resp::json(200, r#"{"text":" via config "}"#)]);
        let mut cfg = Config::default();
        cfg.stt.provider = SttProviderId::Groq;
        let t = with_env(&[("OPENTALK_SECRET_GROQ", Some("gsk_abcdef123456")), (ENV_BASE_URL, Some(mock.url.as_str()))], || {
            transcribe(&cfg, &testutil::audio()).unwrap()
        });
        assert_eq!(t.text, "via config");
        let r = mock.one();
        assert_eq!(r.header("authorization"), Some("Bearer gsk_abcdef123456"));
        assert_eq!(r.multipart().text("model"), ["whisper-large-v3-turbo"]);
    }

    #[test]
    fn dictionary_echo_is_no_speech_but_real_text_passes() {
        let mut o = testutil::opts("whisper-large-v3");
        o.hints = vec!["Priya".into(), "OpenTalk".into(), "Kubernetes".into()];
        for echo in ["Priya, OpenTalk, Kubernetes.", "priya opentalk kubernetes"] {
            let mock = testutil::serve(vec![Resp::json(200, &format!(r#"{{"text":"{echo}"}}"#))]);
            let e = transcribe_with(SttProviderId::Groq, &testutil::audio(), &o, &testutil::creds(&mock, "gsk_abcdef123456")).unwrap_err();
            assert_eq!(e.code, SttErrorCode::NoSpeech, "{echo}");
        }
        let mock = testutil::serve(vec![Resp::json(200, r#"{"text":"Please ask Priya to deploy it on Kubernetes tomorrow."}"#)]);
        let t = transcribe_with(SttProviderId::Groq, &testutil::audio(), &o, &testutil::creds(&mock, "gsk_abcdef123456")).unwrap();
        assert!(t.text.starts_with("Please ask Priya"));
    }

    #[test]
    fn prewarm_skips_streaming_models_and_never_needs_a_key() {
        let mut cfg = Config::default();
        cfg.stt.provider = SttProviderId::Assemblyai;
        prewarm(&cfg); // streaming only: must return without touching secrets or the network
        cfg.stt.provider = SttProviderId::Custom;
        cfg.stt.custom_base_url = String::new();
        prewarm(&cfg); // no endpoint: nothing to warm
    }
}
