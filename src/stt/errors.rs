//! Provider error classification (port of OpenWhispr providerHttpErrors.js codeForStatus).
//! TASK stt-batch implements `classify` and `redact`.
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
}

impl std::fmt::Display for SttError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

/// Classify an HTTP failure for `provider` (display name). Body is redacted before use.
pub fn classify(provider: &str, status: u16, body: &str) -> SttError {
    let _ = (provider, status, body);
    todo!("stt-batch")
}

/// Strip API keys / bearer tokens from text (sk-, gsk_, AIza, xai-, sk-or-, Bearer ...).
pub fn redact(text: &str) -> String {
    let _ = text;
    todo!("stt-batch")
}
