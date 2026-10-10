//! Azure OpenAI chat completions:
//! `POST {endpoint}/openai/deployments/{deployment}/chat/completions?api-version={ver}`
//! with the resource key in the `api-key` header (secret `azure_openai`).
use super::{Deadline, http_error, max_tokens_for, net_err, secret, uri_encode};
use crate::core::config::Config;
use crate::core::http;
use crate::llm::LlmError;
use serde_json::{Value, json};

pub(super) fn complete(cfg: &Config, system: &str, user: &str, deadline: &Deadline, base: Option<&str>) -> Result<String, LlmError> {
    let ai = &cfg.ai_cleanup;
    let endpoint = match base {
        Some(b) => b.to_string(),
        None => normalize_endpoint(&ai.azure_endpoint).ok_or(LlmError::NotConfigured)?,
    };
    let deployment = if ai.azure_deployment.trim().is_empty() { cfg.llm_model() } else { ai.azure_deployment.trim().to_string() };
    let version = if ai.azure_api_version.trim().is_empty() { "2024-10-21" } else { ai.azure_api_version.trim() };
    if deployment.is_empty() {
        return Err(LlmError::NotConfigured);
    }
    let key = secret("azure_openai").ok_or(LlmError::NotConfigured)?;
    // The explicit underlying model, when set, decides the request shape; the deployment name is
    // free text and often says nothing about the model behind it.
    let model = if ai.azure_model.trim().is_empty() { deployment.clone() } else { ai.azure_model.trim().to_string() };
    run(&endpoint, &deployment, &model, version, &key, system, user, deadline)
}

/// `https://<resource>.openai.azure.com` from what the user typed: a bare host gets `https://`, and
/// plain `http://` is refused because the `api-key` header would cross the network in clear text.
fn normalize_endpoint(raw: &str) -> Option<String> {
    let raw = raw.trim();
    let (scheme, rest) = raw.split_once("://").unwrap_or(("https", raw));
    let rest = rest.trim_end_matches('/');
    (scheme.eq_ignore_ascii_case("https") && !rest.is_empty()).then(|| format!("https://{rest}"))
}

/// Reasoning-family deployments (gpt-5 and newer, o1/o3/o4...) reject a non-default `temperature`.
fn accepts_temperature(deployment: &str) -> bool {
    !crate::llm::providers::openai_rejects_temperature(deployment)
}

/// Chat-completions body shaped by the model name (see [`body_for`]).
#[cfg(test)]
fn request_body(model: &str, version: &str, system: &str, user: &str) -> Value {
    body_for(!accepts_temperature(model), model, version, system, user)
}

/// Chat-completions body. Reasoning deployments (gpt-5*, o-series) take `max_completion_tokens`
/// (hidden thinking spends it, so it gets a generous floor) and, on API versions that know the
/// field, the lowest `reasoning_effort` so a cleanup fits the deadline; others get `temperature: 0`.
fn body_for(reasoning: bool, model: &str, version: &str, system: &str, user: &str) -> Value {
    let mut body = json!({ "messages": [
        { "role": "system", "content": system },
        { "role": "user", "content": user },
    ]});
    let budget = max_tokens_for(user);
    if reasoning {
        body["max_completion_tokens"] = json!(budget.max(4096));
        if supports_reasoning_effort(version) {
            if let Some(effort) = crate::llm::providers::openai::reasoning_effort(model) {
                body["reasoning_effort"] = json!(effort);
            }
        }
    } else {
        body["temperature"] = json!(0);
        body["max_tokens"] = json!(budget);
    }
    body
}

/// What a 400 body says about the offending parameter: `error.code`, `error.param`, and the
/// lowercased message (or the whole body when it is not the Azure/OpenAI error JSON).
struct ParamError {
    code: Option<String>,
    param: Option<String>,
    text: String,
}

impl ParamError {
    fn parse(body: &str) -> Self {
        let v: Option<Value> = serde_json::from_str(body).ok();
        let field = |p: &str| v.as_ref().and_then(|v| v.pointer(p)).and_then(Value::as_str).map(str::to_ascii_lowercase).filter(|s| !s.is_empty());
        let message = field("/error/message");
        Self { code: field("/error/code"), param: field("/error/param"), text: message.unwrap_or_else(|| body.to_ascii_lowercase()) }
    }

    /// Does this error blame one of `names`? With a JSON `code`, only `unsupported_parameter` /
    /// `unsupported_value` count, and `param` (when present) must name the field, so e.g.
    /// `invalid_value: "max_tokens is too large"` is not a shape problem. Without a code (older
    /// "Unrecognized request argument supplied: ..." bodies) a substring check decides.
    fn blames(&self, names: &[&str]) -> bool {
        match self.code.as_deref() {
            Some("unsupported_parameter" | "unsupported_value") => match &self.param {
                Some(p) => names.iter().any(|n| p == n),
                None => names.iter().any(|n| self.text.contains(n)),
            },
            Some(_) => false,
            None => names.iter().any(|n| self.text.contains(n)),
        }
    }
}

/// A 400 that says the other body shape is needed: `Some(true)` = switch to the reasoning shape
/// (the deployment rejected `temperature`/`max_tokens`), `Some(false)` = back to the classic shape
/// (it rejected `max_completion_tokens`). `None` for any other failure.
fn shape_switch(status: u16, body: &str, tried_reasoning: bool) -> Option<bool> {
    if status != 400 {
        return None;
    }
    let e = ParamError::parse(body);
    if tried_reasoning {
        e.blames(&["max_completion_tokens"]).then_some(false)
    } else {
        e.blames(&["temperature", "max_tokens"]).then_some(true)
    }
}

/// A 400 on a reasoning-shaped request that rejects `reasoning_effort` (the api-version or model
/// does not know the field although [`supports_reasoning_effort`] guessed it does).
fn rejects_reasoning_effort(status: u16, body: &str) -> bool {
    status == 400 && ParamError::parse(body).blames(&["reasoning_effort"])
}

/// `reasoning_effort` on gpt-5 needs 2025-04-01 or later (dated), or the undated `v1`/`preview`.
fn supports_reasoning_effort(version: &str) -> bool {
    match version.get(..10) {
        Some(date) if date.as_bytes()[0].is_ascii_digit() => date >= "2025-04-01",
        _ => true,
    }
}

#[allow(clippy::too_many_arguments)]
fn run(endpoint: &str, deployment: &str, model: &str, version: &str, key: &str, system: &str, user: &str, deadline: &Deadline) -> Result<String, LlmError> {
    let url = format!(
        "{endpoint}/openai/deployments/{}/chat/completions?api-version={}",
        uri_encode(deployment),
        uri_encode(version)
    );
    let mut reasoning = !accepts_temperature(model);
    let mut drop_effort = false;
    let mut retried = false;
    loop {
        let mut body = body_for(reasoning, model, version, system, user);
        if drop_effort {
            if let Some(o) = body.as_object_mut() {
                o.remove("reasoning_effort");
            }
        }
        let resp = http::post_json(&url, &[("api-key", key)], &body, deadline.remaining()?).map_err(net_err)?;
        if resp.ok() {
            let v = resp.json().map_err(|e| LlmError::InvalidOutput(format!("bad JSON: {e}")))?;
            return extract(&v);
        }
        // At most one retry (two requests in all), inside the same deadline (`remaining()` above
        // bounds it): the other body shape, or the same reasoning body without `reasoning_effort`.
        if !retried {
            if let Some(next) = shape_switch(resp.status, &resp.body, reasoning) {
                retried = true;
                reasoning = next;
                continue;
            }
            if reasoning && body.get("reasoning_effort").is_some() && rejects_reasoning_effort(resp.status, &resp.body) {
                retried = true;
                drop_effort = true;
                continue;
            }
        }
        return Err(http_error(resp.status, &resp.body));
    }
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
        let out = run(&mock.base, "my dep", "my dep", "2024-10-21", "KEY123", "SYS", "USR", &deadline).unwrap();
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
    fn reasoning_body_uses_completion_budget_and_effort() {
        let b = request_body("gpt-5-mini", "2025-04-01-preview", "s", "u");
        assert!(b.get("temperature").is_none() && b.get("max_tokens").is_none());
        assert_eq!(b["max_completion_tokens"], json!(4096));
        assert_eq!(b["reasoning_effort"], json!("minimal"));
        // Older api-versions would reject the field; the v1 API accepts it.
        assert!(request_body("gpt-5-mini", "2024-10-21", "s", "u").get("reasoning_effort").is_none());
        assert_eq!(request_body("gpt-5-mini", "preview", "s", "u")["reasoning_effort"], json!("minimal"));
        assert_eq!(request_body("gpt-5.5", "v1", "s", "u")["reasoning_effort"], json!("low"));
        // o-series: completion budget, no effort sent.
        let o = request_body("o3-mini", "2025-04-01-preview", "s", "u");
        assert!(o.get("reasoning_effort").is_none());
        assert_eq!(o["max_completion_tokens"], json!(4096));
        let n = request_body("gpt-4.1-mini", "2024-10-21", "s", "u");
        assert_eq!(n["max_tokens"], json!(256));
        assert_eq!(n["temperature"], json!(0));
        assert!(n.get("max_completion_tokens").is_none());
    }

    const UNSUPPORTED_TEMP: &str = r#"{"error":{"code":"unsupported_parameter","message":"Unsupported parameter: 'max_tokens' is not supported with this model. Use 'max_completion_tokens' instead."}}"#;

    /// Mock deployment that behaves like gpt-5: rejects `temperature`/`max_tokens` with a 400.
    fn reasoning_backend() -> super::super::testutil::Mock {
        serve(|r| {
            let b: Value = serde_json::from_str(&r.body).unwrap();
            if b.get("temperature").is_some() || b.get("max_tokens").is_some() { (400, UNSUPPORTED_TEMP.into()) } else { (200, OK.into()) }
        })
    }

    #[test]
    fn unsupported_parameter_400_retries_with_reasoning_body() {
        let mock = reasoning_backend();
        // Deployment "cleanup" gives no hint, so the first body is the classic shape.
        let out = run(&mock.base, "cleanup", "cleanup", "2024-10-21", "K", "s", "u", &Deadline::new(5000)).unwrap();
        assert_eq!(out, "Hello, world.");
        let reqs = mock.reqs.lock().unwrap();
        assert_eq!(reqs.len(), 2);
        let first: Value = serde_json::from_str(&reqs[0].body).unwrap();
        let second: Value = serde_json::from_str(&reqs[1].body).unwrap();
        assert_eq!(first["temperature"], json!(0));
        assert!(second.get("temperature").is_none() && second.get("max_tokens").is_none());
        assert_eq!(second["max_completion_tokens"], json!(4096));
    }

    #[test]
    fn retry_happens_once_and_only_for_shape_errors() {
        // A persistent 400 about temperature is retried exactly once, then reported.
        let mock = serve(|_| (400, UNSUPPORTED_TEMP.into()));
        let e = run(&mock.base, "cleanup", "cleanup", "2024-10-21", "K", "s", "u", &Deadline::new(5000)).unwrap_err();
        assert!(matches!(e, LlmError::Http { status: 400, .. }));
        assert_eq!(mock.reqs.lock().unwrap().len(), 2);
        // An unrelated 400 is not retried.
        let mock = serve(|_| (400, r#"{"error":{"message":"content filter"}}"#.into()));
        assert!(run(&mock.base, "cleanup", "cleanup", "2024-10-21", "K", "s", "u", &Deadline::new(5000)).is_err());
        assert_eq!(mock.reqs.lock().unwrap().len(), 1);
        // Neither is another status.
        let mock = serve(|_| (500, "temperature".into()));
        assert!(run(&mock.base, "cleanup", "cleanup", "2024-10-21", "K", "s", "u", &Deadline::new(5000)).is_err());
        assert_eq!(mock.reqs.lock().unwrap().len(), 1);
    }

    #[test]
    fn reasoning_shape_rejected_falls_back_to_classic_once() {
        let mock = serve(|r| {
            let b: Value = serde_json::from_str(&r.body).unwrap();
            if b.get("max_completion_tokens").is_some() {
                (400, r#"{"error":{"message":"Unrecognized request argument supplied: max_completion_tokens"}}"#.into())
            } else {
                (200, OK.into())
            }
        });
        // "o-series"-looking name on a deployment that is really a classic model.
        assert_eq!(run(&mock.base, "o3-proxy", "o3-proxy", "2024-10-21", "K", "s", "u", &Deadline::new(5000)).unwrap(), "Hello, world.");
        assert_eq!(mock.reqs.lock().unwrap().len(), 2);
    }

    #[test]
    fn rejected_reasoning_effort_is_dropped_once() {
        let mock = serve(|r| {
            let b: Value = serde_json::from_str(&r.body).unwrap();
            if b.get("reasoning_effort").is_some() {
                (400, r#"{"error":{"message":"Unrecognized request argument supplied: reasoning_effort"}}"#.into())
            } else {
                (200, OK.into())
            }
        });
        // New enough api-version that the effort is sent on the first try.
        let out = run(&mock.base, "gpt-5-mini", "gpt-5-mini", "2025-04-01-preview", "K", "s", "u", &Deadline::new(5000)).unwrap();
        assert_eq!(out, "Hello, world.");
        let reqs = mock.reqs.lock().unwrap();
        assert!(reqs.len() <= 2, "{} requests", reqs.len());
        assert_eq!(reqs.len(), 2);
        let first: Value = serde_json::from_str(&reqs[0].body).unwrap();
        let second: Value = serde_json::from_str(&reqs[1].body).unwrap();
        assert_eq!(first["reasoning_effort"], json!("minimal"));
        assert!(second.get("reasoning_effort").is_none());
        assert_eq!(second["max_completion_tokens"], json!(4096));
        assert!(second.get("temperature").is_none() && second.get("max_tokens").is_none());
    }

    #[test]
    fn persistent_reasoning_effort_400_stops_after_two_requests() {
        let mock = serve(|_| (400, r#"{"error":{"code":"unsupported_parameter","param":"reasoning_effort","message":"nope"}}"#.into()));
        let e = run(&mock.base, "gpt-5-mini", "gpt-5-mini", "2025-04-01-preview", "K", "s", "u", &Deadline::new(5000)).unwrap_err();
        assert!(matches!(e, LlmError::Http { status: 400, .. }));
        assert_eq!(mock.reqs.lock().unwrap().len(), 2);
    }

    #[test]
    fn json_error_code_decides_the_shape_switch() {
        // Not a shape problem: the value is too large. One request, mapped error.
        let mock = serve(|_| (400, r#"{"error":{"code":"invalid_value","message":"max_tokens is too large","param":"max_tokens"}}"#.into()));
        let e = run(&mock.base, "cleanup", "cleanup", "2024-10-21", "K", "s", "u", &Deadline::new(5000)).unwrap_err();
        assert_eq!(e, LlmError::Http { status: 400, detail: "max_tokens is too large".into() });
        assert_eq!(mock.reqs.lock().unwrap().len(), 1);
        // Pure decision table.
        let up = |param: &str| format!(r#"{{"error":{{"code":"unsupported_value","param":"{param}","message":"x"}}}}"#);
        assert_eq!(shape_switch(400, &up("temperature"), false), Some(true));
        assert_eq!(shape_switch(400, &up("max_completion_tokens"), true), Some(false));
        assert_eq!(shape_switch(400, &up("top_p"), false), None, "param names some other field");
        assert_eq!(shape_switch(400, &up("temperature"), true), None, "wrong direction");
        assert_eq!(shape_switch(400, UNSUPPORTED_TEMP, false), Some(true));
        // No JSON code: substring fallback.
        assert_eq!(shape_switch(400, "Unrecognized request argument supplied: temperature", false), Some(true));
        assert_eq!(shape_switch(400, r#"{"error":{"message":"max_tokens is too large"}}"#, false), Some(true));
        assert_eq!(shape_switch(429, UNSUPPORTED_TEMP, false), None);
    }

    #[test]
    fn retry_stays_inside_the_deadline() {
        let mock = serve(|_| {
            std::thread::sleep(std::time::Duration::from_millis(300));
            (400, UNSUPPORTED_TEMP.into())
        });
        let t = std::time::Instant::now();
        let r = run(&mock.base, "cleanup", "cleanup", "2024-10-21", "K", "s", "u", &Deadline::new(400));
        assert!(r.is_err());
        assert!(t.elapsed() < std::time::Duration::from_millis(1500), "took {:?}", t.elapsed());
    }

    #[test]
    fn explicit_azure_model_picks_reasoning_body_first_try() {
        // SAFETY: same variable and value as `complete_uses_config_and_override`.
        unsafe { std::env::set_var("OPENTALK_SECRET_AZURE_OPENAI", "ENVKEY") };
        let mock = reasoning_backend();
        let mut c = cfg(LlmProviderId::AzureOpenai);
        c.ai_cleanup.azure_endpoint = "https://unused.openai.azure.com".into();
        c.ai_cleanup.azure_deployment = "cleanup".into();
        c.ai_cleanup.azure_model = " gpt-5-mini ".into();
        c.ai_cleanup.azure_api_version = "2025-04-01-preview".into();
        assert_eq!(complete_with(&c, "s", "u", 5000, Some(&mock.base)).unwrap(), "Hello, world.");
        let reqs = mock.reqs.lock().unwrap();
        assert_eq!(reqs.len(), 1, "no retry needed when the model is known");
        assert!(reqs[0].target.starts_with("/openai/deployments/cleanup/"), "URL still uses the deployment: {}", reqs[0].target);
        let b: Value = serde_json::from_str(&reqs[0].body).unwrap();
        assert_eq!(b["max_completion_tokens"], json!(4096));
        assert_eq!(b["reasoning_effort"], json!("minimal"));
        assert!(b.get("temperature").is_none());
    }

    #[test]
    fn explicit_azure_model_can_force_classic_shape() {
        // SAFETY: same variable and value as `complete_uses_config_and_override`.
        unsafe { std::env::set_var("OPENTALK_SECRET_AZURE_OPENAI", "ENVKEY") };
        let mock = serve(|_| (200, OK.into()));
        let mut c = cfg(LlmProviderId::AzureOpenai);
        c.ai_cleanup.azure_endpoint = "https://unused.openai.azure.com".into();
        c.ai_cleanup.azure_deployment = "gpt-5-prod-name".into();
        c.ai_cleanup.azure_model = "gpt-4.1-mini".into();
        complete_with(&c, "s", "u", 5000, Some(&mock.base)).unwrap();
        let b: Value = serde_json::from_str(&mock.reqs.lock().unwrap()[0].body).unwrap();
        assert_eq!(b["temperature"], json!(0));
    }

    #[test]
    fn errors_map() {
        let mock = serve(|_| (401, r#"{"error":{"code":"401","message":"Access denied"}}"#.into()));
        let e = run(&mock.base, "d", "d", "v", "bad", "s", "u", &Deadline::new(5000)).unwrap_err();
        assert_eq!(e, LlmError::Http { status: 401, detail: "Access denied".into() });

        let mock = serve(|_| (200, r#"{"choices":[{"message":{"content":""}}]}"#.into()));
        assert!(matches!(run(&mock.base, "d", "d", "v", "k", "s", "u", &Deadline::new(5000)), Err(LlmError::InvalidOutput(_))));
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
    fn endpoint_normalization() {
        assert_eq!(normalize_endpoint(" https://r.openai.azure.com/ ").as_deref(), Some("https://r.openai.azure.com"));
        assert_eq!(normalize_endpoint("r.openai.azure.com").as_deref(), Some("https://r.openai.azure.com"));
        assert_eq!(normalize_endpoint("http://r.openai.azure.com"), None, "api-key must not travel over http");
        assert_eq!(normalize_endpoint("  "), None);
        assert_eq!(normalize_endpoint("https://"), None);
    }

    #[test]
    fn stalled_server_times_out_at_the_deadline() {
        // SAFETY: same variable and value as `complete_uses_config_and_override`.
        unsafe { std::env::set_var("OPENTALK_SECRET_AZURE_OPENAI", "ENVKEY") };
        let mock = serve(|_| {
            std::thread::sleep(std::time::Duration::from_millis(2500));
            (200, OK.into())
        });
        let mut c = cfg(LlmProviderId::AzureOpenai);
        c.ai_cleanup.azure_deployment = "dep".into();
        let t = std::time::Instant::now();
        assert_eq!(complete_with(&c, "s", "u", 400, Some(&mock.base)), Err(LlmError::Timeout));
        assert!(t.elapsed() < std::time::Duration::from_millis(1500), "returned after {:?}", t.elapsed());
    }

    #[test]
    fn missing_endpoint_is_not_configured() {
        let c = cfg(LlmProviderId::AzureOpenai);
        assert_eq!(complete_with(&c, "s", "u", 1000, None), Err(LlmError::NotConfigured));
    }
}
