//! Enterprise LLM clouds for AI cleanup: Azure OpenAI chat completions (`api-key` header),
//! AWS Bedrock Converse (hand-rolled SigV4 over sha2/hmac) and Google Vertex AI
//! (service-account JSON -> RS256 JWT -> cached OAuth token -> generateContent).
//!
//! `OPENTALK_LLM_BASE_URL` replaces each provider's origin (scheme + host) so tests and mock
//! servers can stand in for the real cloud; the path/query layout stays identical.
mod azure;
mod bedrock;
mod vertex;

use super::LlmError;
use crate::core::config::{Config, LlmProviderId};
use crate::core::http::HttpError;
use std::time::{Duration, Instant};

/// One-shot completion for enterprise providers (AzureOpenai | Bedrock | Vertex).
pub fn complete(cfg: &Config, system: &str, user: &str, timeout_ms: u64) -> Result<String, LlmError> {
    let base = std::env::var("OPENTALK_LLM_BASE_URL").ok().filter(|s| !s.trim().is_empty());
    complete_with(cfg, system, user, timeout_ms, base.as_deref())
}

/// [`complete`] with an explicit origin override (no process-global env read).
///
/// The request runs on a short-lived thread and the caller waits at most `timeout + 300 ms`, so a
/// stall outside ureq's own deadline (DNS, TLS handshake, DPAPI) cannot hold up the paste.
pub(crate) fn complete_with(cfg: &Config, system: &str, user: &str, timeout_ms: u64, base: Option<&str>) -> Result<String, LlmError> {
    if !matches!(cfg.ai_cleanup.provider, LlmProviderId::AzureOpenai | LlmProviderId::Bedrock | LlmProviderId::Vertex) {
        return Err(LlmError::NotConfigured);
    }
    let deadline = Deadline::new(timeout_ms);
    let (cfg, system, user, base) = (cfg.clone(), system.to_string(), user.to_string(), base.map(str::to_string));
    let (tx, rx) = std::sync::mpsc::channel();
    let spawned = std::thread::Builder::new().name("llm-enterprise".into()).spawn(move || {
        let _ = tx.send(dispatch(&cfg, &system, &user, &deadline, base.as_deref()));
    });
    if let Err(e) = spawned {
        return Err(LlmError::Network(format!("cannot start request thread: {e}")));
    }
    match rx.recv_timeout(Duration::from_millis(timeout_ms) + Duration::from_millis(300)) {
        Ok(r) => r,
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => Err(LlmError::Timeout),
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => Err(LlmError::Network("request thread ended".into())),
    }
}

fn dispatch(cfg: &Config, system: &str, user: &str, deadline: &Deadline, base: Option<&str>) -> Result<String, LlmError> {
    let base = base.map(|b| b.trim().trim_end_matches('/')).filter(|b| !b.is_empty());
    match cfg.ai_cleanup.provider {
        LlmProviderId::AzureOpenai => azure::complete(cfg, system, user, deadline, base),
        LlmProviderId::Bedrock => bedrock::complete(cfg, system, user, deadline, base),
        LlmProviderId::Vertex => vertex::complete(cfg, system, user, deadline, base),
        _ => Err(LlmError::NotConfigured),
    }
}

/// Wall-clock budget shared by every request one completion makes (token fetch + call).
pub(crate) struct Deadline(Instant);

impl Deadline {
    pub(crate) fn new(ms: u64) -> Self { Self(Instant::now() + Duration::from_millis(ms)) }
    /// Time left, or `Timeout` once spent.
    pub(crate) fn remaining(&self) -> Result<Duration, LlmError> {
        let left = self.0.saturating_duration_since(Instant::now());
        if left.is_zero() { Err(LlmError::Timeout) } else { Ok(left) }
    }
}

/// Secret lookup with the documented `OPENTALK_SECRET_<NAME>` env override first.
pub(crate) fn secret(name: &str) -> Option<String> {
    let env = format!("OPENTALK_SECRET_{}", name.to_ascii_uppercase());
    if let Ok(v) = std::env::var(&env) {
        if !v.trim().is_empty() {
            return Some(v.trim().to_string());
        }
    }
    crate::core::secrets::get(name).map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

pub(crate) fn net_err(e: HttpError) -> LlmError {
    match e {
        HttpError::Timeout => LlmError::Timeout,
        HttpError::Network(m) => LlmError::Network(m),
    }
}

/// Non-2xx response -> `LlmError::Http` with a short, human detail (never the whole body).
pub(crate) fn http_error(status: u16, body: &str) -> LlmError {
    let detail = serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| {
            let m = v.pointer("/error/message").or_else(|| v.get("message")).or_else(|| v.get("error_description")).or_else(|| v.get("error"));
            m.and_then(|m| m.as_str()).map(str::to_string)
        })
        .unwrap_or_else(|| body.to_string());
    LlmError::Http { status, detail: detail.chars().take(300).collect() }
}

/// Output budget for a cleanup of `user`: roughly the input length, bounded.
pub(crate) fn max_tokens_for(user: &str) -> u32 {
    (user.len() as u32 / 3).clamp(256, 4096)
}

/// `host[:port]` of an absolute URL.
pub(crate) fn authority(url: &str) -> &str {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    rest.split(['/', '?']).next().unwrap_or(rest)
}

/// Percent-encode everything except RFC 3986 unreserved characters.
pub(crate) fn uri_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
pub(crate) mod testutil {
    //! Minimal keep-alive HTTP/1.1 mock server on 127.0.0.1 (plain http) for provider tests.
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Debug, Default)]
    pub struct Req {
        pub method: String,
        pub target: String,
        pub headers: Vec<(String, String)>,
        pub body: String,
    }

    impl Req {
        pub fn header(&self, name: &str) -> Option<&str> {
            self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
        }
    }

    pub struct Mock {
        pub base: String,
        pub reqs: Arc<Mutex<Vec<Req>>>,
    }

    /// Serves forever (detached threads); `handler(&Req) -> (status, json body)`.
    pub fn serve(handler: impl Fn(&Req) -> (u16, String) + Send + Sync + 'static) -> Mock {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let reqs = Arc::new(Mutex::new(Vec::new()));
        let handler = Arc::new(handler);
        let log = reqs.clone();
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                let Ok(conn) = conn else { continue };
                let (handler, log) = (handler.clone(), log.clone());
                std::thread::spawn(move || {
                    let mut w = conn.try_clone().unwrap();
                    let mut r = BufReader::new(conn);
                    loop {
                        let mut line = String::new();
                        if r.read_line(&mut line).unwrap_or(0) == 0 { return; }
                        let mut parts = line.split_whitespace();
                        let mut req = Req { method: parts.next().unwrap_or("").into(), target: parts.next().unwrap_or("").into(), ..Req::default() };
                        let mut len = 0usize;
                        loop {
                            let mut h = String::new();
                            if r.read_line(&mut h).unwrap_or(0) == 0 { return; }
                            let h = h.trim_end();
                            if h.is_empty() { break; }
                            if let Some((k, v)) = h.split_once(':') {
                                if k.eq_ignore_ascii_case("content-length") { len = v.trim().parse().unwrap_or(0); }
                                req.headers.push((k.trim().into(), v.trim().into()));
                            }
                        }
                        let mut body = vec![0u8; len];
                        if r.read_exact(&mut body).is_err() { return; }
                        req.body = String::from_utf8_lossy(&body).into_owned();
                        let (status, resp) = handler(&req);
                        log.lock().unwrap().push(req);
                        let out = format!("HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{resp}", resp.len());
                        if w.write_all(out.as_bytes()).is_err() { return; }
                    }
                });
            }
        });
        Mock { base, reqs }
    }

    pub fn cfg(provider: crate::core::config::LlmProviderId) -> crate::core::config::Config {
        let mut c = crate::core::config::Config::default();
        c.ai_cleanup.provider = provider;
        c
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_enterprise_provider_is_not_configured() {
        let cfg = testutil::cfg(LlmProviderId::Openai);
        assert_eq!(complete_with(&cfg, "s", "u", 1000, None), Err(LlmError::NotConfigured));
    }

    #[test]
    fn helpers() {
        assert_eq!(uri_encode("a b/c:1~"), "a%20b%2Fc%3A1~");
        assert_eq!(authority("http://127.0.0.1:8080/x?y"), "127.0.0.1:8080");
        assert_eq!(authority("https://h.example.com"), "h.example.com");
        assert_eq!(max_tokens_for("x"), 256);
        assert_eq!(max_tokens_for(&"x".repeat(100_000)), 4096);
        let LlmError::Http { status, detail } = http_error(429, r#"{"error":{"message":"slow down"}}"#) else { panic!() };
        assert_eq!((status, detail.as_str()), (429, "slow down"));
        assert!(matches!(http_error(500, "boom"), LlmError::Http { detail, .. } if detail == "boom"));
    }

    #[test]
    fn deadline_expires() {
        let d = Deadline::new(0);
        assert_eq!(d.remaining(), Err(LlmError::Timeout));
        assert!(Deadline::new(5000).remaining().unwrap() > Duration::from_secs(1));
    }
}
