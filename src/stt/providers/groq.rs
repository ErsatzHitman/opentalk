//! Groq: OpenAI-compatible `POST {base}/audio/transcriptions`; prompt budget 890 chars
//! (Groq rejects prompts over 896).
use super::openai::{Auth, Request, transcribe_multipart};
use super::{api_key, base_url, model_or};
use crate::core::config::SttProviderId;
use crate::stt::errors::SttError;
use crate::stt::{Audio, BatchProvider, Credentials, SttOptions, Transcript};

pub const DEFAULT_BASE: &str = "https://api.groq.com/openai/v1";
pub const DEFAULT_MODEL: &str = "whisper-large-v3-turbo";

pub struct Groq;

impl BatchProvider for Groq {
    fn id(&self) -> SttProviderId {
        SttProviderId::Groq
    }

    fn prewarm_url(&self, creds: &Credentials) -> Option<String> {
        Some(base_url(creds, DEFAULT_BASE))
    }

    fn transcribe(&self, audio: &Audio, opts: &SttOptions, creds: &Credentials) -> Result<Transcript, SttError> {
        let key = api_key(creds, "Groq")?;
        let url = format!("{}/audio/transcriptions", base_url(creds, DEFAULT_BASE));
        transcribe_multipart(
            audio,
            opts,
            &Request {
                name: "Groq",
                self_hosted: false,
                url: &url,
                auth: Auth::Bearer(key),
                model: model_or(opts, DEFAULT_MODEL),
                prompt_limit: super::prompt_limit(true, ""),
                stream: false,
                keywords: false,
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stt::SttErrorCode;
    use crate::stt::providers::testutil::*;

    #[test]
    fn request_shape() {
        let mock = serve(vec![Resp::json(200, r#"{"text":" hi","language":"english"}"#)]);
        let mut o = opts("whisper-large-v3");
        o.language = Some("de".into());
        o.hints = (0..400).map(|i| format!("word{i}")).collect();
        let t = Groq.transcribe(&audio(), &o, &creds(&mock, "gsk_abcdef123456")).unwrap();
        assert_eq!(t.text, " hi");
        assert_eq!(t.detected_language.as_deref(), Some("english"));
        let r = mock.one();
        assert_eq!((r.method.as_str(), r.path.as_str()), ("POST", "/audio/transcriptions"));
        assert_eq!(r.header("authorization"), Some("Bearer gsk_abcdef123456"));
        let parts = r.multipart();
        assert_eq!(parts.names(), ["file", "model", "language", "prompt"]);
        assert_eq!(parts.text("model"), ["whisper-large-v3"]);
        assert_eq!(parts.text("language"), ["de"]);
        let prompt = parts.text("prompt").remove(0);
        assert!(prompt.chars().count() <= 890 && prompt.chars().count() > 870);
    }

    #[test]
    fn default_model_and_no_stream() {
        let mock = serve(vec![Resp::json(200, r#"{"text":"x"}"#)]);
        Groq.transcribe(&audio(), &opts(""), &creds(&mock, "gsk_abcdef123456")).unwrap();
        let parts = mock.one().multipart();
        assert_eq!(parts.text("model"), ["whisper-large-v3-turbo"]);
        assert!(parts.text("stream").is_empty() && parts.text("keywords[]").is_empty());
    }

    #[test]
    fn errors() {
        let mock = serve(vec![Resp::json(429, r#"{"error":{"message":"Rate limit reached ... https://console.groq.com/settings/billing"}}"#)]);
        let e = Groq.transcribe(&audio(), &opts("whisper-large-v3"), &creds(&mock, "gsk_abcdef123456")).unwrap_err();
        assert_eq!(e.code, SttErrorCode::RateLimited);
        assert_eq!(e.message, "Groq rate-limited the request. Wait a moment and try again.");
        let e = Groq.transcribe(&audio(), &opts("x"), &Credentials::default()).unwrap_err();
        assert_eq!(e.code, SttErrorCode::NotConfigured);
    }
}
