//! Gemini Interactions API: `POST {base}/interactions`, JSON with inline base64 audio,
//! `x-goog-api-key`. (gemini-3.5-transcribe is served only by this endpoint.)
use super::{api_key, base_lang, base_url, model_or, timeout};
use crate::core::config::SttProviderId;
use crate::core::http;
use crate::stt::errors::{self, SttError, SttErrorCode};
use crate::stt::{Audio, BatchProvider, Credentials, SttOptions, Transcript};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Value, json};

pub const DEFAULT_BASE: &str = "https://generativelanguage.googleapis.com/v1beta";
pub const DEFAULT_MODEL: &str = "gemini-3.5-transcribe";
/// Inline-audio cap (OpenWhispr `GEMINI_FILE_SIZE_LIMIT`).
pub const FILE_LIMIT: usize = 14 * 1024 * 1024;

pub struct Gemini;

impl BatchProvider for Gemini {
    fn id(&self) -> SttProviderId {
        SttProviderId::Gemini
    }

    fn prewarm_url(&self, creds: &Credentials) -> Option<String> {
        Some(base_url(creds, DEFAULT_BASE))
    }

    fn transcribe(&self, audio: &Audio, opts: &SttOptions, creds: &Credentials) -> Result<Transcript, SttError> {
        let key = api_key(creds, "Gemini")?;
        let wav = audio.to_wav();
        super::check_size("Gemini", wav.len(), FILE_LIMIT)?;
        let model = model_or(opts, DEFAULT_MODEL);
        let body = request_body(model, &wav, opts);

        let url = format!("{}/interactions", base_url(creds, DEFAULT_BASE));
        let res = http::post_json(&url, &[("x-goog-api-key", key)], &body, timeout(opts));
        let resp = errors::check_response("Gemini", Some(model), false, res)?;
        let data: Value = resp.json().map_err(|e| errors::bad_response("Gemini", false, &format!("invalid JSON ({e})"), &resp.body))?;
        parse_response(&data)
    }
}

fn request_body(model: &str, wav: &[u8], opts: &SttOptions) -> Value {
    let mut cfg = serde_json::Map::new();
    if let Some(lang) = opts.language.as_deref().filter(|l| !l.is_empty() && *l != "auto") {
        cfg.insert("language_codes".into(), json!([base_lang(lang)]));
    }
    let vocab: Vec<&str> = opts.hints.iter().map(|h| h.trim()).filter(|h| !h.is_empty()).collect();
    if !vocab.is_empty() {
        cfg.insert("custom_vocabulary".into(), json!(vocab));
    }
    let mut body = json!({
        "model": model,
        "input": [{ "type": "audio", "data": STANDARD.encode(wav), "mime_type": "audio/wav" }],
    });
    if !cfg.is_empty() {
        body["generation_config"] = json!({ "transcription_config": Value::Object(cfg) });
    }
    body
}

/// `output_text`, else the joined text parts of `steps[].content[]`. Anything but a
/// `completed` status is a failure, never a (possibly truncated) transcript.
fn parse_response(data: &Value) -> Result<Transcript, SttError> {
    if let Some(status) = data.get("status").and_then(|s| s.as_str()).filter(|s| !s.is_empty() && *s != "completed") {
        let detail = data.pointer("/error/message").and_then(|m| m.as_str());
        let message = match detail {
            Some(d) => format!("Gemini transcription failed: {d}"),
            None => format!("Gemini transcription did not complete (status: {status})"),
        };
        return Err(SttError::new(SttErrorCode::Other, errors::redact(&message)).with_detail(format!("status {status}")));
    }
    let text = match data.get("output_text").and_then(|t| t.as_str()).filter(|t| !t.trim().is_empty()) {
        Some(t) => t.to_string(),
        None => data
            .get("steps")
            .and_then(|s| s.as_array())
            .into_iter()
            .flatten()
            .flat_map(|step| step.get("content").and_then(|c| c.as_array()).into_iter().flatten())
            .filter(|part| part.get("type").and_then(|t| t.as_str()) == Some("text"))
            .filter_map(|part| part.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join(" "),
    };
    Ok(Transcript { text, detected_language: None })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stt::providers::testutil::*;

    #[test]
    fn request_shape() {
        let mock = serve(vec![Resp::json(200, r#"{"status":"completed","output_text":"Hello there."}"#)]);
        let mut o = opts("gemini-3.5-transcribe");
        o.language = Some("en-US".into());
        o.hints = vec!["Priya".into(), "OpenTalk".into()];
        let a = audio();
        let t = Gemini.transcribe(&a, &o, &creds(&mock, "AIzaSy-test-key")).unwrap();
        assert_eq!(t.text, "Hello there.");

        let r = mock.one();
        assert_eq!((r.method.as_str(), r.path.as_str()), ("POST", "/interactions"));
        assert_eq!(r.header("x-goog-api-key"), Some("AIzaSy-test-key"));
        assert!(r.header("authorization").is_none());
        assert_eq!(r.header("content-type"), Some("application/json"));
        let body = r.json();
        assert_eq!(body["model"], "gemini-3.5-transcribe");
        assert_eq!(body["input"].as_array().unwrap().len(), 1);
        assert_eq!(body["input"][0]["type"], "audio");
        assert_eq!(body["input"][0]["mime_type"], "audio/wav");
        assert_eq!(STANDARD.decode(body["input"][0]["data"].as_str().unwrap()).unwrap(), a.to_wav());
        assert_eq!(
            body["generation_config"],
            json!({"transcription_config": {"language_codes": ["en"], "custom_vocabulary": ["Priya", "OpenTalk"]}})
        );
    }

    #[test]
    fn no_language_or_vocabulary_means_no_generation_config() {
        let mock = serve(vec![Resp::json(200, r#"{"status":"completed","output_text":"x"}"#)]);
        Gemini.transcribe(&audio(), &opts(""), &creds(&mock, "AIzaSy-test-key")).unwrap();
        let body = mock.one().json();
        assert_eq!(body["model"], DEFAULT_MODEL);
        assert!(body.get("generation_config").is_none());
    }

    #[test]
    fn falls_back_to_step_content_text() {
        let mock = serve(vec![Resp::json(
            200,
            r#"{"status":"completed","steps":[{"content":[{"type":"text","text":"Hello"},{"type":"thought","text":"hmm"}]},{"content":[{"type":"text","text":"world."}]}]}"#,
        )]);
        let t = Gemini.transcribe(&audio(), &opts("g"), &creds(&mock, "AIzaSy-test-key")).unwrap();
        assert_eq!(t.text, "Hello world.");
        // A blank output_text also falls through to the steps.
        let v: Value = serde_json::from_str(r#"{"output_text":"  ","steps":[{"content":[{"type":"text","text":"ok"}]}]}"#).unwrap();
        assert_eq!(parse_response(&v).unwrap().text, "ok");
    }

    #[test]
    fn non_completed_status_is_an_error() {
        let v: Value = serde_json::from_str(r#"{"status":"failed","error":{"message":"quota blew up"},"output_text":"partial"}"#).unwrap();
        let e = parse_response(&v).unwrap_err();
        assert_eq!(e.message, "Gemini transcription failed: quota blew up");
        let v: Value = serde_json::from_str(r#"{"status":"budget_exceeded"}"#).unwrap();
        assert_eq!(parse_response(&v).unwrap_err().message, "Gemini transcription did not complete (status: budget_exceeded)");
    }

    #[test]
    fn http_errors() {
        // Google answers a bad key with 400 + API_KEY_INVALID.
        let mock = serve(vec![Resp::json(400, r#"{"error":{"status":"INVALID_ARGUMENT","details":[{"reason":"API_KEY_INVALID"}]}}"#)]);
        let e = Gemini.transcribe(&audio(), &opts("g"), &creds(&mock, "AIzaSy-test-key")).unwrap_err();
        assert_eq!((e.code, e.message.as_str()), (SttErrorCode::AuthFailed, "Gemini rejected your API key."));
        let mock = serve(vec![Resp::json(429, r#"{"error":{"message":"You exceeded your current quota, please check your plan and billing details"}}"#)]);
        let e = Gemini.transcribe(&audio(), &opts("g"), &creds(&mock, "AIzaSy-test-key")).unwrap_err();
        assert_eq!(e.code, SttErrorCode::RateLimited);
        let e = Gemini.transcribe(&audio(), &opts("g"), &Credentials::default()).unwrap_err();
        assert_eq!(e.code, SttErrorCode::NotConfigured);
    }

    #[test]
    fn audio_over_14_mb_is_rejected_locally() {
        let big = Audio { samples: vec![0; 8 * 1024 * 1024], sample_rate: 16_000 }; // 16 MB of PCM
        let c = Credentials { api_key: Some("AIzaSy-test-key".into()), base_url: Some("http://127.0.0.1:1".into()), extra: vec![] };
        assert_eq!(Gemini.transcribe(&big, &opts("g"), &c).unwrap_err().code, SttErrorCode::PayloadTooLarge);
    }
}
