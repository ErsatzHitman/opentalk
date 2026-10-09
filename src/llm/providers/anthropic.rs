//! Anthropic Messages API: `POST {base}/messages`.
use super::{Built, Req, embedded_error, send};
use crate::llm::LlmError;
use serde_json::{Value, json};

pub const ANTHROPIC_VERSION: &str = "2023-06-01";

/// Models that reject a `temperature` parameter: Opus 4.7 and later, Sonnet 5+, Fable/Mythos-class.
pub fn rejects_temperature(model: &str) -> bool {
    let m = super::bare_model(model);
    if m.contains("fable") || m.contains("mythos") {
        return true;
    }
    for (family, first_rejecting_major) in [("opus", 5u32), ("sonnet", 5), ("haiku", 6)] {
        if let Some(rest) = m.split(&format!("{family}-")).nth(1) {
            let mut nums = rest.split(['-', '.']).map(|p| p.parse::<u32>());
            // A date stamp (`claude-3-5-sonnet-20241022`) is not a version.
            let major = nums.next().and_then(Result::ok).filter(|n| *n < 100);
            let minor = nums.next().and_then(Result::ok).filter(|n| *n < 100).unwrap_or(0);
            if let Some(major) = major {
                if major >= first_rejecting_major {
                    return true;
                }
                // Opus 4.7 / 4.8 reject it too (minor >= 7 within major 4).
                if family == "opus" && major == 4 && minor >= 7 {
                    return true;
                }
            }
        }
        // `claude-4-opus` style ids are legacy (accept temperature).
    }
    false
}

pub fn build(req: &Req) -> Built {
    let mut body = json!({
        "model": req.model,
        "max_tokens": req.max_tokens,
        "system": req.system,
        "messages": [ { "role": "user", "content": req.user } ],
    });
    if !rejects_temperature(req.model) {
        body["temperature"] = json!(0);
    }
    Built {
        url: format!("{}/messages", req.base_url),
        headers: vec![
            ("x-api-key", req.key.to_string()),
            ("anthropic-version", ANTHROPIC_VERSION.to_string()),
        ],
        body,
    }
}

pub fn parse(v: &Value, key: &str) -> Result<String, LlmError> {
    if v.get("type").and_then(Value::as_str) == Some("error") {
        return Err(embedded_error(v, key).unwrap_or(LlmError::InvalidOutput("provider error".into())));
    }
    let mut text = String::new();
    for block in v.get("content").and_then(Value::as_array).into_iter().flatten() {
        if block.get("type").and_then(Value::as_str) == Some("text") {
            if let Some(t) = block.get("text").and_then(Value::as_str) {
                text.push_str(t);
            }
        }
    }
    if text.trim().is_empty() {
        let why = v.get("stop_reason").and_then(Value::as_str).unwrap_or("no text in response");
        return Err(LlmError::InvalidOutput(format!("empty completion ({why})")));
    }
    Ok(text)
}

pub fn complete(req: &Req) -> Result<String, LlmError> {
    let v = send(build(req), req)?;
    parse(&v, req.key)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::config::LlmProviderId;
    use crate::llm::providers::testutil::serve_once;
    use std::time::Duration;

    fn req<'a>(base: &'a str, model: &'a str) -> Req<'a> {
        Req {
            provider: LlmProviderId::Anthropic,
            base_url: base,
            model,
            key: "sk-ant-test-12345",
            system: "SYS",
            user: "USR",
            max_tokens: 256,
            timeout: Duration::from_secs(5),
        }
    }

    #[test]
    fn temperature_rule() {
        for m in ["claude-haiku-4-5", "claude-sonnet-4-6", "claude-opus-4-6", "claude-opus-4-5", "claude-sonnet-4-5", "anthropic.claude-haiku-4-5", "claude-3-5-sonnet-20241022", "claude-opus-4-1-20250805", "claude-sonnet-4-5-20250929"] {
            assert!(!rejects_temperature(m), "{m} accepts temperature");
        }
        for m in ["claude-opus-4-7", "claude-opus-4-8", "claude-opus-5", "claude-sonnet-5", "claude-fable-5", "claude-fable-5-1", "claude-sonnet-5-1"] {
            assert!(rejects_temperature(m), "{m} rejects temperature");
        }
    }

    #[test]
    fn request_shape() {
        let b = build(&req("https://api.anthropic.com/v1", "claude-haiku-4-5"));
        assert_eq!(b.url, "https://api.anthropic.com/v1/messages");
        assert_eq!(b.body["system"], "SYS");
        assert_eq!(b.body["messages"], json!([{"role":"user","content":"USR"}]));
        assert_eq!(b.body["temperature"], 0);
        assert_eq!(b.body["max_tokens"], 256);
        let b = build(&req("https://x", "claude-sonnet-5"));
        assert!(b.body.get("temperature").is_none());
    }

    #[test]
    fn parses_text_blocks() {
        let v = json!({"content":[{"type":"thinking","thinking":"..."},{"type":"text","text":"Hello."},{"type":"text","text":" World."}]});
        assert_eq!(parse(&v, "k").unwrap(), "Hello. World.");
        assert!(matches!(parse(&json!({"content":[],"stop_reason":"max_tokens"}), "k"), Err(LlmError::InvalidOutput(_))));
        assert!(matches!(parse(&json!({"type":"error","error":{"type":"overloaded_error","message":"x"}}), "k"), Err(LlmError::Http { .. })));
    }

    #[test]
    fn mock_server_roundtrip() {
        let (base, rx) = serve_once(200, r#"{"content":[{"type":"text","text":"Clean."}],"stop_reason":"end_turn"}"#, Duration::ZERO);
        let url = format!("{base}/v1");
        assert_eq!(complete(&req(&url, "claude-haiku-4-5")).unwrap(), "Clean.");
        let c = rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(c.path(), "/v1/messages");
        assert_eq!(c.header("x-api-key"), Some("sk-ant-test-12345"));
        assert_eq!(c.header("anthropic-version"), Some("2023-06-01"));
        assert!(c.header("authorization").is_none());
        let b = c.json();
        assert_eq!(b["system"], "SYS");
        assert_eq!(b["temperature"], 0);
    }
}
