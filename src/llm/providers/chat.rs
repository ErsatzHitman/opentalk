//! OpenAI-compatible Chat Completions (Groq, OpenRouter, Custom/LAN): `POST {base}/chat/completions`.
use super::{Built, Req, embedded_error, openai_rejects_temperature, send};
use crate::core::config::LlmProviderId;
use crate::llm::LlmError;
use serde_json::{Value, json};

pub const OPENROUTER_REFERER: &str = "https://github.com/opentalk";
pub const OPENROUTER_TITLE: &str = "OpenTalk";

/// Chat-completions endpoint for a base URL (a base already ending in `/chat/completions` is kept).
pub fn endpoint(base_url: &str) -> String {
    let b = base_url.trim_end_matches('/');
    if b.ends_with("/chat/completions") { b.to_string() } else { format!("{b}/chat/completions") }
}

pub fn build(req: &Req) -> Built {
    let mut body = json!({
        "model": req.model,
        "messages": [
            { "role": "system", "content": req.system },
            { "role": "user", "content": req.user },
        ],
        "max_tokens": req.max_tokens,
    });
    if !openai_rejects_temperature(req.model) {
        body["temperature"] = json!(0);
    }
    // gpt-oss thinks by default and cannot switch it off; cleanup wants the least it allows.
    if req.model.to_ascii_lowercase().contains("gpt-oss") {
        body["reasoning_effort"] = json!("low");
    } else if req.provider == LlmProviderId::Openrouter {
        // OpenRouter's reasoning toggle. Without it, thinking models (the :free Nemotron, LFM, ...)
        // spend the whole token budget on reasoning or leak it into the answer. Models without
        // reasoning support ignore the field; ones where it is mandatory answer 400 (-> fallback).
        body["reasoning"] = json!({ "enabled": false });
    }
    let mut headers = Vec::new();
    if !req.key.is_empty() {
        headers.push(("Authorization", format!("Bearer {}", req.key)));
    }
    if req.provider == LlmProviderId::Openrouter {
        headers.push(("HTTP-Referer", OPENROUTER_REFERER.to_string()));
        headers.push(("X-Title", OPENROUTER_TITLE.to_string()));
    }
    Built { url: endpoint(req.base_url), headers, body }
}

pub fn parse(v: &Value, key: &str) -> Result<String, LlmError> {
    if v.get("choices").is_none() {
        if let Some(e) = embedded_error(v, key) {
            return Err(e);
        }
    }
    let choice = v.pointer("/choices/0");
    let content = choice.and_then(|c| c.pointer("/message/content"));
    let text = match content {
        Some(Value::String(s)) => s.clone(),
        // Some gateways return content parts.
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|p| p.get("text").and_then(Value::as_str))
            .collect::<String>(),
        _ => String::new(),
    };
    if text.trim().is_empty() {
        let why = choice.and_then(|c| c.get("finish_reason")).and_then(Value::as_str).unwrap_or("no text in response");
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
    use crate::llm::providers::testutil::serve_once;
    use std::time::Duration;

    fn req<'a>(provider: LlmProviderId, base: &'a str, model: &'a str, key: &'a str) -> Req<'a> {
        Req { provider, base_url: base, model, key, system: "SYS", user: "USR", max_tokens: 300, timeout: Duration::from_secs(5) }
    }

    #[test]
    fn endpoint_forms() {
        assert_eq!(endpoint("https://api.groq.com/openai/v1"), "https://api.groq.com/openai/v1/chat/completions");
        assert_eq!(endpoint("http://lan:8080/v1/"), "http://lan:8080/v1/chat/completions");
        assert_eq!(endpoint("http://lan/v1/chat/completions"), "http://lan/v1/chat/completions");
    }

    #[test]
    fn request_shape_groq_and_gpt_oss() {
        let b = build(&req(LlmProviderId::Groq, "https://g/v1", "openai/gpt-oss-20b", "gsk_testkey_123"));
        assert_eq!(b.url, "https://g/v1/chat/completions");
        assert_eq!(b.body["messages"][0], json!({"role":"system","content":"SYS"}));
        assert_eq!(b.body["messages"][1], json!({"role":"user","content":"USR"}));
        assert_eq!(b.body["temperature"], 0);
        assert_eq!(b.body["reasoning_effort"], "low");
        assert_eq!(b.body["max_tokens"], 300);
        assert_eq!(b.headers, vec![("Authorization", "Bearer gsk_testkey_123".to_string())]);
        let b = build(&req(LlmProviderId::Groq, "https://g/v1", "qwen/qwen3.8-27b", "k"));
        assert!(b.body.get("reasoning_effort").is_none());
    }

    #[test]
    fn openrouter_headers_and_reasoning_toggle() {
        let b = build(&req(LlmProviderId::Openrouter, "https://openrouter.ai/api/v1", "nvidia/nemotron-3.5-lightning:free", "sk-or-test-123"));
        assert!(b.headers.contains(&("HTTP-Referer", "https://github.com/opentalk".to_string())));
        assert!(b.headers.contains(&("X-Title", "OpenTalk".to_string())));
        assert_eq!(b.body["temperature"], 0);
        assert!(b.body.get("reasoning_effort").is_none());
        assert_eq!(b.body["reasoning"], json!({"enabled": false}));
        // gpt-oss via OpenRouter gets the effort hint instead; OpenAI reasoning models skip temperature.
        let b = build(&req(LlmProviderId::Openrouter, "https://o", "openai/gpt-oss-120b:free", "k"));
        assert_eq!(b.body["reasoning_effort"], "low");
        assert!(b.body.get("reasoning").is_none());
        // Other chat providers never get the OpenRouter-only field.
        let b = build(&req(LlmProviderId::Groq, "https://g", "qwen/qwen3.8-27b", "k"));
        assert!(b.body.get("reasoning").is_none());
        let b = build(&req(LlmProviderId::Openrouter, "https://o", "openai/gpt-5-mini", "k"));
        assert!(b.body.get("temperature").is_none());
    }

    #[test]
    fn custom_without_key_sends_no_auth_and_no_openrouter_headers() {
        let b = build(&req(LlmProviderId::Custom, "http://lan/v1", "llama", ""));
        assert!(b.headers.is_empty());
    }

    #[test]
    fn parses_string_and_part_content_and_errors() {
        let v = json!({"choices":[{"message":{"content":"Hello."},"finish_reason":"stop"}]});
        assert_eq!(parse(&v, "k").unwrap(), "Hello.");
        let parts = json!({"choices":[{"message":{"content":[{"type":"text","text":"A"},{"type":"text","text":"B"}]}}]});
        assert_eq!(parse(&parts, "k").unwrap(), "AB");
        let empty = json!({"choices":[{"message":{"content":null,"reasoning":"..."},"finish_reason":"length"}]});
        assert!(matches!(parse(&empty, "k"), Err(LlmError::InvalidOutput(m)) if m.contains("length")));
        let err = json!({"error":{"message":"rate limited","code":429}});
        assert!(matches!(parse(&err, "k"), Err(LlmError::Http { status: 429, .. })));
    }

    #[test]
    fn mock_server_roundtrip_openrouter() {
        let (base, rx) = serve_once(200, r#"{"choices":[{"message":{"role":"assistant","content":"Clean."},"finish_reason":"stop"}]}"#, Duration::ZERO);
        let url = format!("{base}/api/v1");
        let r = req(LlmProviderId::Openrouter, &url, "google/gemma-4-31b-it:free", "sk-or-test-123");
        assert_eq!(complete(&r).unwrap(), "Clean.");
        let c = rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(c.path(), "/api/v1/chat/completions");
        assert_eq!(c.header("authorization"), Some("Bearer sk-or-test-123"));
        assert_eq!(c.header("http-referer"), Some("https://github.com/opentalk"));
        assert_eq!(c.header("x-title"), Some("OpenTalk"));
        assert_eq!(c.json()["model"], "google/gemma-4-31b-it:free");
    }

    #[test]
    fn mock_server_429_is_http_error() {
        let (base, _rx) = serve_once(429, r#"{"error":{"message":"Rate limit exceeded: free-models-per-day"}}"#, Duration::ZERO);
        let r = req(LlmProviderId::Groq, &base, "openai/gpt-oss-20b", "gsk_testkey_123");
        match complete(&r) {
            Err(LlmError::Http { status: 429, detail }) => assert!(detail.contains("Rate limit")),
            other => panic!("{other:?}"),
        }
    }
}
