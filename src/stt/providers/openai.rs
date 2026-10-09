//! OpenAI `POST {base}/audio/transcriptions` (multipart) plus the shared request/response
//! engine for every OpenAI-shaped endpoint (Groq, Tinfoil, Custom/Azure).
//!
//! `gpt-transcribe`, `gpt-4o-transcribe` and `gpt-4o-mini-transcribe` are requested with
//! `stream=true` and answered as SSE (`transcript.text.delta|segment|done`, `[DONE]`); a
//! reply that is not an event stream is parsed as plain JSON.
use super::{BYOK_FILE_LIMIT, api_key, base_lang, base_url, check_size, join_prompt, model_or, prompt_limit, split_terms, text_json, timeout, trim_prompt};
use crate::core::config::SttProviderId;
use crate::core::http::{self, HttpResponse, Multipart};
use crate::stt::errors::{self, SttError};
use crate::stt::{Audio, BatchProvider, Credentials, SttOptions, Transcript};

pub const DEFAULT_BASE: &str = "https://api.openai.com/v1";
pub const DEFAULT_MODEL: &str = "gpt-4o-mini-transcribe";

/// OpenAI rejects forms with more than ~1,000 parts; the rest ride in `prompt`.
const MAX_KEYWORDS: usize = 900;

pub struct Openai;

impl BatchProvider for Openai {
    fn id(&self) -> SttProviderId {
        SttProviderId::Openai
    }

    fn prewarm_url(&self, creds: &Credentials) -> Option<String> {
        Some(base_url(creds, DEFAULT_BASE))
    }

    fn transcribe(&self, audio: &Audio, opts: &SttOptions, creds: &Credentials) -> Result<Transcript, SttError> {
        let key = api_key(creds, "OpenAI")?;
        let model = model_or(opts, DEFAULT_MODEL);
        let url = format!("{}/audio/transcriptions", base_url(creds, DEFAULT_BASE));
        transcribe_multipart(
            audio,
            opts,
            &Request {
                name: "OpenAI",
                self_hosted: false,
                url: &url,
                auth: Auth::Bearer(key),
                model,
                prompt_limit: prompt_limit(false, model),
                stream: should_stream(model),
                keywords: model.trim() == "gpt-transcribe",
            },
        )
    }
}

/// Models answered as an SSE stream (OpenWhispr `shouldStreamTranscription`).
pub fn should_stream(model: &str) -> bool {
    let m = model.trim();
    matches!(m, "gpt-transcribe" | "gpt-4o-transcribe" | "gpt-4o-transcribe-diarize") || m.starts_with("gpt-4o-mini-transcribe")
}

#[derive(Clone, Copy)]
pub enum Auth<'a> {
    Bearer(&'a str),
    /// Azure OpenAI: `api-key` header.
    ApiKey(&'a str),
    None,
}

/// One OpenAI-shaped multipart transcription call.
pub struct Request<'a> {
    /// Provider name for error messages.
    pub name: &'a str,
    pub self_hosted: bool,
    pub url: &'a str,
    pub auth: Auth<'a>,
    pub model: &'a str,
    pub prompt_limit: usize,
    pub stream: bool,
    /// Send the dictionary as `keywords[]` (gpt-transcribe) with overflow in `prompt`.
    pub keywords: bool,
}

pub fn transcribe_multipart(audio: &Audio, opts: &SttOptions, req: &Request) -> Result<Transcript, SttError> {
    let wav = audio.to_wav();
    check_size(req.name, wav.len(), BYOK_FILE_LIMIT)?;

    let mut form = Multipart::new();
    form.file("file", "audio.wav", "audio/wav", &wav);
    form.text("model", req.model);
    if let Some(lang) = opts.language.as_deref().filter(|l| !l.is_empty() && *l != "auto") {
        form.text("language", &base_lang(lang));
    }
    let (keywords, prompt) = if req.keywords {
        let mut terms = split_terms(&opts.hints);
        let overflow = if terms.len() > MAX_KEYWORDS { terms.split_off(MAX_KEYWORDS) } else { Vec::new() };
        (terms, trim_prompt(overflow.join(", "), req.prompt_limit))
    } else {
        (Vec::new(), join_prompt(&opts.hints, req.prompt_limit))
    };
    if !prompt.is_empty() {
        form.text("prompt", &prompt);
    }
    for k in &keywords {
        form.text("keywords[]", k);
    }
    if req.stream {
        form.text("stream", "true");
    }
    let (content_type, body) = form.finish();

    let auth_value;
    let mut headers: Vec<(&str, &str)> = Vec::with_capacity(2);
    match req.auth {
        Auth::Bearer(k) => {
            auth_value = format!("Bearer {k}");
            headers.push(("Authorization", &auth_value));
        }
        Auth::ApiKey(k) => headers.push(("api-key", k)),
        Auth::None => {}
    }
    let resp = errors::check_response(req.name, Some(req.model), req.self_hosted, http::post(req.url, &headers, &content_type, &body, timeout(opts)))?;

    if req.stream && is_event_stream(&resp) {
        let text = parse_sse(&resp.body);
        return Ok(Transcript { text, detected_language: None });
    }
    text_json(req.name, req.self_hosted, &resp)
}

fn is_event_stream(resp: &HttpResponse) -> bool {
    resp.header("content-type").is_some_and(|c| c.to_ascii_lowercase().contains("text/event-stream"))
        || resp.body.trim_start().starts_with("data:")
}

/// Assemble the transcript from an OpenAI transcription event stream: the `done` event's
/// full text wins, else the concatenated `delta` / `segment` texts.
pub fn parse_sse(body: &str) -> String {
    let mut collected = String::new();
    let mut done: Option<String> = None;
    for line in body.lines() {
        let line = line.trim();
        let Some(data) = line.strip_prefix("data:") else { continue };
        let data = data.trim();
        if data == "[DONE]" {
            done.get_or_insert_with(|| collected.clone());
            continue;
        }
        let Ok(ev) = serde_json::from_str::<serde_json::Value>(data) else { continue };
        let text = |k: &str| ev.get(k).and_then(|v| v.as_str());
        match ev.get("type").and_then(|t| t.as_str()) {
            Some("transcript.text.delta") => collected.push_str(text("delta").unwrap_or_default()),
            Some("transcript.text.segment") => collected.push_str(text("text").unwrap_or_default()),
            Some("transcript.text.done") => {
                if let Some(t) = text("text") {
                    done = Some(t.to_string());
                }
            }
            _ => {}
        }
    }
    done.unwrap_or(collected)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stt::SttErrorCode;
    use crate::stt::providers::testutil::*;

    #[test]
    fn request_shape_and_json_response() {
        let mock = serve(vec![Resp::json(200, r#"{"text":" Hello there."}"#)]);
        let mut o = opts("whisper-1");
        o.language = Some("en-US".into());
        o.hints = vec!["Priya".into(), "OpenTalk".into()];
        let t = Openai.transcribe(&audio(), &o, &creds(&mock, "sk-test-123456")).unwrap();
        assert_eq!(t.text, " Hello there.");

        let r = mock.one();
        assert_eq!(r.method, "POST");
        assert_eq!(r.path, "/audio/transcriptions");
        assert_eq!(r.header("authorization"), Some("Bearer sk-test-123456"));
        assert!(r.header("content-type").unwrap().starts_with("multipart/form-data; boundary="));
        let parts = r.multipart();
        assert_eq!(parts.names(), ["file", "model", "language", "prompt"]);
        let file = &parts[0];
        assert_eq!(file.filename.as_deref(), Some("audio.wav"));
        assert_eq!(file.content_type.as_deref(), Some("audio/wav"));
        assert_eq!(&file.data[..4], b"RIFF");
        assert_eq!(file.data, audio().to_wav());
        assert_eq!(parts.text("model"), ["whisper-1"]);
        assert_eq!(parts.text("language"), ["en"]);
        assert_eq!(parts.text("prompt"), ["Priya, OpenTalk"]);
        assert!(parts.text("stream").is_empty(), "whisper-1 does not stream");
    }

    #[test]
    fn auto_language_and_no_hints_omit_fields() {
        let mock = serve(vec![Resp::json(200, r#"{"text":"x"}"#)]);
        Openai.transcribe(&audio(), &opts("whisper-1"), &creds(&mock, "k-12345")).unwrap();
        assert_eq!(mock.one().multipart().names(), ["file", "model"]);
    }

    #[test]
    fn prompt_budgets_by_model() {
        let hints: Vec<String> = (0..2000).map(|i| format!("term{i}")).collect();
        for (model, cap) in [("whisper-1", 900), ("gpt-4o-transcribe", 8000)] {
            let mock = serve(vec![Resp::json(200, r#"{"text":"x"}"#)]);
            let mut o = opts(model);
            o.hints = hints.clone();
            Openai.transcribe(&audio(), &o, &creds(&mock, "k-12345")).unwrap();
            let prompt = mock.one().multipart().text("prompt").remove(0);
            assert!(prompt.chars().count() <= cap && prompt.chars().count() > cap - 20, "{model}: {}", prompt.len());
            assert!(!prompt.ends_with(','), "cut at a term boundary");
        }
    }

    #[test]
    fn gpt_transcribe_uses_keywords_and_overflow_prompt() {
        let mock = serve(vec![Resp::sse("data: {\"type\":\"transcript.text.done\",\"text\":\"ok\"}\n\ndata: [DONE]\n\n")]);
        let mut o = opts("gpt-transcribe");
        o.hints = (0..905).map(|i| format!("term{i}")).collect();
        o.hints.push("Bad <b>tag</b>\nline".into());
        o.hints.push("   ".into());
        let t = Openai.transcribe(&audio(), &o, &creds(&mock, "k-12345")).unwrap();
        assert_eq!(t.text, "ok");
        let parts = mock.one().multipart();
        let kws = parts.text("keywords[]");
        assert_eq!(kws.len(), 900);
        assert_eq!(kws[0], "term0");
        assert_eq!(kws[899], "term899");
        assert_eq!(parts.text("prompt"), ["term900, term901, term902, term903, term904, Bad btag/b, line"]);
        assert_eq!(parts.text("stream"), ["true"]);
        assert_eq!(parts.names().last().unwrap(), "stream");
        assert!(kws.iter().all(|k| !k.contains(['<', '>', '\n'])));
    }

    #[test]
    fn sse_deltas_are_concatenated_and_done_text_wins() {
        let body = "data: {\"type\":\"transcript.text.delta\",\"delta\":\"Hel\"}\n\n\
                    data: {\"type\":\"transcript.text.delta\",\"delta\":\"lo\"}\n\n\
                    data: {\"type\":\"transcript.text.done\",\"text\":\"Hello!\"}\n\n\
                    data: [DONE]\n\n";
        assert_eq!(parse_sse(body), "Hello!");
        let no_done = "data:{\"type\":\"transcript.text.delta\",\"delta\":\"Hel\"}\n\ndata: {\"type\":\"transcript.text.segment\",\"text\":\"lo\"}\n\ndata: [DONE]\n";
        assert_eq!(parse_sse(no_done), "Hello");
        assert_eq!(parse_sse("event: x\ndata: not json\n\n"), "");
    }

    #[test]
    fn streaming_models_request_sse_and_parse_it() {
        for model in ["gpt-4o-transcribe", "gpt-4o-mini-transcribe", "gpt-transcribe"] {
            let mock = serve(vec![Resp::sse(
                "data: {\"type\":\"transcript.text.delta\",\"delta\":\"Hi \"}\n\ndata: {\"type\":\"transcript.text.delta\",\"delta\":\"there\"}\n\ndata: [DONE]\n\n",
            )]);
            let t = Openai.transcribe(&audio(), &opts(model), &creds(&mock, "k-12345")).unwrap();
            assert_eq!(t.text, "Hi there", "{model}");
            assert_eq!(mock.one().multipart().text("stream"), ["true"], "{model}");
        }
    }

    #[test]
    fn stream_requested_but_json_returned_falls_back_to_json() {
        let mock = serve(vec![Resp::json(200, r#"{"text":"plain json"}"#)]);
        let t = Openai.transcribe(&audio(), &opts("gpt-4o-transcribe"), &creds(&mock, "k-12345")).unwrap();
        assert_eq!(t.text, "plain json");
    }

    #[test]
    fn error_statuses_are_classified() {
        for (status, body, code, msg) in [
            (401, r#"{"error":{"message":"Incorrect API key provided: sk-abcdefgh1234"}}"#, SttErrorCode::AuthFailed, "OpenAI rejected your API key."),
            (429, r#"{"error":{"code":"insufficient_quota"}}"#, SttErrorCode::QuotaExhausted, "Your OpenAI account is out of credit."),
            (429, r#"{"error":{"message":"Rate limit reached"}}"#, SttErrorCode::RateLimited, "OpenAI rate-limited the request. Wait a moment and try again."),
            (413, "too big", SttErrorCode::PayloadTooLarge, "This recording is too large for OpenAI."),
            (404, r#"{"error":{"code":"model_not_found"}}"#, SttErrorCode::ModelNotFound, "OpenAI doesn't recognize the model “whisper-9”."),
            (500, "oops", SttErrorCode::Unavailable, "OpenAI is having problems right now. Try again shortly."),
        ] {
            let mock = serve(vec![Resp::json(status, body)]);
            let e = Openai.transcribe(&audio(), &opts("whisper-9"), &creds(&mock, "k-12345")).unwrap_err();
            assert_eq!(e.code, code, "{status}");
            assert_eq!(e.message, msg);
            assert_eq!(e.status, Some(status));
            assert!(!e.detail.contains("abcdefgh1234"), "secret leaked into detail: {}", e.detail);
        }
    }

    #[test]
    fn missing_key_is_not_configured() {
        let e = Openai.transcribe(&audio(), &opts("whisper-1"), &Credentials::default()).unwrap_err();
        assert_eq!(e.code, SttErrorCode::NotConfigured);
        assert_eq!(e.message, "No API key is set for OpenAI. Add it in Settings.");
    }

    #[test]
    fn unparsable_success_body_is_reported() {
        let mock = serve(vec![Resp::text(200, "not json")]);
        let e = Openai.transcribe(&audio(), &opts("whisper-1"), &creds(&mock, "k-12345")).unwrap_err();
        assert_eq!(e.code, SttErrorCode::Other);
        assert_eq!(e.message, "OpenAI returned an unexpected response.");
    }

    #[test]
    fn unreachable_server() {
        let c = Credentials { api_key: Some("k-12345".into()), base_url: Some("http://127.0.0.1:1".into()), extra: vec![] };
        let e = Openai.transcribe(&audio(), &opts("whisper-1"), &c).unwrap_err();
        assert_eq!(e.code, SttErrorCode::Unreachable);
    }

    #[test]
    fn oversized_audio_is_rejected_locally() {
        let big = Audio { samples: vec![0; 13 * 1024 * 1024], sample_rate: 16_000 };
        let c = Credentials { api_key: Some("k-12345".into()), base_url: Some("http://127.0.0.1:1".into()), extra: vec![] };
        let e = Openai.transcribe(&big, &opts("whisper-1"), &c).unwrap_err();
        assert_eq!(e.code, SttErrorCode::PayloadTooLarge);
    }
}
