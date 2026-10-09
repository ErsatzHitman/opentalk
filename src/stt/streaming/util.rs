//! Small helpers shared by the streaming providers: error mapping, percent-encoding,
//! language normalisation and the 16 kHz -> 24 kHz upsampler OpenAI Realtime needs.
use crate::stt::{SttError, SttErrorCode};

pub fn err(code: SttErrorCode, message: impl Into<String>, detail: impl Into<String>) -> SttError {
    SttError { code, message: message.into(), detail: detail.into(), status: None }
}

/// Remove `secret` from `text` and bound its length, so it is safe for `SttError::detail`.
pub fn scrub(text: &str, secret: &str) -> String {
    let mut out = if secret.is_empty() { text.to_string() } else { text.replace(secret, "***") };
    if out.len() > 300 {
        let mut cut = 300;
        while !out.is_char_boundary(cut) {
            cut -= 1;
        }
        out.truncate(cut);
    }
    out
}

/// Map an HTTP status (WebSocket upgrade refusal or token mint) to a classified error.
pub fn status_error(provider: &str, status: u16, body: &str, secret: &str) -> SttError {
    let (code, message) = match status {
        401 => (SttErrorCode::AuthFailed, format!("{provider} rejected your API key.")),
        402 => (SttErrorCode::QuotaExhausted, format!("{provider} quota or credit exhausted.")),
        403 => (SttErrorCode::AccessDenied, format!("{provider} denied access for this key.")),
        404 => (SttErrorCode::ModelNotFound, format!("{provider} could not find that model or endpoint.")),
        408 | 504 => (SttErrorCode::Timeout, format!("{provider} timed out.")),
        413 => (SttErrorCode::PayloadTooLarge, format!("{provider} rejected the audio as too large.")),
        429 => (SttErrorCode::RateLimited, format!("{provider} rate limit reached. Try again shortly.")),
        500..=599 => (SttErrorCode::Unavailable, format!("{provider} is temporarily unavailable.")),
        _ => (SttErrorCode::BadRequest, format!("{provider} rejected the request (HTTP {status}).")),
    };
    SttError { code, message, detail: scrub(body, secret), status: Some(status) }
}

/// Classify a transport-level WebSocket failure.
pub fn ws_error(provider: &str, e: &tungstenite::Error, secret: &str) -> SttError {
    use tungstenite::Error as E;
    match e {
        E::Http(resp) => {
            let body = resp.body().as_deref().map(String::from_utf8_lossy).unwrap_or_default();
            status_error(provider, resp.status().as_u16(), &body, secret)
        }
        E::Io(io) if is_timeout(io) => err(SttErrorCode::Timeout, format!("{provider} did not respond in time."), io.to_string()),
        E::Io(io) => err(SttErrorCode::Unreachable, format!("Could not reach {provider}."), scrub(&io.to_string(), secret)),
        E::Tls(t) => err(SttErrorCode::Unreachable, format!("Secure connection to {provider} failed."), scrub(&t.to_string(), secret)),
        E::ConnectionClosed | E::AlreadyClosed | E::Protocol(tungstenite::error::ProtocolError::ResetWithoutClosingHandshake) => {
            err(SttErrorCode::Unreachable, format!("{provider} closed the connection."), "connection closed")
        }
        other => err(SttErrorCode::Other, format!("{provider} streaming failed."), scrub(&other.to_string(), secret)),
    }
}

pub fn is_timeout(e: &std::io::Error) -> bool {
    matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut)
}

pub fn cancelled() -> SttError {
    err(SttErrorCode::Cancelled, "Dictation cancelled.", "")
}

pub fn not_configured(provider: &str) -> SttError {
    err(SttErrorCode::NotConfigured, format!("Add your {provider} API key in Settings."), "")
}

/// RFC 3986 percent-encoding of a query component (unreserved characters pass through).
pub fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// `None` for unset/"auto"/blank, otherwise the trimmed code.
pub fn explicit_language(lang: Option<&str>) -> Option<&str> {
    lang.map(str::trim).filter(|l| !l.is_empty() && !l.eq_ignore_ascii_case("auto"))
}

/// Two-letter lowercase base code (`pt-BR` -> `pt`).
pub fn base_language(lang: &str) -> String {
    lang.split(['-', '_']).next().unwrap_or(lang).to_ascii_lowercase()
}

/// Trimmed, de-duplicated (case-insensitive) hint words, at most `max`.
pub fn clean_hints(hints: &[String], max: usize) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for h in hints {
        let t = h.trim();
        if t.is_empty() || !seen.insert(t.to_lowercase()) {
            continue;
        }
        out.push(t.to_string());
        if out.len() == max {
            break;
        }
    }
    out
}

/// Linear 16 kHz -> 24 kHz upsample (ratio 3:2), the same interpolation as OpenWhispr.
pub fn upsample_16k_to_24k(src: &[i16]) -> Vec<i16> {
    if src.is_empty() {
        return Vec::new();
    }
    let n_out = src.len() * 3 / 2;
    let last = src.len() - 1;
    let mut out = Vec::with_capacity(n_out);
    for i in 0..n_out {
        // position = i / 1.5 = 2i/3, kept in integers: whole index and a 0/3, 1/3, 2/3 fraction.
        let low = (2 * i) / 3;
        let frac = ((2 * i) % 3) as f64 / 3.0;
        let a = src[low.min(last)] as f64;
        let b = src[(low + 1).min(last)] as f64;
        out.push((a + (b - a) * frac).round() as i16);
    }
    out
}

pub fn pcm_to_le_bytes(pcm: &[i16]) -> Vec<u8> {
    let mut v = Vec::with_capacity(pcm.len() * 2);
    for s in pcm {
        v.extend_from_slice(&s.to_le_bytes());
    }
    v
}

/// Normalised RMS (0..1) of a PCM frame.
pub fn rms(pcm: &[i16]) -> f32 {
    if pcm.is_empty() {
        return 0.0;
    }
    let sum: f64 = pcm.iter().map(|&s| (s as f64 / 32768.0).powi(2)).sum();
    (sum / pcm.len() as f64).sqrt() as f32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_encoding() {
        assert_eq!(percent_encode("a b&c=d/é"), "a%20b%26c%3Dd%2F%C3%A9");
        assert_eq!(percent_encode("Az09-_.~"), "Az09-_.~");
    }

    #[test]
    fn upsample_matches_linear_interpolation() {
        let src = [0i16, 300, 600, 900];
        let out = upsample_16k_to_24k(&src);
        assert_eq!(out.len(), 6);
        // positions 0, 2/3, 4/3, 2, 8/3, 10/3 (clamped to the last sample)
        assert_eq!(out, vec![0, 200, 400, 600, 800, 900]);
        assert!(upsample_16k_to_24k(&[]).is_empty());
        assert_eq!(upsample_16k_to_24k(&[1000i16; 800]).len(), 1200);
    }

    #[test]
    fn hints_dedupe_and_cap() {
        let h: Vec<String> = ["Rust", " rust ", "", "Tungstenite", "Deepgram"].iter().map(|s| s.to_string()).collect();
        assert_eq!(clean_hints(&h, 2), vec!["Rust", "Tungstenite"]);
    }

    #[test]
    fn scrub_hides_secret_and_truncates() {
        let s = scrub(&format!("bad key sk-secret {}", "x".repeat(400)), "sk-secret");
        assert!(!s.contains("sk-secret") && s.len() <= 300);
    }

    #[test]
    fn languages() {
        assert_eq!(explicit_language(Some("auto")), None);
        assert_eq!(explicit_language(Some(" ")), None);
        assert_eq!(explicit_language(Some("pt-BR")), Some("pt-BR"));
        assert_eq!(base_language("pt-BR"), "pt");
    }
}
