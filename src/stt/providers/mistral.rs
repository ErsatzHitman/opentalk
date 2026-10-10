//! Mistral `POST {base}/audio/transcriptions` (multipart, `x-api-key`). The dictionary is
//! sent as repeated `context_bias` tokens (comma/whitespace split, at most 100).
use super::{BYOK_FILE_LIMIT, api_key, base_lang, base_url, check_size, model_or, text_json_with, timeout};
use crate::core::config::SttProviderId;
use crate::core::http::{self, Multipart};
use crate::stt::errors::{self, SttError};
use crate::stt::{Audio, BatchProvider, Credentials, SttOptions, Transcript};

pub const DEFAULT_BASE: &str = "https://api.mistral.ai/v1";
pub const DEFAULT_MODEL: &str = "voxtral-mini-latest";
const MAX_TOKENS: usize = 100;

pub struct Mistral;

impl BatchProvider for Mistral {
    fn id(&self) -> SttProviderId {
        SttProviderId::Mistral
    }

    fn prewarm_url(&self, creds: &Credentials) -> Option<String> {
        Some(base_url(creds, DEFAULT_BASE))
    }

    fn transcribe(&self, audio: &Audio, opts: &SttOptions, creds: &Credentials) -> Result<Transcript, SttError> {
        let key = api_key(creds, "Mistral")?;
        let wav = audio.to_wav();
        check_size("Mistral", wav.len(), BYOK_FILE_LIMIT)?;
        let model = model_or(opts, DEFAULT_MODEL);

        let mut form = Multipart::new();
        form.file("file", "audio.wav", "audio/wav", &wav);
        form.text("model", model);
        if let Some(lang) = opts.language.as_deref().filter(|l| !l.is_empty() && *l != "auto") {
            form.text("language", &base_lang(lang));
        }
        for token in context_bias(&opts.hints) {
            form.text("context_bias", token);
        }
        let (content_type, body) = form.finish();

        let url = format!("{}/audio/transcriptions", base_url(creds, DEFAULT_BASE));
        let res = http::post(&url, &[("x-api-key", key)], &content_type, &body, timeout(opts));
        let resp = errors::check_response_with("Mistral", Some(model), false, res, &[key])?;
        text_json_with("Mistral", false, &resp, &[key])
    }
}

/// Dictionary entries split on commas, then on whitespace, first 100 tokens.
fn context_bias(hints: &[String]) -> Vec<&str> {
    hints.iter().flat_map(|h| h.split(',')).flat_map(str::split_whitespace).take(MAX_TOKENS).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stt::SttErrorCode;
    use crate::stt::providers::testutil::*;

    #[test]
    fn request_shape() {
        let mock = serve(vec![Resp::json(200, r#"{"text":"Bonjour","language":"fr"}"#)]);
        let mut o = opts("voxtral-mini-latest");
        o.language = Some("fr-CA".into());
        o.hints = vec!["Priya Shah, OpenTalk".into(), "Zed".into()];
        let t = Mistral.transcribe(&audio(), &o, &creds(&mock, "mistral-key-1")).unwrap();
        assert_eq!(t.text, "Bonjour");
        let r = mock.one();
        assert_eq!((r.method.as_str(), r.path.as_str()), ("POST", "/audio/transcriptions"));
        assert_eq!(r.header("x-api-key"), Some("mistral-key-1"));
        assert!(r.header("authorization").is_none());
        let parts = r.multipart();
        assert_eq!(parts.names(), ["file", "model", "language", "context_bias", "context_bias", "context_bias", "context_bias"]);
        assert_eq!(parts.text("model"), ["voxtral-mini-latest"]);
        assert_eq!(parts.text("language"), ["fr"]);
        assert_eq!(parts.text("context_bias"), ["Priya", "Shah", "OpenTalk", "Zed"]);
    }

    #[test]
    fn bias_tokens_are_capped_at_100() {
        let hints = vec![(0..150).map(|i| format!("t{i}")).collect::<Vec<_>>().join(" ")];
        assert_eq!(context_bias(&hints).len(), 100);
    }

    #[test]
    fn default_model_and_errors() {
        let mock = serve(vec![Resp::json(403, r#"{"message":"Unauthorized"}"#)]);
        let e = Mistral.transcribe(&audio(), &opts(""), &creds(&mock, "mistral-key-1")).unwrap_err();
        assert_eq!((e.code, e.message.as_str()), (SttErrorCode::AuthFailed, "Mistral rejected your API key."));
        assert_eq!(mock.one().multipart().text("model"), ["voxtral-mini-latest"]);
    }

    #[test]
    fn configured_key_echoed_by_the_provider_is_redacted() {
        const KEY: &str = "mi9Zq3Lw7Tn2Vb8Xc5Rd1Fh4Jk6Pa0Sy";
        let mock = serve(vec![Resp::json(401, &format!(r#"{{"message":"Unauthorized","detail":"api key {KEY} is invalid"}}"#))]);
        let e = Mistral.transcribe(&audio(), &opts(""), &creds(&mock, KEY)).unwrap_err();
        assert_eq!(e.code, SttErrorCode::AuthFailed);
        assert_no_leak(&e, KEY);
        let mock = serve(vec![Resp::json(200, &format!(r#"{{"note":"{KEY}"}}"#))]);
        assert_no_leak(&Mistral.transcribe(&audio(), &opts(""), &creds(&mock, KEY)).unwrap_err(), KEY);
    }
}
