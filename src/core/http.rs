//! Shared HTTP client: one keep-alive `ureq::Agent` over schannel (native-tls),
//! a multipart builder, and connection pre-warming.
//!
//! Non-2xx responses are returned as `Ok(HttpResponse)` with the status set — callers classify.
//! Only transport failures are `Err(HttpError)`.
use std::io::Read;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

pub const USER_AGENT: &str = concat!("OpenTalk/", env!("CARGO_PKG_VERSION"), " (Windows)");

static AGENT: OnceLock<ureq::Agent> = OnceLock::new();

pub fn agent() -> &'static ureq::Agent {
    AGENT.get_or_init(|| {
        let tls = native_tls::TlsConnector::new().expect("schannel TLS available");
        ureq::AgentBuilder::new()
            .tls_connector(Arc::new(tls))
            .user_agent(USER_AGENT)
            .timeout_connect(Duration::from_secs(10))
            .max_idle_connections_per_host(4)
            .build()
    })
}

#[derive(Debug, Clone)]
pub struct HttpResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl HttpResponse {
    pub fn ok(&self) -> bool { (200..300).contains(&self.status) }
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
    pub fn json(&self) -> Result<serde_json::Value, serde_json::Error> { serde_json::from_str(&self.body) }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HttpError {
    Timeout,
    /// DNS/connect/TLS/IO failure (message is safe to log; contains no secrets).
    Network(String),
}

impl std::fmt::Display for HttpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HttpError::Timeout => write!(f, "request timed out"),
            HttpError::Network(m) => write!(f, "network error: {m}"),
        }
    }
}

/// Classify a transport/IO error message. Windows reports socket deadlines as WSAETIMEDOUT
/// ("... did not properly respond ... (os error 10060)"), which never says "timed out".
fn transport_error(msg: String) -> HttpError {
    let low = msg.to_ascii_lowercase();
    if low.contains("timed out") || low.contains("timeout") || low.contains("os error 10060") || low.contains("did not properly respond") {
        HttpError::Timeout
    } else {
        HttpError::Network(msg)
    }
}

fn convert(res: Result<ureq::Response, ureq::Error>) -> Result<HttpResponse, HttpError> {
    let resp = match res {
        Ok(r) => r,
        Err(ureq::Error::Status(_, r)) => r,
        Err(ureq::Error::Transport(t)) => return Err(transport_error(t.to_string())),
    };
    let status = resp.status();
    let headers = resp
        .headers_names()
        .into_iter()
        .filter_map(|n| resp.header(&n).map(|v| (n.clone(), v.to_string())))
        .collect();
    let mut body = String::new();
    resp.into_reader()
        .take(32 * 1024 * 1024)
        .read_to_string(&mut body)
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock => HttpError::Timeout,
            _ => transport_error(e.to_string()),
        })?;
    Ok(HttpResponse { status, headers, body })
}

/// POST raw bytes.
pub fn post(url: &str, headers: &[(&str, &str)], content_type: &str, body: &[u8], timeout: Duration) -> Result<HttpResponse, HttpError> {
    let mut req = agent().post(url).timeout(timeout).set("Content-Type", content_type);
    for (k, v) in headers {
        req = req.set(k, v);
    }
    convert(req.send_bytes(body))
}

/// POST a JSON value.
pub fn post_json(url: &str, headers: &[(&str, &str)], body: &serde_json::Value, timeout: Duration) -> Result<HttpResponse, HttpError> {
    post(url, headers, "application/json", body.to_string().as_bytes(), timeout)
}

pub fn get(url: &str, headers: &[(&str, &str)], timeout: Duration) -> Result<HttpResponse, HttpError> {
    let mut req = agent().get(url).timeout(timeout);
    for (k, v) in headers {
        req = req.set(k, v);
    }
    convert(req.call())
}

pub fn delete(url: &str, headers: &[(&str, &str)], timeout: Duration) -> Result<HttpResponse, HttpError> {
    let mut req = agent().delete(url).timeout(timeout);
    for (k, v) in headers {
        req = req.set(k, v);
    }
    convert(req.call())
}

/// Open (and pool) a TLS connection to `url`'s origin in the background, so the real
/// request right after recording reuses a warm keep-alive socket. Errors are ignored.
pub fn prewarm(url: &str) {
    let origin = match url.find("://").and_then(|i| url[i + 3..].find('/').map(|j| &url[..i + 3 + j])) {
        Some(o) => o.to_string(),
        None => url.to_string(),
    };
    std::thread::Builder::new()
        .name("prewarm".into())
        .spawn(move || {
            let t = crate::core::timing::now_ms();
            let r = agent().head(&format!("{origin}/")).timeout(Duration::from_secs(5)).call();
            crate::log_debug!("prewarm {origin} -> {} in {:.0} ms", match &r { Ok(x) => x.status(), Err(ureq::Error::Status(c, _)) => *c, Err(_) => 0 }, crate::core::timing::now_ms() - t);
            // Drain so the connection returns to the pool.
            if let Ok(resp) | Err(ureq::Error::Status(_, resp)) = r {
                let _ = std::io::copy(&mut resp.into_reader().take(64 * 1024), &mut std::io::sink());
            }
        })
        .ok();
}

/// multipart/form-data builder (ureq 2 has none).
pub struct Multipart {
    boundary: String,
    buf: Vec<u8>,
}

impl Default for Multipart {
    fn default() -> Self { Self::new() }
}

impl Multipart {
    pub fn new() -> Self {
        let n = crate::core::timing::unix_ms() as u64 ^ (std::process::id() as u64) << 20;
        Self { boundary: format!("----OpenTalkBoundary{n:016x}"), buf: Vec::with_capacity(64 * 1024) }
    }
    pub fn text(&mut self, name: &str, value: &str) -> &mut Self {
        self.buf.extend_from_slice(format!("--{}\r\nContent-Disposition: form-data; name=\"{}\"\r\n\r\n", self.boundary, name).as_bytes());
        self.buf.extend_from_slice(value.as_bytes());
        self.buf.extend_from_slice(b"\r\n");
        self
    }
    pub fn file(&mut self, name: &str, filename: &str, content_type: &str, data: &[u8]) -> &mut Self {
        self.buf.extend_from_slice(
            format!(
                "--{}\r\nContent-Disposition: form-data; name=\"{}\"; filename=\"{}\"\r\nContent-Type: {}\r\n\r\n",
                self.boundary, name, filename, content_type
            )
            .as_bytes(),
        );
        self.buf.extend_from_slice(data);
        self.buf.extend_from_slice(b"\r\n");
        self
    }
    /// Returns (content-type header value, body).
    pub fn finish(mut self) -> (String, Vec<u8>) {
        self.buf.extend_from_slice(format!("--{}--\r\n", self.boundary).as_bytes());
        (format!("multipart/form-data; boundary={}", self.boundary), self.buf)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn multipart_shape() {
        let mut m = Multipart::new();
        m.text("model", "whisper-1").file("file", "audio.wav", "audio/wav", b"RIFF");
        let (ct, body) = m.finish();
        let b = String::from_utf8_lossy(&body);
        let boundary = ct.split("boundary=").nth(1).unwrap();
        assert!(b.starts_with(&format!("--{boundary}\r\n")));
        assert!(b.contains("name=\"model\"\r\n\r\nwhisper-1\r\n"));
        assert!(b.contains("filename=\"audio.wav\"\r\nContent-Type: audio/wav\r\n\r\nRIFF\r\n"));
        assert!(b.ends_with(&format!("--{boundary}--\r\n")));
    }

    #[test]
    fn timeout_messages_are_classified() {
        let wsa = "Network Error: A connection attempt failed because the connected party did not properly respond after a period of time (os error 10060)";
        assert_eq!(transport_error(wsa.into()), HttpError::Timeout);
        assert_eq!(transport_error("io: timed out reading response".into()), HttpError::Timeout);
        assert!(matches!(transport_error("Connect error: connection refused (os error 10061)".into()), HttpError::Network(_)));
    }
}
