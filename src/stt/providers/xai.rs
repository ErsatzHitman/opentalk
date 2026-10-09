//! xAI `POST {base}/stt` (multipart, Bearer). The API takes no `model`; `language` +
//! `format=true` (inverse text normalisation) are sent only for languages xAI supports;
//! the dictionary goes out as repeated `keyterm` fields.
use super::{BYOK_FILE_LIMIT, api_key, base_lang, base_url, check_size, text_json, timeout};
use crate::core::config::SttProviderId;
use crate::core::http::{self, Multipart};
use crate::stt::errors::{self, SttError};
use crate::stt::{Audio, BatchProvider, Credentials, SttOptions, Transcript};

pub const DEFAULT_BASE: &str = "https://api.x.ai/v1";
const MAX_KEYTERMS: usize = 100;
const MAX_KEYTERM_CHARS: usize = 50;

/// Languages with inverse text normalisation (OpenWhispr `XAI_STT_LANGUAGES`).
pub const XAI_STT_LANGUAGES: &[&str] = &[
    "ar", "cs", "da", "de", "en", "es", "fa", "fil", "fr", "hi", "id", "it", "ja", "ko", "mk", "ms", "nl", "pl", "pt", "ro", "ru", "sv",
    "th", "tr", "vi",
];

pub struct Xai;

impl BatchProvider for Xai {
    fn id(&self) -> SttProviderId {
        SttProviderId::Xai
    }

    fn prewarm_url(&self, creds: &Credentials) -> Option<String> {
        Some(base_url(creds, DEFAULT_BASE))
    }

    fn transcribe(&self, audio: &Audio, opts: &SttOptions, creds: &Credentials) -> Result<Transcript, SttError> {
        let key = api_key(creds, "xAI")?;
        let wav = audio.to_wav();
        check_size("xAI", wav.len(), BYOK_FILE_LIMIT)?;

        let mut form = Multipart::new();
        form.file("file", "audio.wav", "audio/wav", &wav);
        if let Some(lang) = opts.language.as_deref().map(base_lang).filter(|l| XAI_STT_LANGUAGES.contains(&l.as_str())) {
            form.text("language", &lang);
            form.text("format", "true");
        }
        for term in keyterms(&opts.hints) {
            form.text("keyterm", &term);
        }
        let (content_type, body) = form.finish();

        let url = format!("{}/stt", base_url(creds, DEFAULT_BASE));
        let auth = format!("Bearer {key}");
        let res = http::post(&url, &[("Authorization", &auth)], &content_type, &body, timeout(opts));
        let resp = errors::check_response("xAI", None, false, res)?;
        text_json("xAI", false, &resp)
    }
}

/// At most 100 non-empty terms, each cut to 50 characters.
fn keyterms(hints: &[String]) -> Vec<String> {
    hints
        .iter()
        .map(|t| t.trim().chars().take(MAX_KEYTERM_CHARS).collect::<String>())
        .filter(|t| !t.is_empty())
        .take(MAX_KEYTERMS)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stt::SttErrorCode;
    use crate::stt::providers::testutil::*;

    #[test]
    fn request_shape() {
        let mock = serve(vec![Resp::json(200, r#"{"text":"Hello world.","language":"en","duration":1.0}"#)]);
        let mut o = opts("grok-stt");
        o.language = Some("en-GB".into());
        o.hints = vec!["Priya".into(), "  OpenTalk ".into(), "".into()];
        let t = Xai.transcribe(&audio(), &o, &creds(&mock, "xai-abcdef123456")).unwrap();
        assert_eq!(t.text, "Hello world.");
        assert_eq!(t.detected_language.as_deref(), Some("en"));
        let r = mock.one();
        assert_eq!((r.method.as_str(), r.path.as_str()), ("POST", "/stt"));
        assert_eq!(r.header("authorization"), Some("Bearer xai-abcdef123456"));
        let parts = r.multipart();
        assert_eq!(parts.names(), ["file", "language", "format", "keyterm", "keyterm"]);
        assert!(parts.text("model").is_empty(), "xAI takes no model field");
        assert_eq!(parts.text("language"), ["en"]);
        assert_eq!(parts.text("format"), ["true"]);
        assert_eq!(parts.text("keyterm"), ["Priya", "OpenTalk"]);
    }

    #[test]
    fn unsupported_or_auto_language_sends_neither_language_nor_format() {
        for lang in [None, Some("zh".to_string()), Some("auto-ish".to_string())] {
            let mock = serve(vec![Resp::json(200, r#"{"text":"x"}"#)]);
            let mut o = opts("grok-stt");
            o.language = lang;
            Xai.transcribe(&audio(), &o, &creds(&mock, "xai-abcdef123456")).unwrap();
            assert_eq!(mock.one().multipart().names(), ["file"]);
        }
    }

    #[test]
    fn keyterms_are_capped() {
        let hints: Vec<String> = (0..150).map(|i| format!("{i}{}", "x".repeat(80))).collect();
        let k = keyterms(&hints);
        assert_eq!(k.len(), 100);
        assert!(k.iter().all(|t| t.chars().count() == 50));
    }

    #[test]
    fn supported_languages_match_openwhispr() {
        assert_eq!(XAI_STT_LANGUAGES.len(), 25);
        assert!(XAI_STT_LANGUAGES.contains(&"fil") && !XAI_STT_LANGUAGES.contains(&"zh"));
    }

    #[test]
    fn errors() {
        let mock = serve(vec![Resp::json(402, "{}")]);
        let e = Xai.transcribe(&audio(), &opts("grok-stt"), &creds(&mock, "xai-abcdef123456")).unwrap_err();
        assert_eq!((e.code, e.message.as_str()), (SttErrorCode::QuotaExhausted, "Your xAI account is out of credit."));
        let e = Xai.transcribe(&audio(), &opts("grok-stt"), &Credentials::default()).unwrap_err();
        assert_eq!(e.code, SttErrorCode::NotConfigured);
    }
}
