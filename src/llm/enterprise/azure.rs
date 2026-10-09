//! Azure OpenAI chat completions:
//! `POST {endpoint}/openai/deployments/{deployment}/chat/completions?api-version={ver}`
//! with the resource key in the `api-key` header (secret `azure_openai`).
use super::{Deadline, http_error, net_err, secret, uri_encode};
use crate::core::config::Config;
use crate::core::http;
use crate::llm::LlmError;
use serde_json::{Value, json};

pub(super) fn complete(cfg: &Config, system: &str, user: &str, deadline: &Deadline, base: Option<&str>) -> Result<String, LlmError> {
    let ai = &cfg.ai_cleanup;
    let endpoint = base.unwrap_or_else(|| ai.azure_endpoint.trim().trim_end_matches('/'));
    let deployment = if ai.azure_deployment.trim().is_empty() { cfg.llm_model() } else { ai.azure_deployment.trim().to_string() };
    let version = if ai.azure_api_version.trim().is_empty() { "2024-10-21" } else { ai.azure_api_version.trim() };
    if endpoint.is_empty() || deployment.is_empty() {
        return Err(LlmError::NotConfigured);
    }
    let key = secret("azure_openai").ok_or(LlmError::NotConfigured)?;
    run(endpoint, &deployment, version, &key, system, user, deadline)
}

/// Reasoning-family deployments (gpt-5*, o1/o3/o4...) reject a non-default `temperature`.
fn accepts_temperature(deployment: &str) -> bool {
    let d = deployment.to_ascii_lowercase();
    let o_series = d.strip_prefix('o').is_some_and(|r| r.starts_with(|c: char| c.is_ascii_digit()));
    !(d.starts_with("gpt-5") || o_series)
}

fn run(endpoint: &str, deployment: &str, version: &str, key: &str, system: &str, user: &str, deadline: &Deadline) -> Result<String, LlmError> {
    let url = format!(
        "{endpoint}/openai/deployments/{}/chat/completions?api-version={}",
        uri_encode(deployment),
        uri_encode(version)
    );
    let mut body = json!({ "messages": [
        { "role": "system", "content": system },
        { "role": "user", "content": user },
    ]});
    if accepts_temperature(deployment) {
        body["temperature"] = json!(0);
    }
    let resp = http::post_json(&url, &[("api-key", key)], &body, deadline.remaining()?).map_err(net_err)?;
    if !resp.ok() {
        return Err(http_error(resp.status, &resp.body));
    }
    let v = resp.json().map_err(|e| LlmError::InvalidOutput(format!("bad JSON: {e}")))?;
    extract(&v)
}

/// `choices[0].message.content` (string, or an array of text parts).
fn extract(v: &Value) -> Result<String, LlmError> {
    let content = v.pointer("/choices/0/message/content");
    let text = match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => parts.iter().filter_map(|p| p.get("text").and_then(Value::as_str)).collect::<String>(),
        _ => String::new(),
    };
    if text.trim().is_empty() {
        return Err(LlmError::InvalidOutput("empty completion".into()));
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::super::testutil::{cfg, serve};
    use super::super::complete_with;
    use super::*;
    use crate::core::config::LlmProviderId;

    const OK: &str = r#"{"choices":[{"message":{"role":"assistant","content":"Hello, world."}}]}"#;

    #[test]
    fn request_shape_and_response() {
        let mock = serve(|_| (200, OK.into()));
        let deadline = Deadline::new(5000);
        let out = run(&mock.base, "my dep", "2024-10-21", "KEY123", "SYS", "USR", &deadline).unwrap();
        assert_eq!(out, "Hello, world.");
        let reqs = mock.reqs.lock().unwrap();
        let r = &reqs[0];
        assert_eq!(r.method, "POST");
        assert_eq!(r.target, "/openai/deployments/my%20dep/chat/completions?api-version=2024-10-21");
        assert_eq!(r.header("api-key"), Some("KEY123"));
        assert!(r.header("authorization").is_none());
        let b: Value = serde_json::from_str(&r.body).unwrap();
        assert_eq!(b["messages"][0], json!({"role":"system","content":"SYS"}));
        assert_eq!(b["messages"][1], json!({"role":"user","content":"USR"}));
        assert_eq!(b["temperature"], json!(0));
    }

    #[test]
    fn reasoning_deployments_omit_temperature() {
        assert!(accepts_temperature("gpt-4.1-mini"));
        assert!(accepts_temperature("cleanup"));
        assert!(!accepts_temperature("gpt-5-mini"));
        assert!(!accepts_temperature("o3-mini"));
        assert!(!accepts_temperature("O4"));
        assert!(accepts_temperature("omni-prod"));
    }

    #[test]
    fn errors_map() {
        let mock = serve(|_| (401, r#"{"error":{"code":"401","message":"Access denied"}}"#.into()));
        let e = run(&mock.base, "d", "v", "bad", "s", "u", &Deadline::new(5000)).unwrap_err();
        assert_eq!(e, LlmError::Http { status: 401, detail: "Access denied".into() });

        let mock = serve(|_| (200, r#"{"choices":[{"message":{"content":""}}]}"#.into()));
        assert!(matches!(run(&mock.base, "d", "v", "k", "s", "u", &Deadline::new(5000)), Err(LlmError::InvalidOutput(_))));
    }

    #[test]
    fn content_parts_are_joined() {
        let v: Value = serde_json::from_str(r#"{"choices":[{"message":{"content":[{"type":"text","text":"a"},{"type":"text","text":"b"}]}}]}"#).unwrap();
        assert_eq!(extract(&v).unwrap(), "ab");
    }

    #[test]
    fn complete_uses_config_and_override() {
        // Key comes from the env override; the variable is unique to this test's provider.
        // SAFETY: only this test touches OPENTALK_SECRET_AZURE_OPENAI.
        unsafe { std::env::set_var("OPENTALK_SECRET_AZURE_OPENAI", "ENVKEY") };
        let mock = serve(|_| (200, OK.into()));
        let mut c = cfg(LlmProviderId::AzureOpenai);
        assert_eq!(complete_with(&c, "s", "u", 5000, Some(&mock.base)), Err(LlmError::NotConfigured)); // no deployment
        c.ai_cleanup.azure_endpoint = "https://unused.openai.azure.com/".into();
        c.ai_cleanup.azure_deployment = "dep1".into();
        c.ai_cleanup.azure_api_version = "2025-01-01".into();
        assert_eq!(complete_with(&c, "s", "u", 5000, Some(&mock.base)).unwrap(), "Hello, world.");
        let reqs = mock.reqs.lock().unwrap();
        assert_eq!(reqs[0].target, "/openai/deployments/dep1/chat/completions?api-version=2025-01-01");
        assert_eq!(reqs[0].header("api-key"), Some("ENVKEY"));
    }

    #[test]
    fn missing_endpoint_is_not_configured() {
        let c = cfg(LlmProviderId::AzureOpenai);
        assert_eq!(complete_with(&c, "s", "u", 1000, None), Err(LlmError::NotConfigured));
    }
}
