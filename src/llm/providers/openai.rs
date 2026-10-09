//! OpenAI Responses API: `POST {base}/responses`.
use super::{Built, Req, bare_model, embedded_error, gpt_major, openai_rejects_temperature, send};
use crate::llm::LlmError;
use serde_json::{Value, json};

/// Reasoning effort for the model, if it should be set. The original gpt-5 family (gpt-5,
/// gpt-5-mini, gpt-5-nano) accepts `minimal`; later gpt-5.x and gpt-6+ models dropped it, so they
/// get `low`. o-series and non-reasoning models send none.
pub fn reasoning_effort(model: &str) -> Option<&'static str> {
    let m = bare_model(model);
    if m == "gpt-5" || m.starts_with("gpt-5-") {
        Some("minimal")
    } else if gpt_major(&m).is_some_and(|v| v >= 5) {
        Some("low")
    } else {
        None
    }
}

/// Does this model spend output tokens on hidden reasoning?
pub fn is_reasoning(model: &str) -> bool {
    openai_rejects_temperature(model)
}

pub fn build(req: &Req) -> Built {
    let mut body = json!({
        "model": req.model,
        "input": [
            { "role": "system", "content": req.system },
            { "role": "user", "content": req.user },
        ],
        "max_output_tokens": req.max_tokens,
        "store": false,
    });
    if !openai_rejects_temperature(req.model) {
        body["temperature"] = json!(0);
    }
    if let Some(effort) = reasoning_effort(req.model) {
        body["reasoning"] = json!({ "effort": effort });
    }
    Built {
        url: format!("{}/responses", req.base_url),
        headers: vec![("Authorization", format!("Bearer {}", req.key))],
        body,
    }
}

pub fn parse(v: &Value, key: &str) -> Result<String, LlmError> {
    if let Some(e) = embedded_error(v, key) {
        return Err(e);
    }
    if let Some(t) = v.get("output_text").and_then(Value::as_str).filter(|t| !t.trim().is_empty()) {
        return Ok(t.to_string());
    }
    let mut text = String::new();
    for item in v.get("output").and_then(Value::as_array).into_iter().flatten() {
        if item.get("type").and_then(Value::as_str).is_some_and(|t| t != "message") {
            continue;
        }
        for part in item.get("content").and_then(Value::as_array).into_iter().flatten() {
            if let Some(t) = part.get("text").and_then(Value::as_str) {
                text.push_str(t);
            }
        }
    }
    if text.trim().is_empty() {
        let why = v.pointer("/incomplete_details/reason").and_then(Value::as_str).unwrap_or("no text in response");
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
            provider: LlmProviderId::Openai,
            base_url: base,
            model,
            key: "sk-test-key-12345",
            system: "SYS",
            user: "USR",
            max_tokens: 300,
            timeout: Duration::from_secs(5),
        }
    }

    #[test]
    fn request_shape_gpt4_has_temperature_no_reasoning() {
        let b = build(&req("https://x/v1", "gpt-4.1-nano"));
        assert_eq!(b.url, "https://x/v1/responses");
        assert_eq!(b.body["model"], "gpt-4.1-nano");
        assert_eq!(b.body["input"][0], json!({"role":"system","content":"SYS"}));
        assert_eq!(b.body["input"][1], json!({"role":"user","content":"USR"}));
        assert_eq!(b.body["max_output_tokens"], 300);
        assert_eq!(b.body["temperature"], 0);
        assert!(b.body.get("reasoning").is_none());
    }

    #[test]
    fn gpt5_omits_temperature_and_sets_effort() {
        for (m, effort) in [("gpt-5-nano", Some("minimal")), ("gpt-5", Some("minimal")), ("gpt-5.2", Some("low")), ("gpt-5.6-luna", Some("low")), ("gpt-6-astra", Some("low")), ("o3-mini", None), ("gpt-4.1", None)] {
            let b = build(&req("https://x/v1", m));
            assert_eq!(openai_rejects_temperature(m), m != "gpt-4.1", "{m}");
            assert_eq!(is_reasoning(m), m != "gpt-4.1", "{m}");
            assert_eq!(b.body.get("temperature").is_none(), m != "gpt-4.1", "{m}");
            assert_eq!(b.body.pointer("/reasoning/effort").and_then(Value::as_str), effort, "{m}");
        }
    }

    #[test]
    fn parses_output_text_and_nested_output() {
        assert_eq!(parse(&json!({"output_text":"Hello."}), "k").unwrap(), "Hello.");
        let nested = json!({"output":[
            {"type":"reasoning","summary":[]},
            {"type":"message","content":[{"type":"output_text","text":"Hi"},{"type":"output_text","text":" there."}]}
        ]});
        assert_eq!(parse(&nested, "k").unwrap(), "Hi there.");
        let incomplete = json!({"status":"incomplete","incomplete_details":{"reason":"max_output_tokens"},"output":[{"type":"reasoning"}]});
        assert!(matches!(parse(&incomplete, "k"), Err(LlmError::InvalidOutput(m)) if m.contains("max_output_tokens")));
        let err = json!({"error":{"message":"bad","code":429}});
        assert!(matches!(parse(&err, "k"), Err(LlmError::Http { status: 429, .. })));
    }

    #[test]
    fn mock_server_roundtrip() {
        let (base, rx) = serve_once(200, r#"{"output":[{"type":"message","content":[{"type":"output_text","text":"Clean."}]}]}"#, Duration::ZERO);
        let url = format!("{base}/v1");
        let out = complete(&req(&url, "gpt-5-nano")).unwrap();
        assert_eq!(out, "Clean.");
        let c = rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(c.path(), "/v1/responses");
        assert_eq!(c.header("authorization"), Some("Bearer sk-test-key-12345"));
        assert_eq!(c.header("content-type"), Some("application/json"));
        let b = c.json();
        assert_eq!(b["model"], "gpt-5-nano");
        assert_eq!(b["reasoning"]["effort"], "minimal");
        assert!(b.get("temperature").is_none());
    }

    #[test]
    fn http_error_is_classified_and_redacted() {
        let (base, _rx) = serve_once(401, r#"{"error":{"message":"Incorrect API key provided: sk-test-key-12345."}}"#, Duration::ZERO);
        let url = format!("{base}/v1");
        match complete(&req(&url, "gpt-4.1")) {
            Err(LlmError::Http { status: 401, detail }) => {
                assert!(detail.contains("Incorrect API key"));
                assert!(!detail.contains("sk-test-key-12345"), "{detail}");
            }
            other => panic!("{other:?}"),
        }
    }
}
