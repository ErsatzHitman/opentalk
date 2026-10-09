//! Wire formats for the cleanup LLM providers. Each module is a pure request builder + response
//! parser (unit-tested against mock servers); `send` is the single transport with a hard deadline.
use super::LlmError;
use crate::core::config::LlmProviderId;
use serde_json::Value;
use std::sync::mpsc;
use std::time::Duration;

pub mod anthropic;
pub mod chat;
pub mod gemini;
pub mod openai;
pub mod sanitize;

/// Everything a provider needs for one completion.
#[derive(Clone, Debug)]
pub struct Req<'a> {
    pub provider: LlmProviderId,
    /// Resolved API base (no trailing slash), e.g. `https://api.openai.com/v1`.
    pub base_url: &'a str,
    pub model: &'a str,
    pub key: &'a str,
    pub system: &'a str,
    pub user: &'a str,
    /// Output token budget (see [`max_tokens`]).
    pub max_tokens: u32,
    pub timeout: Duration,
}

/// A fully built HTTP request.
#[derive(Clone, Debug)]
pub struct Built {
    pub url: String,
    pub headers: Vec<(&'static str, String)>,
    pub body: Value,
}

/// Output budget for `text_chars` of input: twice the input length (tokens are ~ chars / 4, so this
/// is generous), at least 256. Reasoning models also spend this budget on thinking, so they get 1024+.
pub fn max_tokens(text_chars: usize, reasoning: bool) -> u32 {
    let floor = if reasoning { 1024 } else { 256 };
    (text_chars.saturating_mul(2)).clamp(floor, 16_384) as u32
}

/// Model id without a `vendor/` prefix, lowercased (`openai/gpt-5-mini` -> `gpt-5-mini`).
pub fn bare_model(model: &str) -> String {
    model.rsplit('/').next().unwrap_or(model).to_ascii_lowercase()
}

pub fn is_o_series(bare: &str) -> bool {
    let mut it = bare.chars();
    it.next() == Some('o') && it.next().is_some_and(|c| c.is_ascii_digit())
}

/// OpenAI reasoning models (gpt-5*, o1/o3/o4*) reject `temperature`.
pub fn openai_rejects_temperature(model: &str) -> bool {
    let m = bare_model(model);
    m.starts_with("gpt-5") || is_o_series(&m)
}

/// Run the provider-specific completion.
pub fn complete(req: &Req) -> Result<String, LlmError> {
    match req.provider {
        LlmProviderId::Openai => openai::complete(req),
        LlmProviderId::Anthropic => anthropic::complete(req),
        LlmProviderId::Gemini => gemini::complete(req),
        LlmProviderId::Groq | LlmProviderId::Openrouter | LlmProviderId::Custom => chat::complete(req),
        // Enterprise ids are dispatched by `llm::cleanup` before reaching this module.
        LlmProviderId::AzureOpenai | LlmProviderId::Bedrock | LlmProviderId::Vertex => Err(LlmError::NotConfigured),
    }
}

/// POST `built` and return the parsed JSON body of a 2xx answer.
///
/// The request runs on a short-lived thread and the caller waits at most `timeout + 300 ms`, so a
/// stall outside ureq's own deadline (DNS, TLS handshake) still cannot hold up the paste. No retries.
pub fn send(built: Built, req: &Req) -> Result<Value, LlmError> {
    let started = crate::core::timing::now_ms();
    let timeout = req.timeout;
    let (tx, rx) = mpsc::channel();
    let spawned = std::thread::Builder::new().name("llm-cleanup".into()).spawn(move || {
        let headers: Vec<(&str, &str)> = built.headers.iter().map(|(k, v)| (*k, v.as_str())).collect();
        let r = crate::core::http::post_json(&built.url, &headers, &built.body, timeout);
        let _ = tx.send(r);
    });
    if let Err(e) = spawned {
        return Err(LlmError::Network(format!("cannot start request thread: {e}")));
    }
    let resp = match rx.recv_timeout(timeout + Duration::from_millis(300)) {
        Ok(Ok(r)) => r,
        Ok(Err(crate::core::http::HttpError::Timeout)) | Err(mpsc::RecvTimeoutError::Timeout) => {
            return Err(LlmError::Timeout);
        }
        Ok(Err(crate::core::http::HttpError::Network(m))) => {
            // Windows reports a socket read/connect deadline as WSAETIMEDOUT (10060) without "timed out".
            let low = m.to_ascii_lowercase();
            if low.contains("os error 10060") || low.contains("did not properly respond") || low.contains("timed out") {
                return Err(LlmError::Timeout);
            }
            return Err(LlmError::Network(redact(&m, req.key)));
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => return Err(LlmError::Network("request thread ended".into())),
    };
    crate::log_debug!(
        "llm {:?} {} -> {} in {:.0} ms",
        req.provider,
        req.model,
        resp.status,
        crate::core::timing::now_ms() - started
    );
    if !resp.ok() {
        return Err(LlmError::Http { status: resp.status, detail: error_detail(&resp.body, req.key) });
    }
    resp.json().map_err(|_| LlmError::InvalidOutput("response was not JSON".into()))
}

/// Short, single-line, secret-free description of an error body.
pub fn error_detail(body: &str, key: &str) -> String {
    let msg = serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| {
            let e = v.get("error")?;
            e.get("message").and_then(Value::as_str).or_else(|| e.as_str()).map(str::to_string)
        })
        .unwrap_or_else(|| body.to_string());
    let one_line = msg.split_whitespace().collect::<Vec<_>>().join(" ");
    let one_line = redact(&one_line, key);
    if one_line.chars().count() > 300 {
        let cut: String = one_line.chars().take(300).collect();
        format!("{cut}…")
    } else {
        one_line
    }
}

/// Replace any occurrence of the API key in `text`.
pub fn redact(text: &str, key: &str) -> String {
    if key.len() >= 8 { text.replace(key, "[redacted]") } else { text.to_string() }
}

/// Error object inside a 2xx answer (OpenRouter and some gateways do this): `{"error": {...}}`.
pub fn embedded_error(v: &Value, key: &str) -> Option<LlmError> {
    let e = v.get("error").filter(|e| !e.is_null())?;
    let status = e.get("code").and_then(Value::as_u64).filter(|c| (100..600).contains(c)).unwrap_or(502) as u16;
    Some(LlmError::Http { status, detail: error_detail(&v.to_string(), key) })
}

#[cfg(test)]
pub(crate) mod testutil {
    //! Tiny in-process HTTP server for request-shape tests (plain http on 127.0.0.1).
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::mpsc::{Receiver, channel};
    use std::time::Duration;

    #[derive(Debug, Clone)]
    pub struct Captured {
        pub request_line: String,
        pub headers: Vec<(String, String)>,
        pub body: String,
    }

    impl Captured {
        pub fn header(&self, name: &str) -> Option<&str> {
            self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
        }
        pub fn json(&self) -> serde_json::Value {
            serde_json::from_str(&self.body).expect("request body is JSON")
        }
        pub fn path(&self) -> &str {
            self.request_line.split_whitespace().nth(1).unwrap_or("")
        }
    }

    /// Serve exactly one request: wait `delay`, answer `status` + `body`. Returns the base URL and a
    /// receiver for the captured request.
    pub fn serve_once(status: u16, body: &str, delay: Duration) -> (String, Receiver<Captured>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let base = format!("http://{}", listener.local_addr().unwrap());
        let (tx, rx) = channel();
        let body = body.to_string();
        std::thread::spawn(move || {
            let Ok((mut s, _)) = listener.accept() else { return };
            s.set_read_timeout(Some(Duration::from_secs(5))).ok();
            let mut buf = Vec::new();
            let mut tmp = [0u8; 4096];
            let (head_end, content_len) = loop {
                let n = match s.read(&mut tmp) {
                    Ok(0) | Err(_) => return,
                    Ok(n) => n,
                };
                buf.extend_from_slice(&tmp[..n]);
                if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    let head = String::from_utf8_lossy(&buf[..p]).to_string();
                    let cl = head
                        .lines()
                        .find_map(|l| {
                            l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse::<usize>().unwrap_or(0))
                        })
                        .unwrap_or(0);
                    break (p + 4, cl);
                }
            };
            while buf.len() < head_end + content_len {
                match s.read(&mut tmp) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => buf.extend_from_slice(&tmp[..n]),
                }
            }
            let head = String::from_utf8_lossy(&buf[..head_end - 4]).to_string();
            let mut lines = head.lines();
            let request_line = lines.next().unwrap_or("").to_string();
            let headers = lines
                .filter_map(|l| l.split_once(':').map(|(k, v)| (k.trim().to_string(), v.trim().to_string())))
                .collect();
            let body_in = String::from_utf8_lossy(&buf[head_end..(head_end + content_len).min(buf.len())]).to_string();
            let _ = tx.send(Captured { request_line, headers, body: body_in });
            std::thread::sleep(delay);
            let resp = format!(
                "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = s.write_all(resp.as_bytes());
            let _ = s.flush();
        });
        (base, rx)
    }
}
