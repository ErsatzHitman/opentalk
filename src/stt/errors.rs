//! Provider error classification (port of OpenWhispr providerHttpErrors.js `codeForStatus`,
//! `redactProviderBody` and the English message table). Pure: no I/O, no regex crate.
use crate::core::http::{HttpError, HttpResponse};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SttErrorCode {
    AuthFailed,
    QuotaExhausted,
    AccessDenied,
    RateLimited,
    ModelNotFound,
    PayloadTooLarge,
    BadRequest,
    Timeout,
    Unavailable,
    Unreachable,
    NoSpeech,
    StreamingOnly,
    NotConfigured,
    Cancelled,
    Other,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SttError {
    pub code: SttErrorCode,
    /// Short user-facing message, e.g. "OpenAI rejected your API key."
    pub message: String,
    /// Redacted technical detail for logs/history (no secrets).
    pub detail: String,
    pub status: Option<u16>,
}

impl SttError {
    pub fn new(code: SttErrorCode, message: impl Into<String>) -> Self {
        Self { code, message: message.into(), detail: String::new(), status: None }
    }

    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = detail.into();
        self
    }
}

impl std::fmt::Display for SttError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for SttError {}

/// Name used in place of the provider for user-run endpoints (Custom / Azure / self-hosted).
pub const SELF_HOSTED_NAME: &str = "Your server";

const MAX_DETAIL_CHARS: usize = 500;

// ---- signals (hand-rolled equivalents of the OpenWhispr regexes; input is ASCII-lowercased) ----

fn is_sep(b: u8) -> bool {
    matches!(b, b' ' | b'_' | b'-')
}

/// `invalid[ _-]?(x-)?api[ _-]?key`
fn invalid_api_key(t: &str) -> bool {
    let b = t.as_bytes();
    let mut from = 0;
    while let Some(p) = t[from..].find("invalid") {
        let mut i = from + p + "invalid".len();
        if b.get(i).is_some_and(|c| is_sep(*c)) {
            i += 1;
        }
        if t.get(i..).is_some_and(|s| s.starts_with("x-")) {
            i += 2;
        }
        if t.get(i..).is_some_and(|s| s.starts_with("api")) {
            i += 3;
            if b.get(i).is_some_and(|c| is_sep(*c)) {
                i += 1;
            }
            if t.get(i..).is_some_and(|s| s.starts_with("key")) {
                return true;
            }
        }
        from += p + "invalid".len();
    }
    false
}

fn invalid_key_signal(t: &str) -> bool {
    ["incorrect api key", "api_key_invalid", "authentication_error", "unauthorized"].iter().any(|s| t.contains(s))
        || invalid_api_key(t)
}

fn quota_signal(t: &str) -> bool {
    ["insufficient_quota", "credit balance", "insufficient credits", "insufficient balance", "insufficient funds", "out of credits", "payment required"]
        .iter()
        .any(|s| t.contains(s))
}

/// `model_not_found | model[^.\n]{0,80}(not found|does not exist|not available|is not supported)
///  | (unknown|invalid|unsupported) model`
fn model_signal(t: &str) -> bool {
    if t.contains("model_not_found") || ["unknown model", "invalid model", "unsupported model"].iter().any(|s| t.contains(s)) {
        return true;
    }
    let mut from = 0;
    while let Some(p) = t[from..].find("model") {
        let start = from + p + "model".len();
        let rest = &t[start..];
        let seg_end = rest.find(['.', '\n']).unwrap_or(rest.len());
        let seg = &rest[..seg_end];
        if ["not found", "does not exist", "not available", "is not supported"]
            .iter()
            .any(|ph| seg.find(ph).is_some_and(|at| at <= 80))
        {
            return true;
        }
        from = start;
    }
    false
}

fn code_for_status(status: u16, text: &str, self_hosted: bool) -> SttErrorCode {
    let t = text.to_ascii_lowercase();
    use SttErrorCode as C;
    match status {
        401 => C::AuthFailed,
        400 | 403 if invalid_key_signal(&t) => C::AuthFailed,
        402 => C::QuotaExhausted,
        400 | 403 | 429 if quota_signal(&t) => C::QuotaExhausted,
        403 => C::AccessDenied,
        429 => C::RateLimited,
        // A self-hosted 404 is far more often a wrong URL than a wrong model.
        404 => {
            if self_hosted && !model_signal(&t) {
                C::Other
            } else {
                C::ModelNotFound
            }
        }
        400 if model_signal(&t) => C::ModelNotFound,
        413 => C::PayloadTooLarge,
        400 | 415 | 422 => C::BadRequest,
        408 | 504 => C::Timeout,
        500..=599 => C::Unavailable,
        _ => C::Other,
    }
}

/// English user-facing sentence for a code (OpenWhispr `ENGLISH` table).
pub fn message_for(code: SttErrorCode, provider: &str, model: Option<&str>, self_hosted: bool) -> String {
    use SttErrorCode as C;
    let model = model.map(str::trim).filter(|m| !m.is_empty());
    if self_hosted {
        return match code {
            C::AuthFailed => "Your server rejected the API key.".into(),
            C::AccessDenied => "Your server denied access. The key may not include this model.".into(),
            C::QuotaExhausted => "Your server says the account is out of credit.".into(),
            C::RateLimited => "Your server rate-limited the request. Wait a moment and try again.".into(),
            C::ModelNotFound => match model {
                Some(m) => format!("Your server doesn't recognize the model “{m}”."),
                None => "Your server doesn't recognize the selected model.".into(),
            },
            C::PayloadTooLarge => "This recording is too large for your server.".into(),
            C::BadRequest => "Your server couldn't process this request.".into(),
            C::Unavailable => "Your server is having problems right now. Try again shortly.".into(),
            C::Timeout => "Your server took too long to respond.".into(),
            C::Unreachable => "Couldn't reach your server. Check that it's running.".into(),
            C::NotConfigured => "No server URL is set for the custom endpoint.".into(),
            _ => generic_message(code, SELF_HOSTED_NAME, model),
        };
    }
    match code {
        C::AuthFailed => format!("{provider} rejected your API key."),
        C::AccessDenied => format!("{provider} denied access. Your key may not include this model."),
        C::QuotaExhausted => format!("Your {provider} account is out of credit."),
        C::RateLimited => format!("{provider} rate-limited the request. Wait a moment and try again."),
        C::ModelNotFound => match model {
            Some(m) => format!("{provider} doesn't recognize the model “{m}”."),
            None => format!("{provider} doesn't recognize the selected model."),
        },
        C::PayloadTooLarge => format!("This recording is too large for {provider}."),
        C::BadRequest => format!("{provider} couldn't process this request."),
        C::Unavailable => format!("{provider} is having problems right now. Try again shortly."),
        C::Timeout => format!("{provider} took too long to respond."),
        C::Unreachable => format!("Couldn't reach {provider}. Check your connection."),
        _ => generic_message(code, provider, model),
    }
}

fn generic_message(code: SttErrorCode, provider: &str, model: Option<&str>) -> String {
    use SttErrorCode as C;
    match code {
        C::NoSpeech => "No audio detected".into(),
        C::StreamingOnly => match model {
            Some(m) => format!("{provider} {m} only supports live transcription. Turn on streaming or choose another model."),
            None => format!("{provider} only supports live transcription. Choose another provider for file uploads and retries."),
        },
        C::NotConfigured => format!("No API key is set for {provider}. Add it in Settings."),
        C::Cancelled => "Transcription cancelled.".into(),
        _ => format!("{provider} returned an unexpected error."),
    }
}

/// Classify an HTTP failure for `provider` (display name). Body is redacted before use.
pub fn classify(provider: &str, status: u16, body: &str) -> SttError {
    classify_for(provider, None, false, status, body)
}

/// Like [`classify`], with the model (for "doesn't recognize the model" text) and the
/// self-hosted flag (user-run endpoints: "Your server ..." wording, 404 is a wrong URL).
pub fn classify_for(provider: &str, model: Option<&str>, self_hosted: bool, status: u16, body: &str) -> SttError {
    let code = code_for_status(status, body, self_hosted);
    SttError {
        code,
        message: message_for(code, provider, model, self_hosted),
        detail: format!("HTTP {status}: {}", sanitize_detail(body)),
        status: Some(status),
    }
}

/// Map a finished request to its body, or a classified error (transport failure or non-2xx).
pub fn check_response(
    provider: &str,
    model: Option<&str>,
    self_hosted: bool,
    res: Result<HttpResponse, HttpError>,
) -> Result<HttpResponse, SttError> {
    match res {
        Err(e) => Err(from_transport(provider, self_hosted, &e)),
        Ok(r) if r.ok() => Ok(r),
        Ok(r) => {
            let mut err = classify_for(provider, model, self_hosted, r.status, &r.body);
            let id = ["x-request-id", "request-id", "cf-ray"].iter().find_map(|h| r.header(h));
            if let Some(id) = id {
                err.detail.push_str(&format!(" (request id {})", redact(id)));
            }
            Err(err)
        }
    }
}

/// Transport-level failure (DNS/connect/TLS/timeout).
pub fn from_transport(provider: &str, self_hosted: bool, e: &HttpError) -> SttError {
    let (code, status) = match e {
        HttpError::Timeout => (SttErrorCode::Timeout, None),
        HttpError::Network(_) => (SttErrorCode::Unreachable, None),
    };
    SttError { code, message: message_for(code, provider, None, self_hosted), detail: redact(&e.to_string()), status }
}

pub fn not_configured(provider: &str) -> SttError {
    SttError::new(SttErrorCode::NotConfigured, message_for(SttErrorCode::NotConfigured, provider, None, false))
}

pub fn no_speech() -> SttError {
    SttError::new(SttErrorCode::NoSpeech, "No audio detected")
}

pub fn streaming_only(provider: &str, model: &str) -> SttError {
    SttError::new(SttErrorCode::StreamingOnly, message_for(SttErrorCode::StreamingOnly, provider, Some(model), false))
}

/// A 2xx response whose body we cannot use.
pub fn bad_response(provider: &str, self_hosted: bool, what: &str, body: &str) -> SttError {
    let name = if self_hosted { SELF_HOSTED_NAME } else { provider };
    SttError::new(SttErrorCode::Other, format!("{name} returned an unexpected response."))
        .with_detail(format!("{what}: {}", sanitize_detail(body)))
}

// ---- redaction ----

fn is_word(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

fn run_len(b: &[u8], from: usize, ok: impl Fn(u8) -> bool) -> usize {
    b[from..].iter().take_while(|c| ok(**c)).count()
}

/// Strip API keys / bearer tokens from text (`sk-`, `sk-or-`, `gsk_`, `AIza`, `xai-`,
/// `Bearer ...`, `Token ...`).
pub fn redact(text: &str) -> String {
    let b = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    let mut copied = 0; // start of the not-yet-copied span
    while i < b.len() {
        // A secret starts at a word boundary.
        if i > 0 && is_word(b[i - 1]) {
            i += 1;
            continue;
        }
        let rest = &b[i..];
        let mut hit: Option<(usize, &str)> = None; // (end, replacement)
        let starts = |p: &str| rest.len() >= p.len() && rest[..p.len()].eq_ignore_ascii_case(p.as_bytes());
        if starts("bearer") || starts("token") {
            let word = if starts("bearer") { 6 } else { 5 };
            let ws = run_len(b, i + word, |c| c.is_ascii_whitespace());
            if ws > 0 {
                let n = run_len(b, i + word + ws, |c| c.is_ascii_alphanumeric() || b"._~+/=-".contains(&c));
                if n >= 4 {
                    hit = Some((i + word + ws + n, if word == 6 { "Bearer [redacted]" } else { "Token [redacted]" }));
                }
            }
        } else if rest.starts_with(b"sk-") {
            let n = run_len(b, i + 3, |c| c.is_ascii_alphanumeric() || b"_*.-".contains(&c));
            if n >= 4 {
                hit = Some((i + 3 + n, "[redacted]"));
            }
        } else if rest.starts_with(b"gsk_") || rest.starts_with(b"xai-") {
            let n = run_len(b, i + 4, |c| c.is_ascii_alphanumeric() || c == b'*');
            if n >= 4 {
                hit = Some((i + 4 + n, "[redacted]"));
            }
        } else if rest.starts_with(b"AIza") {
            let n = run_len(b, i + 4, |c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-');
            if n >= 4 {
                hit = Some((i + 4 + n, "[redacted]"));
            }
        }
        match hit {
            Some((end, rep)) => {
                out.push_str(&text[copied..i]);
                out.push_str(rep);
                i = end;
                copied = end;
            }
            None => i += 1,
        }
    }
    out.push_str(&text[copied..]);
    out
}

/// Redacted, single-line, length-bounded body for `SttError::detail`.
fn sanitize_detail(body: &str) -> String {
    let lower = body.to_ascii_lowercase();
    if ["<!doctype", "<html", "<body", "<head"].iter().any(|t| lower.contains(t)) {
        return "<HTML response>".into();
    }
    let red = redact(body);
    let flat = red.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() > MAX_DETAIL_CHARS {
        let cut: String = flat.chars().take(MAX_DETAIL_CHARS).collect();
        format!("{cut}…")
    } else {
        flat
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use SttErrorCode as C;

    #[test]
    fn status_mapping() {
        let code = |s, b: &str| classify("OpenAI", s, b).code;
        assert_eq!(code(401, ""), C::AuthFailed);
        assert_eq!(code(403, "{\"error\":\"Invalid API key provided\"}"), C::AuthFailed);
        assert_eq!(code(400, "API_KEY_INVALID: API key not valid"), C::AuthFailed);
        assert_eq!(code(400, "invalid x-api-key"), C::AuthFailed);
        assert_eq!(code(402, ""), C::QuotaExhausted);
        assert_eq!(code(429, "insufficient_quota: You exceeded your current quota"), C::QuotaExhausted);
        assert_eq!(code(400, "Your credit balance is too low"), C::QuotaExhausted);
        assert_eq!(code(403, "nope"), C::AccessDenied);
        assert_eq!(code(429, "Rate limit reached for requests"), C::RateLimited);
        // Gemini/Groq 429s mention quota/billing but are plain rate limits.
        assert_eq!(code(429, "You exceeded your current quota, please check your plan and billing details"), C::RateLimited);
        assert_eq!(code(404, ""), C::ModelNotFound);
        assert_eq!(code(400, "The model `foo` does not exist"), C::ModelNotFound);
        assert_eq!(code(400, "model_not_found"), C::ModelNotFound);
        assert_eq!(code(413, ""), C::PayloadTooLarge);
        assert_eq!(code(400, "bad"), C::BadRequest);
        assert_eq!(code(415, ""), C::BadRequest);
        assert_eq!(code(422, ""), C::BadRequest);
        assert_eq!(code(408, ""), C::Timeout);
        assert_eq!(code(504, ""), C::Timeout);
        assert_eq!(code(500, ""), C::Unavailable);
        assert_eq!(code(503, ""), C::Unavailable);
        assert_eq!(code(302, ""), C::Other);
    }

    #[test]
    fn model_signal_window_and_sentence_boundary() {
        assert!(model_signal("the model `x` is not supported"));
        assert!(!model_signal("model. not found"));
        let far = format!("model{} not found", " ".repeat(90));
        assert!(!model_signal(&far));
    }

    #[test]
    fn self_hosted_404_is_a_wrong_url() {
        let e = classify_for("Custom", Some("whisper-1"), true, 404, "page not found");
        assert_eq!(e.code, C::Other);
        assert_eq!(e.message, "Your server returned an unexpected error.");
        let e = classify_for("Custom", Some("whisper-9"), true, 404, "model whisper-9 does not exist");
        assert_eq!(e.code, C::ModelNotFound);
        assert_eq!(e.message, "Your server doesn't recognize the model “whisper-9”.");
    }

    #[test]
    fn friendly_messages() {
        assert_eq!(classify("OpenAI", 401, "").message, "OpenAI rejected your API key.");
        assert_eq!(classify("Groq", 429, "").message, "Groq rate-limited the request. Wait a moment and try again.");
        assert_eq!(classify("xAI", 402, "").message, "Your xAI account is out of credit.");
        assert_eq!(classify("Mistral", 503, "").message, "Mistral is having problems right now. Try again shortly.");
        assert_eq!(classify_for("Gemini", Some("g-1"), false, 404, "").message, "Gemini doesn't recognize the model “g-1”.");
        assert_eq!(classify("Gemini", 413, "").message, "This recording is too large for Gemini.");
    }

    #[test]
    fn redacts_known_secret_shapes() {
        let s = "key sk-proj-abcdefghij1234 and sk-or-v1-0123456789abcdef plus gsk_abcDEF123 AIzaSyA-bc_def xai-abcd1234 \
                 Authorization: Bearer abc.def-123 / Token 0123456789abcdef";
        let r = redact(s);
        for leak in ["abcdefghij1234", "0123456789abcdef", "abcDEF123", "SyA-bc_def", "abcd1234", "abc.def-123"] {
            assert!(!r.contains(leak), "leaked {leak} in {r}");
        }
        assert!(r.contains("Bearer [redacted]"));
        assert!(r.contains("Token [redacted]"));
        assert!(redact("sk-abc").contains("sk-abc"), "too short to be a key");
        assert_eq!(redact("task-based desk-1234567"), "task-based desk-1234567", "needs a word boundary");
    }

    #[test]
    fn detail_is_redacted_flat_and_bounded() {
        let e = classify("OpenAI", 401, "{\n \"error\": \"bad key sk-abcdefghijkl\"\n}");
        assert!(!e.detail.contains("abcdefghijkl"));
        assert!(!e.detail.contains('\n'));
        assert!(e.detail.starts_with("HTTP 401: "));
        assert_eq!(e.status, Some(401));
        let long = classify("OpenAI", 500, &"x ".repeat(1000));
        assert!(long.detail.chars().count() < 520);
        assert_eq!(classify("OpenAI", 502, "<!DOCTYPE html><html>").detail, "HTTP 502: <HTML response>");
    }

    #[test]
    fn transport_errors() {
        assert_eq!(from_transport("Groq", false, &HttpError::Timeout).code, C::Timeout);
        let e = from_transport("Groq", false, &HttpError::Network("dns".into()));
        assert_eq!(e.code, C::Unreachable);
        assert_eq!(e.message, "Couldn't reach Groq. Check your connection.");
    }
}
