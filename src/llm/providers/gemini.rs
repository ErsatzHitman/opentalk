//! Gemini `generateContent`: `POST {base}/models/{model}:generateContent` (key in `x-goog-api-key`).
use super::{Built, Req, bare_model, embedded_error, send};
use crate::llm::LlmError;
use serde_json::{Value, json};

/// Thinking config that keeps latency low, for models known to accept one.
/// 2.5 Flash / Flash-Lite can switch thinking off (`thinkingBudget: 0`); Gemini 3 takes a level
/// (`low` is accepted by every 3.x model, Pro included). Others (Gemma, 2.5 Pro) get none.
pub fn thinking_config(model: &str) -> Option<Value> {
    let m = bare_model(model);
    let m = m.strip_prefix("models/").unwrap_or(&m);
    if m.starts_with("gemini-2.5-flash") {
        Some(json!({ "thinkingBudget": 0 }))
    } else if m.starts_with("gemini-3") {
        Some(json!({ "thinkingLevel": "low" }))
    } else {
        None
    }
}

pub fn build(req: &Req) -> Built {
    let model = req.model.strip_prefix("models/").unwrap_or(req.model);
    let mut gen_cfg = json!({ "temperature": 0, "maxOutputTokens": req.max_tokens });
    if let Some(t) = thinking_config(model) {
        gen_cfg["thinkingConfig"] = t;
    }
    let body = json!({
        "systemInstruction": { "parts": [ { "text": req.system } ] },
        "contents": [ { "role": "user", "parts": [ { "text": req.user } ] } ],
        "generationConfig": gen_cfg,
    });
    Built {
        url: format!("{}/models/{}:generateContent", req.base_url, model),
        headers: vec![("x-goog-api-key", req.key.to_string())],
        body,
    }
}

pub fn parse(v: &Value, key: &str) -> Result<String, LlmError> {
    if let Some(e) = embedded_error(v, key) {
        return Err(e);
    }
    if let Some(reason) = v.pointer("/promptFeedback/blockReason").and_then(Value::as_str) {
        return Err(LlmError::InvalidOutput(format!("prompt blocked ({reason})")));
    }
    let cand = v.pointer("/candidates/0");
    let mut text = String::new();
    for part in cand.and_then(|c| c.pointer("/content/parts")).and_then(Value::as_array).into_iter().flatten() {
        if part.get("thought").and_then(Value::as_bool) == Some(true) {
            continue;
        }
        if let Some(t) = part.get("text").and_then(Value::as_str) {
            text.push_str(t);
        }
    }
    if text.trim().is_empty() {
        let why = cand.and_then(|c| c.get("finishReason")).and_then(Value::as_str).unwrap_or("no text in response");
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
            provider: LlmProviderId::Gemini,
            base_url: base,
            model,
            key: "AIza-test-key-123",
            system: "SYS",
            user: "USR",
            max_tokens: 300,
            timeout: Duration::from_secs(5),
        }
    }

    #[test]
    fn request_shape() {
        let b = build(&req("https://g/v1beta", "gemini-3.5-flash-lite"));
        assert_eq!(b.url, "https://g/v1beta/models/gemini-3.5-flash-lite:generateContent");
        assert_eq!(b.body["systemInstruction"], json!({"parts":[{"text":"SYS"}]}));
        assert_eq!(b.body["contents"], json!([{"role":"user","parts":[{"text":"USR"}]}]));
        assert_eq!(b.body["generationConfig"]["temperature"], 0);
        assert_eq!(b.body["generationConfig"]["maxOutputTokens"], 300);
        assert_eq!(b.body["generationConfig"]["thinkingConfig"], json!({"thinkingLevel":"low"}));
        let b = build(&req("https://g", "models/gemini-2.5-flash"));
        assert!(b.url.ends_with("/models/gemini-2.5-flash:generateContent"));
        assert_eq!(b.body["generationConfig"]["thinkingConfig"], json!({"thinkingBudget":0}));
        let b = build(&req("https://g", "gemma-4-31b-it"));
        assert!(b.body["generationConfig"].get("thinkingConfig").is_none());
    }

    #[test]
    fn parses_candidates_skipping_thoughts() {
        let v = json!({"candidates":[{"content":{"parts":[{"text":"hmm","thought":true},{"text":"Hello."}]},"finishReason":"STOP"}]});
        assert_eq!(parse(&v, "k").unwrap(), "Hello.");
        let blocked = json!({"promptFeedback":{"blockReason":"SAFETY"}});
        assert!(matches!(parse(&blocked, "k"), Err(LlmError::InvalidOutput(m)) if m.contains("SAFETY")));
        let empty = json!({"candidates":[{"finishReason":"MAX_TOKENS"}]});
        assert!(matches!(parse(&empty, "k"), Err(LlmError::InvalidOutput(m)) if m.contains("MAX_TOKENS")));
    }

    #[test]
    fn mock_server_roundtrip() {
        let (base, rx) = serve_once(200, r#"{"candidates":[{"content":{"parts":[{"text":"Clean."}]}}]}"#, Duration::ZERO);
        let url = format!("{base}/v1beta");
        assert_eq!(complete(&req(&url, "gemini-3.5-flash")).unwrap(), "Clean.");
        let c = rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(c.path(), "/v1beta/models/gemini-3.5-flash:generateContent");
        assert_eq!(c.header("x-goog-api-key"), Some("AIza-test-key-123"));
        assert!(!c.request_line.contains("AIza"), "key must not be in the URL");
        assert_eq!(c.json()["systemInstruction"]["parts"][0]["text"], "SYS");
    }
}
