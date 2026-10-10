//! Batch STT providers, one file each: openai.rs (incl. SSE when stream=true), groq.rs,
//! xai.rs, mistral.rs, gemini.rs, deepgram.rs (REST /v1/listen), tinfoil.rs (plain HTTPS),
//! corti.rs (OAuth + interaction flow), openrouter.rs (chat completions + input_audio),
//! custom.rs (OpenAI-compatible + Azure). AssemblyAI has no batch API (streaming only).
//!
//! Every provider takes its base URL from `Credentials::base_url` (tests point it at an
//! in-process mock) and falls back to the production URL.
pub mod corti;
pub mod custom;
pub mod deepgram;
pub mod gemini;
pub mod groq;
pub mod mistral;
pub mod openai;
pub mod openrouter;
pub mod tinfoil;
pub mod xai;

use super::errors::{self, SttError, SttErrorCode};
use super::{Audio, BatchProvider, Credentials, SttOptions, Transcript};
use crate::core::config::SttProviderId;
use crate::core::http::HttpResponse;
use std::time::Duration;

/// Largest upload accepted by the BYOK HTTP providers (OpenWhispr `BYOK_FILE_SIZE_LIMIT`).
pub(crate) const BYOK_FILE_LIMIT: usize = 25 * 1024 * 1024;

pub fn get(id: SttProviderId) -> &'static dyn BatchProvider {
    static OPENAI: Scrubbed = Scrubbed(&openai::Openai);
    static GROQ: Scrubbed = Scrubbed(&groq::Groq);
    static XAI: Scrubbed = Scrubbed(&xai::Xai);
    static MISTRAL: Scrubbed = Scrubbed(&mistral::Mistral);
    static GEMINI: Scrubbed = Scrubbed(&gemini::Gemini);
    static DEEPGRAM: Scrubbed = Scrubbed(&deepgram::Deepgram);
    static CORTI: Scrubbed = Scrubbed(&corti::Corti);
    static TINFOIL: Scrubbed = Scrubbed(&tinfoil::Tinfoil);
    static OPENROUTER: Scrubbed = Scrubbed(&openrouter::Openrouter);
    static CUSTOM: Scrubbed = Scrubbed(&custom::Custom);
    static ASSEMBLYAI: StreamingOnly = StreamingOnly(SttProviderId::Assemblyai);
    match id {
        SttProviderId::Openai => &OPENAI,
        SttProviderId::Groq => &GROQ,
        SttProviderId::Xai => &XAI,
        SttProviderId::Mistral => &MISTRAL,
        SttProviderId::Gemini => &GEMINI,
        SttProviderId::Deepgram => &DEEPGRAM,
        SttProviderId::Corti => &CORTI,
        SttProviderId::Tinfoil => &TINFOIL,
        SttProviderId::Openrouter => &OPENROUTER,
        SttProviderId::Custom => &CUSTOM,
        SttProviderId::Assemblyai => &ASSEMBLYAI,
    }
}

/// Dispatcher wrapper: registers the call's credentials with `errors::scope_secrets`, so
/// every error a provider builds on this thread (including providers that don't thread their
/// key explicitly) has the configured secrets removed before it is bounded.
struct Scrubbed(&'static dyn BatchProvider);

impl BatchProvider for Scrubbed {
    fn id(&self) -> SttProviderId {
        self.0.id()
    }
    fn prewarm_url(&self, creds: &Credentials) -> Option<String> {
        self.0.prewarm_url(creds)
    }
    fn transcribe(&self, audio: &Audio, opts: &SttOptions, creds: &Credentials) -> Result<Transcript, SttError> {
        let _scope = errors::scope_secrets(secrets(creds).into_iter().map(String::from));
        self.0.transcribe(audio, opts, creds)
    }
}

/// Providers with no batch HTTP API: always `StreamingOnly`.
struct StreamingOnly(SttProviderId);

impl BatchProvider for StreamingOnly {
    fn id(&self) -> SttProviderId {
        self.0
    }
    fn prewarm_url(&self, _: &Credentials) -> Option<String> {
        None
    }
    fn transcribe(&self, _: &Audio, opts: &SttOptions, _: &Credentials) -> Result<Transcript, SttError> {
        Err(errors::streaming_only(super::registry::provider(self.0).name, &opts.model))
    }
}

// ---- shared helpers ----

pub(crate) fn timeout(opts: &SttOptions) -> Duration {
    Duration::from_millis(if opts.timeout_ms == 0 { 30_000 } else { opts.timeout_ms })
}

/// `creds.base_url` or `default`, without trailing slashes.
pub(crate) fn base_url(creds: &Credentials, default: &str) -> String {
    let b = creds.base_url.as_deref().map(str::trim).filter(|b| !b.is_empty()).unwrap_or(default);
    b.trim_end_matches('/').to_string()
}

pub(crate) fn api_key<'a>(creds: &'a Credentials, provider: &str) -> Result<&'a str, SttError> {
    creds.api_key.as_deref().map(str::trim).filter(|k| !k.is_empty()).ok_or_else(|| errors::not_configured(provider))
}

/// The credential values of `creds` that must never reach an error text: the API key and the
/// Corti client secret.
pub(crate) fn secrets(creds: &Credentials) -> Vec<&str> {
    creds.api_key.as_deref().into_iter().chain(extra(creds, "client_secret")).collect()
}

pub(crate) fn extra<'a>(creds: &'a Credentials, name: &str) -> Option<&'a str> {
    creds.extra.iter().find(|(k, _)| k == name).map(|(_, v)| v.trim()).filter(|v| !v.is_empty())
}

/// `en-US` / `zh_CN` -> `en` / `zh` (OpenWhispr `getBaseLanguageCode`).
pub(crate) fn base_lang(lang: &str) -> String {
    lang.split(['-', '_']).next().unwrap_or(lang).trim().to_ascii_lowercase()
}

pub(crate) fn model_or<'a>(opts: &'a SttOptions, default: &'a str) -> &'a str {
    let m = opts.model.trim();
    if m.is_empty() { default } else { m }
}

pub(crate) fn check_size(provider: &str, wav_len: usize, cap: usize) -> Result<(), SttError> {
    if wav_len > cap {
        let mut e = SttError::new(
            SttErrorCode::PayloadTooLarge,
            errors::message_for(SttErrorCode::PayloadTooLarge, provider, None, provider == errors::SELF_HOSTED_NAME),
        );
        e.detail = format!("{wav_len} bytes exceeds the {cap} byte limit");
        return Err(e);
    }
    Ok(())
}

/// Dictionary hints as individual terms: split on commas / line breaks, `<` `>` removed,
/// trimmed, empties dropped (OpenWhispr `dictionaryTerms`).
pub(crate) fn split_terms(hints: &[String]) -> Vec<String> {
    hints
        .iter()
        .flat_map(|h| h.split([',', '\r', '\n']))
        .map(|t| t.replace(['<', '>'], "").trim().to_string())
        .filter(|t| !t.is_empty())
        .collect()
}

/// Whisper-style prompt: the hints joined with ", ", cut at the last comma inside
/// `max_chars` so no entry is sent half-spelled (OpenWhispr `trimDictionaryPrompt`).
pub(crate) fn join_prompt(hints: &[String], max_chars: usize) -> String {
    let joined = hints.iter().map(|h| h.trim()).filter(|h| !h.is_empty()).collect::<Vec<_>>().join(", ");
    trim_prompt(joined, max_chars)
}

pub(crate) fn trim_prompt(prompt: String, max_chars: usize) -> String {
    let Some((cut, _)) = prompt.char_indices().nth(max_chars) else { return prompt };
    let head = &prompt[..cut];
    match head.rfind(',') {
        Some(i) if i > 0 => head[..i].to_string(),
        _ => head.to_string(),
    }
}

/// Prompt budget by endpoint/model (OpenWhispr `dictionaryPromptLimit`).
pub(crate) fn prompt_limit(groq: bool, model: &str) -> usize {
    use crate::core::dictionary as d;
    let m = model.trim().to_ascii_lowercase();
    if groq {
        d::GROQ_PROMPT_CHARS
    } else if m.starts_with("gpt-4o") {
        d::TRANSCRIBE_PROMPT_CHARS
    } else if m == "gpt-transcribe" {
        d::TRANSCRIPTION_OVERFLOW_PROMPT_CHARS
    } else {
        d::WHISPER_PROMPT_CHARS
    }
}

/// `{"text": "...", "language": "..."}` bodies (OpenAI, Groq, xAI, Mistral, Tinfoil, custom).
pub(crate) fn text_json(provider: &str, self_hosted: bool, resp: &HttpResponse) -> Result<Transcript, SttError> {
    text_json_with(provider, self_hosted, resp, &[])
}

/// [`text_json`] that removes the configured `secrets` from error details.
pub(crate) fn text_json_with(provider: &str, self_hosted: bool, resp: &HttpResponse, secrets: &[&str]) -> Result<Transcript, SttError> {
    let v = resp.json().map_err(|e| errors::bad_response_with(provider, self_hosted, &format!("invalid JSON ({e})"), &resp.body, secrets))?;
    let Some(text) = v.get("text").and_then(|t| t.as_str()) else {
        return Err(errors::bad_response_with(provider, self_hosted, "no `text` field", &resp.body, secrets));
    };
    let detected_language = v.get("language").and_then(|l| l.as_str()).map(str::trim).filter(|l| !l.is_empty()).map(String::from);
    Ok(Transcript { text: text.to_string(), detected_language })
}

pub(crate) fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Cheap unique-ish id (no uuid crate): time + pid + counter.
pub(crate) fn unique_id() -> String {
    use std::sync::atomic::{AtomicU32, Ordering};
    static N: AtomicU32 = AtomicU32::new(0);
    let t = crate::core::timing::unix_ms() as u64;
    let n = N.fetch_add(1, Ordering::Relaxed);
    format!("{:08x}-{:04x}-{:04x}-{:04x}-{:012x}", (t >> 16) as u32, t as u16, std::process::id() as u16, n as u16, t.wrapping_mul(0x9E37_79B9_7F4A_7C15) & 0xFFFF_FFFF_FFFF)
}

#[cfg(test)]
pub(crate) mod testutil {
    //! In-process HTTP mock (plain http on 127.0.0.1) recording every request.
    use super::super::{Audio, Credentials, SttOptions};
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Debug)]
    pub struct Req {
        pub method: String,
        /// Path including the query string.
        pub path: String,
        /// Lower-cased header names.
        pub headers: Vec<(String, String)>,
        pub body: Vec<u8>,
    }

    impl Req {
        pub fn header(&self, name: &str) -> Option<&str> {
            self.headers.iter().find(|(k, _)| k == &name.to_ascii_lowercase()).map(|(_, v)| v.as_str())
        }
        pub fn body_str(&self) -> String {
            String::from_utf8_lossy(&self.body).into_owned()
        }
        pub fn json(&self) -> serde_json::Value {
            serde_json::from_slice(&self.body).expect("request body is JSON")
        }
        pub fn multipart(&self) -> Vec<Part> {
            parse_multipart(self.header("content-type").expect("content-type"), &self.body)
        }
    }

    #[derive(Clone, Debug)]
    pub struct Part {
        pub name: String,
        pub filename: Option<String>,
        pub content_type: Option<String>,
        pub data: Vec<u8>,
    }

    pub trait Parts {
        fn text(&self, name: &str) -> Vec<String>;
        fn names(&self) -> Vec<String>;
    }
    impl Parts for Vec<Part> {
        fn text(&self, name: &str) -> Vec<String> {
            self.iter().filter(|p| p.name == name).map(|p| String::from_utf8_lossy(&p.data).into_owned()).collect()
        }
        fn names(&self) -> Vec<String> {
            self.iter().map(|p| p.name.clone()).collect()
        }
    }

    pub fn parse_multipart(content_type: &str, body: &[u8]) -> Vec<Part> {
        let boundary = content_type.split("boundary=").nth(1).expect("boundary").trim().to_string();
        let delim = format!("--{boundary}").into_bytes();
        let mut parts = Vec::new();
        let mut pos = find(body, &delim, 0).expect("first boundary") + delim.len();
        loop {
            if body[pos..].starts_with(b"--") {
                break;
            }
            pos += 2; // CRLF after the boundary
            let hdr_end = find(body, b"\r\n\r\n", pos).expect("part headers");
            let head = String::from_utf8_lossy(&body[pos..hdr_end]).into_owned();
            let data_start = hdr_end + 4;
            let next = find(body, &delim, data_start).expect("closing boundary");
            let data = body[data_start..next - 2].to_vec(); // minus CRLF before the boundary
            let mut part = Part { name: String::new(), filename: None, content_type: None, data };
            for line in head.split("\r\n") {
                let lower = line.to_ascii_lowercase();
                if lower.starts_with("content-disposition:") {
                    part.name = quoted(line, "name=\"").unwrap_or_default();
                    part.filename = quoted(line, "filename=\"");
                } else if let Some(ct) = lower.strip_prefix("content-type:") {
                    part.content_type = Some(ct.trim().to_string());
                }
            }
            parts.push(part);
            pos = next + delim.len();
        }
        parts
    }

    fn quoted(line: &str, key: &str) -> Option<String> {
        // `name="` must not match inside `filename="`.
        let mut from = 0;
        while let Some(i) = line[from..].find(key) {
            let at = from + i;
            if at == 0 || !line.as_bytes()[at - 1].is_ascii_alphabetic() {
                let rest = &line[at + key.len()..];
                return rest.find('"').map(|e| rest[..e].to_string());
            }
            from = at + key.len();
        }
        None
    }

    fn find(hay: &[u8], needle: &[u8], from: usize) -> Option<usize> {
        hay.get(from..)?.windows(needle.len()).position(|w| w == needle).map(|p| p + from)
    }

    #[derive(Clone)]
    pub struct Resp {
        pub status: u16,
        pub content_type: String,
        pub body: Vec<u8>,
        pub extra_headers: Vec<(String, String)>,
    }

    impl Resp {
        pub fn json(status: u16, body: &str) -> Self {
            Self { status, content_type: "application/json".into(), body: body.as_bytes().to_vec(), extra_headers: vec![] }
        }
        pub fn sse(body: &str) -> Self {
            Self { status: 200, content_type: "text/event-stream".into(), body: body.as_bytes().to_vec(), extra_headers: vec![] }
        }
        pub fn text(status: u16, body: &str) -> Self {
            Self { status, content_type: "text/plain".into(), body: body.as_bytes().to_vec(), extra_headers: vec![] }
        }
    }

    pub struct Mock {
        pub url: String,
        reqs: Arc<Mutex<Vec<Req>>>,
    }

    impl Mock {
        pub fn requests(&self) -> Vec<Req> {
            self.reqs.lock().unwrap().clone()
        }
        /// The only request (asserts exactly one).
        pub fn one(&self) -> Req {
            let r = self.requests();
            assert_eq!(r.len(), 1, "expected one request, got {:?}", r.iter().map(|r| (&r.method, &r.path)).collect::<Vec<_>>());
            r.into_iter().next().unwrap()
        }
        /// Wait (bounded) until `n` requests arrived.
        pub fn wait_for(&self, n: usize) -> Vec<Req> {
            for _ in 0..200 {
                if self.reqs.lock().unwrap().len() >= n {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            self.requests()
        }
    }

    /// Serve `responses` in order (the last one repeats). Each response closes its connection.
    pub fn serve(responses: Vec<Resp>) -> Mock {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock");
        let url = format!("http://{}", listener.local_addr().unwrap());
        let reqs = Arc::new(Mutex::new(Vec::new()));
        let log = reqs.clone();
        std::thread::spawn(move || {
            let mut idx = 0;
            for stream in listener.incoming() {
                let Ok(mut s) = stream else { continue };
                let Some(req) = read_request(&mut s) else { continue };
                log.lock().unwrap().push(req);
                let resp = responses.get(idx).or(responses.last()).cloned().expect("at least one response");
                idx += 1;
                let reason = if resp.status < 300 { "OK" } else { "ERR" };
                let mut head = format!(
                    "HTTP/1.1 {} {reason}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n",
                    resp.status,
                    resp.content_type,
                    resp.body.len()
                );
                for (k, v) in &resp.extra_headers {
                    head.push_str(&format!("{k}: {v}\r\n"));
                }
                head.push_str("\r\n");
                let _ = s.write_all(head.as_bytes());
                let _ = s.write_all(&resp.body);
                let _ = s.flush();
            }
        });
        Mock { url, reqs }
    }

    fn read_request(s: &mut std::net::TcpStream) -> Option<Req> {
        s.set_read_timeout(Some(std::time::Duration::from_secs(10))).ok()?;
        let mut buf = Vec::new();
        let mut chunk = [0u8; 8192];
        let head_end = loop {
            if let Some(p) = find(&buf, b"\r\n\r\n", 0) {
                break p;
            }
            let n = s.read(&mut chunk).ok()?;
            if n == 0 {
                return None;
            }
            buf.extend_from_slice(&chunk[..n]);
        };
        let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
        let mut lines = head.split("\r\n");
        let mut first = lines.next()?.split(' ');
        let method = first.next()?.to_string();
        let path = first.next()?.to_string();
        let headers: Vec<(String, String)> = lines
            .filter_map(|l| l.split_once(':').map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string())))
            .collect();
        let len = headers.iter().find(|(k, _)| k == "content-length").and_then(|(_, v)| v.parse::<usize>().ok()).unwrap_or(0);
        let mut body = buf[head_end + 4..].to_vec();
        while body.len() < len {
            let n = s.read(&mut chunk).ok()?;
            if n == 0 {
                break;
            }
            body.extend_from_slice(&chunk[..n]);
        }
        Some(Req { method, path, headers, body })
    }

    pub fn audio() -> Audio {
        Audio { samples: (0..1600).map(|i| ((i % 100) as i16 - 50) * 100).collect(), sample_rate: 16_000 }
    }

    pub fn opts(model: &str) -> SttOptions {
        SttOptions { model: model.into(), language: None, hints: vec![], timeout_ms: 5_000 }
    }

    /// The error must carry no byte of `secret` anywhere a user, log or history could see it.
    pub fn assert_no_leak(e: &super::super::SttError, secret: &str) {
        for text in [e.message.clone(), e.detail.clone(), e.to_string(), format!("{e:?}")] {
            assert!(!text.contains(secret), "secret leaked into: {text}");
        }
    }

    pub fn creds(mock: &Mock, key: &str) -> Credentials {
        Credentials { api_key: Some(key.into()), base_url: Some(mock.url.clone()), extra: vec![] }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_is_cut_at_the_last_comma_within_budget() {
        let hints: Vec<String> = ["alpha", "bravo", "charlie"].iter().map(|s| s.to_string()).collect();
        assert_eq!(join_prompt(&hints, 100), "alpha, bravo, charlie");
        assert_eq!(join_prompt(&hints, 15), "alpha, bravo");
        assert_eq!(join_prompt(&hints, 4), "alph", "no comma inside the budget: hard cut");
        assert_eq!(join_prompt(&[], 10), "");
        // Counts characters, not bytes.
        let multi = vec!["ééé".to_string(), "ééé".to_string()];
        assert_eq!(join_prompt(&multi, 7), "ééé");
    }

    #[test]
    fn terms_are_split_and_sanitised() {
        let hints = vec!["a, b\nc".to_string(), "<x>".to_string(), " ".to_string(), "Smith <b>".to_string()];
        assert_eq!(split_terms(&hints), vec!["a", "b", "c", "x", "Smith b"]);
    }

    #[test]
    fn prompt_limits() {
        assert_eq!(prompt_limit(false, "gpt-4o-mini-transcribe"), 8000);
        assert_eq!(prompt_limit(false, "whisper-1"), 900);
        assert_eq!(prompt_limit(true, "whisper-large-v3"), 890);
        assert_eq!(prompt_limit(false, "gpt-transcribe"), 65_536);
    }

    #[test]
    fn languages_and_encoding() {
        assert_eq!(base_lang("en-US"), "en");
        assert_eq!(base_lang("zh_CN"), "zh");
        assert_eq!(base_lang("FIL"), "fil");
        assert_eq!(percent_encode("my deploy/1"), "my%20deploy%2F1");
    }

    #[test]
    fn registry_is_covered() {
        for p in crate::stt::registry::PROVIDERS {
            assert_eq!(get(p.id).id(), p.id);
        }
    }

    #[test]
    fn assemblyai_is_streaming_only() {
        let e = get(SttProviderId::Assemblyai).transcribe(&testutil::audio(), &testutil::opts("universal-3-5-pro"), &Credentials::default()).unwrap_err();
        assert_eq!(e.code, SttErrorCode::StreamingOnly);
    }
}
