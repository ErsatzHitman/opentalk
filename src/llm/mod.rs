//! Optional AI cleanup (and the voice "rewrite concisely" request) through a BYOK LLM. TASK llm implements providers (openai Responses API,
//! anthropic Messages, gemini, groq/openrouter/custom chat-completions) + prompt + validation;
//! TASK enterprise implements azure/bedrock (SigV4)/vertex (service-account JWT) in llm/enterprise/.
//!
//! Contract: the deterministic pipeline (`core::cleanup::process_pre_llm`) runs first; this module
//! only polishes. Any failure/timeout/invalid output => caller keeps the deterministic text.
//! [`rewrite`] shares the provider, model, key and transport with [`cleanup`] but has its own
//! prompt, validator (a concise rewrite legitimately shrinks) and a longer timeout.
pub mod enterprise;
pub mod prompt;
pub mod providers;
pub mod registry;

use crate::core::config::{Config, DEFAULT_REWRITE_TIMEOUT_MS, LlmProviderId};
use providers::Req;
use std::time::Duration;

#[derive(Clone, Debug, PartialEq)]
pub enum LlmError {
    NotConfigured,
    Timeout,
    Http { status: u16, detail: String },
    Network(String),
    /// Output rejected by validation (duplicated, preamble-only, length ratio, empty-for-nonempty).
    InvalidOutput(String),
}

impl std::fmt::Display for LlmError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}

/// Default hard deadline for a cleanup call.
const DEFAULT_TIMEOUT_MS: u64 = 4_000;

/// Bounds of the configurable rewrite timeout (ms).
const REWRITE_TIMEOUT_MIN_MS: u64 = 250;
const REWRITE_TIMEOUT_MAX_MS: u64 = 60_000;

/// Env override of the API origin for every provider (mock servers, proxies). Enterprise
/// providers keep their path layout and swap only the origin.
pub const BASE_URL_ENV: &str = "OPENTALK_LLM_BASE_URL";

/// Clean `text` with the configured LLM. Uses `cfg.ai_cleanup.timeout_ms` (default 4 s), no retries.
pub fn cleanup(cfg: &Config, text: &str, hints: &[String]) -> Result<String, LlmError> {
    let info = registry::provider(cfg.ai_cleanup.provider);
    let key = if info.enterprise { None } else { crate::core::secrets::get(info.secret) };
    let base = std::env::var(BASE_URL_ENV).ok().filter(|b| !b.trim().is_empty());
    cleanup_with(cfg, text, hints, key.as_deref(), base.as_deref())
}

/// [`cleanup`] with the API key and base URL passed in (no secret store or environment access).
pub fn cleanup_with(
    cfg: &Config,
    text: &str,
    hints: &[String],
    key: Option<&str>,
    base_override: Option<&str>,
) -> Result<String, LlmError> {
    // Nothing to polish: skip the network round trip.
    if text.trim().is_empty() {
        return Ok(String::new());
    }
    let ai = &cfg.ai_cleanup;
    let timeout_ms = if ai.timeout_ms == 0 { DEFAULT_TIMEOUT_MS } else { ai.timeout_ms };
    let system = prompt::system_prompt(&ai.custom_prompt, &cfg.language, hints);
    let user = prompt::user_message(text);
    let output = run(cfg, &system, &user, text.chars().count(), timeout_ms, key, base_override)?;
    validate_output(text, &output)
}

/// Timeout of the rewrite call: `rewrite_timeout_ms` (0 = [`DEFAULT_REWRITE_TIMEOUT_MS`]), kept
/// inside a sane range so a typo in config.json cannot hang the dictation or fail it instantly.
pub fn rewrite_timeout_ms(cfg: &Config) -> u64 {
    match cfg.ai_cleanup.rewrite_timeout_ms {
        0 => DEFAULT_REWRITE_TIMEOUT_MS,
        ms => ms.clamp(REWRITE_TIMEOUT_MIN_MS, REWRITE_TIMEOUT_MAX_MS),
    }
}

/// Rewrite `text` (a dictation without its trigger phrase) more concisely with the shared AI
/// provider and key and the rewrite's own model. Works whether or not "Enable AI cleanup" is on. Uses
/// [`rewrite_timeout_ms`] (default 10 s), no retries. `hints` are dictionary words plus snippet
/// triggers, so the model keeps them intact for the snippet stage that follows.
pub fn rewrite(cfg: &Config, text: &str, hints: &[String]) -> Result<String, LlmError> {
    let info = registry::provider(cfg.ai_cleanup.provider);
    let key = if info.enterprise { None } else { crate::core::secrets::get(info.secret) };
    let base = std::env::var(BASE_URL_ENV).ok().filter(|b| !b.trim().is_empty());
    rewrite_with(cfg, text, hints, key.as_deref(), base.as_deref())
}

/// [`rewrite`] with the API key and base URL passed in (no secret store or environment access).
pub fn rewrite_with(
    cfg: &Config,
    text: &str,
    hints: &[String],
    key: Option<&str>,
    base_override: Option<&str>,
) -> Result<String, LlmError> {
    if text.trim().is_empty() {
        return Err(LlmError::InvalidOutput("nothing to rewrite".into()));
    }
    let system = prompt::rewrite_system_prompt(&cfg.language, hints);
    let user = prompt::rewrite_user_message(text);
    // the rewrite's own model (and Azure deployment) on the shared provider
    let rw = cfg.for_llm_task(crate::core::config::LlmTask::Rewrite);
    let output = run(&rw, &system, &user, text.chars().count(), rewrite_timeout_ms(cfg), key, base_override)?;
    providers::sanitize::validate_rewrite(text, &output)
}

/// One completion with the configured provider (enterprise or BYOK). Returns the raw model text.
fn run(
    cfg: &Config,
    system: &str,
    user: &str,
    input_chars: usize,
    timeout_ms: u64,
    key: Option<&str>,
    base_override: Option<&str>,
) -> Result<String, LlmError> {
    let ai = &cfg.ai_cleanup;
    let provider = ai.provider;
    let info = registry::provider(provider);
    if info.enterprise {
        return enterprise::complete_with(cfg, system, user, timeout_ms, base_override);
    }
    let model = cfg.llm_model();
    if model.trim().is_empty() {
        return Err(LlmError::NotConfigured);
    }
    let base = base_override
        .map(str::to_string)
        .unwrap_or_else(|| if provider == LlmProviderId::Custom { ai.custom_base_url.clone() } else { info.base_url.to_string() });
    let base = base.trim().trim_end_matches('/');
    if base.is_empty() {
        return Err(LlmError::NotConfigured);
    }
    // A key is required everywhere except Custom (LAN / self-hosted servers often run open).
    let key = key.map(str::trim).unwrap_or("");
    if key.is_empty() && provider != LlmProviderId::Custom {
        return Err(LlmError::NotConfigured);
    }
    let reasoning = provider == LlmProviderId::Openai && providers::openai::is_reasoning(&model);
    let req = Req {
        provider,
        base_url: base,
        model: &model,
        key,
        system,
        user,
        max_tokens: providers::max_tokens(input_chars, reasoning),
        timeout: Duration::from_millis(timeout_ms),
    };
    providers::complete(&req)
}

/// Validate model output against input (port of OpenWhispr cleanupOutput.ts + ratio/preamble checks).
/// Returns the sanitized output.
pub fn validate_output(raw: &str, output: &str) -> Result<String, LlmError> {
    providers::sanitize::validate(raw, output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::config::Config;
    use providers::testutil::serve_once;

    const RAW: &str = "um so can you uh send me the report by friday";
    const CLEAN: &str = "Can you send me the report by Friday?";

    fn cfg(provider: LlmProviderId, model: &str) -> Config {
        let mut c = Config::default();
        c.ai_cleanup.provider = provider;
        c.ai_cleanup.models.insert(provider, model.to_string());
        c.ai_cleanup.rewrite_models.insert(provider, model.to_string());
        c.ai_cleanup.timeout_ms = 3_000;
        c.ai_cleanup.rewrite_timeout_ms = 3_000;
        c.language = "en".into();
        c
    }

    fn chat_body(content: &str) -> String {
        serde_json::json!({"choices":[{"message":{"role":"assistant","content":content},"finish_reason":"stop"}]}).to_string()
    }

    #[test]
    fn validate_output_wrapper() {
        assert_eq!(validate_output(RAW, CLEAN).unwrap(), CLEAN);
        assert!(matches!(validate_output(RAW, &format!("{CLEAN} {CLEAN}")), Err(LlmError::InvalidOutput(_))));
    }

    #[test]
    fn empty_input_skips_the_network() {
        let c = cfg(LlmProviderId::Openai, "gpt-5-nano");
        // No key and an unroutable base: would fail if a request were attempted.
        assert_eq!(cleanup_with(&c, "  ", &[], None, None).unwrap(), "");
    }

    #[test]
    fn not_configured_cases() {
        let c = cfg(LlmProviderId::Openai, "gpt-5-nano");
        assert_eq!(cleanup_with(&c, RAW, &[], None, None), Err(LlmError::NotConfigured));
        assert_eq!(cleanup_with(&c, RAW, &[], Some("  "), None), Err(LlmError::NotConfigured));
        // Custom needs a base URL and a model, not a key.
        let mut c = cfg(LlmProviderId::Custom, "llama");
        assert_eq!(cleanup_with(&c, RAW, &[], Some("k"), None), Err(LlmError::NotConfigured));
        c.ai_cleanup.custom_base_url = "http://127.0.0.1:1".into();
        c.ai_cleanup.models.insert(LlmProviderId::Custom, String::new());
        assert_eq!(cleanup_with(&c, RAW, &[], None, None), Err(LlmError::NotConfigured));
    }

    #[test]
    fn full_path_openai_responses() {
        let (base, rx) = serve_once(200, r#"{"output_text":"Can you send me the report by Friday?"}"#, Duration::ZERO);
        let c = cfg(LlmProviderId::Openai, "gpt-5-nano");
        let out = cleanup_with(&c, RAW, &["Friday".to_string()], Some("sk-test-key-12345"), Some(&format!("{base}/v1"))).unwrap();
        assert_eq!(out, CLEAN);
        let req = rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(req.path(), "/v1/responses");
        let b = req.json();
        let sys = b["input"][0]["content"].as_str().unwrap();
        assert!(sys.starts_with("You are a transcript cleanup engine"));
        assert!(sys.contains("LANGUAGE CONTEXT"));
        assert!(sys.ends_with("when they appear in the text): Friday"));
        assert_eq!(b["input"][1]["content"], format!("<transcript>\n{RAW}\n</transcript>\n\nOutput only the cleaned transcript."));
        assert!(b["max_output_tokens"].as_u64().unwrap() >= 1024);
    }

    #[test]
    fn enterprise_honours_base_override() {
        // SAFETY: every test that sets this variable sets the same value.
        unsafe { std::env::set_var("OPENTALK_SECRET_AZURE_OPENAI", "ENVKEY") };
        let body = chat_body(CLEAN);
        let (base, rx) = serve_once(200, &body, Duration::ZERO);
        let mut c = cfg(LlmProviderId::AzureOpenai, "");
        c.ai_cleanup.azure_deployment = "dep".into();
        assert_eq!(cleanup_with(&c, RAW, &[], None, Some(&base)).unwrap(), CLEAN);
        let req = rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(req.path(), "/openai/deployments/dep/chat/completions?api-version=2024-10-21");
    }

    #[test]
    fn full_path_anthropic_gemini_chat_custom() {
        let (base, rx) = serve_once(200, r#"{"content":[{"type":"text","text":"<think>x</think>Can you send me the report by Friday?"}]}"#, Duration::ZERO);
        let out = cleanup_with(&cfg(LlmProviderId::Anthropic, "claude-haiku-4-5"), RAW, &[], Some("sk-ant-test-12345"), Some(&format!("{base}/v1"))).unwrap();
        assert_eq!(out, CLEAN, "thinking stripped");
        assert_eq!(rx.recv_timeout(Duration::from_secs(2)).unwrap().path(), "/v1/messages");

        let (base, rx) = serve_once(200, r#"{"candidates":[{"content":{"parts":[{"text":"Here is the cleaned transcript:\nCan you send me the report by Friday?"}]}}]}"#, Duration::ZERO);
        let out = cleanup_with(&cfg(LlmProviderId::Gemini, "gemini-3.5-flash-lite"), RAW, &[], Some("AIza-test-key-123"), Some(&format!("{base}/v1beta"))).unwrap();
        assert_eq!(out, CLEAN, "preamble stripped");
        assert!(rx.recv_timeout(Duration::from_secs(2)).unwrap().path().ends_with(":generateContent"));

        for p in [LlmProviderId::Groq, LlmProviderId::Openrouter] {
            let (base, rx) = serve_once(200, &chat_body(&format!("\"{CLEAN}\"")), Duration::ZERO);
            let out = cleanup_with(&cfg(p, "some/model"), RAW, &[], Some("sk-test-key-12345"), Some(&base)).unwrap();
            assert_eq!(out, CLEAN, "quotes stripped");
            assert_eq!(rx.recv_timeout(Duration::from_secs(2)).unwrap().path(), "/chat/completions");
        }

        // Custom: base URL from config, no key needed.
        let (base, rx) = serve_once(200, &chat_body(CLEAN), Duration::ZERO);
        let mut c = cfg(LlmProviderId::Custom, "llama-3");
        c.ai_cleanup.custom_base_url = format!("{base}/v1/");
        assert_eq!(cleanup_with(&c, RAW, &[], None, None).unwrap(), CLEAN);
        let req = rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(req.path(), "/v1/chat/completions");
        assert!(req.header("authorization").is_none());
    }

    #[test]
    fn invalid_output_is_rejected_end_to_end() {
        let (base, _rx) = serve_once(200, &chat_body(&format!("{CLEAN} {CLEAN}")), Duration::ZERO);
        let r = cleanup_with(&cfg(LlmProviderId::Groq, "openai/gpt-oss-20b"), RAW, &[], Some("gsk_testkey_123"), Some(&base));
        assert!(matches!(r, Err(LlmError::InvalidOutput(_))), "{r:?}");
    }

    #[test]
    fn http_error_maps_without_retry() {
        let (base, rx) = serve_once(500, r#"{"error":{"message":"boom"}}"#, Duration::ZERO);
        let r = cleanup_with(&cfg(LlmProviderId::Groq, "m"), RAW, &[], Some("gsk_testkey_123"), Some(&base));
        assert_eq!(r, Err(LlmError::Http { status: 500, detail: "boom".into() }));
        assert!(rx.recv_timeout(Duration::from_secs(2)).is_ok());
        // The one-shot server is gone: a retry would have surfaced as a network error, not above.
    }

    #[test]
    fn hard_timeout_fires_and_does_not_wait_for_the_server() {
        let (base, _rx) = serve_once(200, &chat_body(CLEAN), Duration::from_millis(2500));
        let mut c = cfg(LlmProviderId::Groq, "m");
        c.ai_cleanup.timeout_ms = 300;
        let t = std::time::Instant::now();
        let r = cleanup_with(&c, RAW, &[], Some("gsk_testkey_123"), Some(&base));
        let ms = t.elapsed().as_millis();
        assert_eq!(r, Err(LlmError::Timeout));
        assert!(ms < 1500, "returned after {ms} ms");
    }

    #[test]
    fn connection_refused_is_network_error() {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", l.local_addr().unwrap());
        drop(l);
        let r = cleanup_with(&cfg(LlmProviderId::Groq, "m"), RAW, &[], Some("gsk_testkey_123"), Some(&base));
        assert!(matches!(r, Err(LlmError::Network(_)) | Err(LlmError::Timeout)), "{r:?}");
    }

    // ---- rewrite: contract tests against the in-process mock server --------------------------

    const WORDY: &str = "so I was thinking that maybe we could possibly move the meeting from Thursday to Friday afternoon if that works for everyone because the client is out on Thursday";
    const CONCISE: &str = "Can we move the meeting from Thursday to Friday afternoon? The client is out on Thursday.";

    fn hints() -> Vec<String> {
        vec!["Friday".to_string(), "bob sig".to_string()]
    }

    #[test]
    fn rewrite_request_shape_on_chat_completions() {
        let (base, rx) = serve_once(200, &chat_body(CONCISE), Duration::ZERO);
        let c = cfg(LlmProviderId::Openrouter, "vendor/some-model:free");
        // The cleanup toggle is off (default) and the rewrite still goes out.
        assert!(!c.ai_cleanup.enabled);
        let out = rewrite_with(&c, WORDY, &hints(), Some("sk-test-key-12345"), Some(&base)).unwrap();
        assert_eq!(out, CONCISE);
        let req = rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(req.path(), "/chat/completions");
        assert_eq!(req.header("authorization"), Some("Bearer sk-test-key-12345"));
        let b = req.json();
        assert_eq!(b["model"], "vendor/some-model:free");
        assert_eq!(b["temperature"], 0);
        assert_eq!(b["messages"][0]["role"], "system");
        let sys = b["messages"][0]["content"].as_str().unwrap();
        assert!(sys.starts_with("You are a text-rewriting engine inside a dictation app."), "{sys}");
        assert!(sys.contains("LANGUAGE CONTEXT"));
        assert!(sys.ends_with("when they appear in the text): Friday, bob sig"), "dictionary words AND snippet triggers: {sys}");
        assert!(!sys.contains("transcript cleanup engine"));
        assert_eq!(b["messages"][1]["role"], "user");
        assert_eq!(b["messages"][1]["content"], format!("<transcript>\n{WORDY}\n</transcript>\n\nOutput only the concise rewrite."));
        assert!(b["max_tokens"].as_u64().unwrap() >= 1024);
        assert_eq!(b["reasoning"]["enabled"], false, "OpenRouter thinking off");
    }

    #[test]
    fn rewrite_ignores_the_custom_cleanup_prompt() {
        let (base, rx) = serve_once(200, &chat_body(CONCISE), Duration::ZERO);
        let mut c = cfg(LlmProviderId::Groq, "m");
        c.ai_cleanup.custom_prompt = "Translate everything to French.".into();
        rewrite_with(&c, WORDY, &[], Some("gsk_testkey_123"), Some(&base)).unwrap();
        let sys = rx.recv_timeout(Duration::from_secs(2)).unwrap().json()["messages"][0]["content"].as_str().unwrap().to_string();
        assert!(!sys.contains("French") && sys.starts_with("You are a text-rewriting engine"));
    }

    #[test]
    fn rewrite_full_path_openai_anthropic_gemini_custom() {
        let (base, rx) = serve_once(200, &format!(r#"{{"output_text":"{CONCISE}"}}"#), Duration::ZERO);
        let out = rewrite_with(&cfg(LlmProviderId::Openai, "gpt-5-nano"), WORDY, &hints(), Some("sk-test-key-12345"), Some(&format!("{base}/v1"))).unwrap();
        assert_eq!(out, CONCISE);
        let req = rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(req.path(), "/v1/responses");
        let b = req.json();
        assert!(b["input"][0]["content"].as_str().unwrap().starts_with("You are a text-rewriting engine"));
        assert_eq!(b["input"][1]["content"], format!("<transcript>\n{WORDY}\n</transcript>\n\nOutput only the concise rewrite."));

        let (base, rx) = serve_once(200, &format!(r#"{{"content":[{{"type":"text","text":"<think>x</think>{CONCISE}"}}]}}"#), Duration::ZERO);
        let out = rewrite_with(&cfg(LlmProviderId::Anthropic, "claude-haiku-4-5"), WORDY, &[], Some("sk-ant-test-12345"), Some(&format!("{base}/v1"))).unwrap();
        assert_eq!(out, CONCISE, "thinking stripped");
        let req = rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(req.path(), "/v1/messages");
        assert!(req.json()["system"].as_str().unwrap().starts_with("You are a text-rewriting engine"));

        let body = serde_json::json!({"candidates":[{"content":{"parts":[{"text":format!("Here is the concise version:\n{CONCISE}")}]}}]}).to_string();
        let (base, rx) = serve_once(200, &body, Duration::ZERO);
        let out = rewrite_with(&cfg(LlmProviderId::Gemini, "gemini-3.5-flash-lite"), WORDY, &[], Some("AIza-test-key-123"), Some(&format!("{base}/v1beta"))).unwrap();
        assert_eq!(out, CONCISE, "preamble stripped");
        assert!(rx.recv_timeout(Duration::from_secs(2)).unwrap().path().ends_with(":generateContent"));

        let (base, rx) = serve_once(200, &chat_body(&format!("\"{CONCISE}\"")), Duration::ZERO);
        let mut c = cfg(LlmProviderId::Custom, "llama-3");
        c.ai_cleanup.custom_base_url = format!("{base}/v1/");
        assert_eq!(rewrite_with(&c, WORDY, &[], None, None).unwrap(), CONCISE, "quotes stripped, no key needed");
        let req = rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(req.path(), "/v1/chat/completions");
        assert!(req.header("authorization").is_none());
    }

    #[test]
    fn rewrite_enterprise_honours_base_override() {
        // SAFETY: every test that sets this variable sets the same value.
        unsafe { std::env::set_var("OPENTALK_SECRET_AZURE_OPENAI", "ENVKEY") };
        let (base, rx) = serve_once(200, &chat_body(CONCISE), Duration::ZERO);
        let mut c = cfg(LlmProviderId::AzureOpenai, "");
        c.ai_cleanup.azure_deployment = "dep".into();
        assert_eq!(rewrite_with(&c, WORDY, &[], None, Some(&base)).unwrap(), CONCISE);
        let req = rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(req.path(), "/openai/deployments/dep/chat/completions?api-version=2024-10-21");
        assert!(req.json()["messages"][0]["content"].as_str().unwrap().starts_with("You are a text-rewriting engine"));
    }

    #[test]
    fn rewrite_and_cleanup_each_send_their_own_model() {
        let mut c = cfg(LlmProviderId::Custom, "cleanup-model");
        c.ai_cleanup.rewrite_models.insert(LlmProviderId::Custom, "rewrite-model".into());
        let (base, rx) = serve_once(200, &chat_body(CONCISE), Duration::ZERO);
        c.ai_cleanup.custom_base_url = format!("{base}/v1");
        rewrite_with(&c, WORDY, &[], None, None).unwrap();
        assert_eq!(rx.recv_timeout(Duration::from_secs(2)).unwrap().json()["model"], "rewrite-model");
        let (base, rx) = serve_once(200, &chat_body(CLEAN), Duration::ZERO);
        c.ai_cleanup.custom_base_url = format!("{base}/v1");
        cleanup_with(&c, RAW, &[], None, None).unwrap();
        assert_eq!(rx.recv_timeout(Duration::from_secs(2)).unwrap().json()["model"], "cleanup-model");
    }

    #[test]
    fn rewrite_not_configured_cases() {
        let c = cfg(LlmProviderId::Openai, "gpt-5-nano");
        assert_eq!(rewrite_with(&c, WORDY, &[], None, None), Err(LlmError::NotConfigured));
        assert_eq!(rewrite_with(&c, WORDY, &[], Some("  "), None), Err(LlmError::NotConfigured));
        let mut c = cfg(LlmProviderId::Custom, "llama");
        assert_eq!(rewrite_with(&c, WORDY, &[], Some("k"), None), Err(LlmError::NotConfigured), "custom without a base URL");
        c.ai_cleanup.custom_base_url = "http://127.0.0.1:1".into();
        c.ai_cleanup.rewrite_models.insert(LlmProviderId::Custom, String::new());
        assert_eq!(rewrite_with(&c, WORDY, &[], None, None), Err(LlmError::NotConfigured), "no model");
    }

    #[test]
    fn rewrite_of_nothing_never_reaches_the_network() {
        let c = cfg(LlmProviderId::Openai, "gpt-5-nano");
        assert!(matches!(rewrite_with(&c, "  ", &[], None, None), Err(LlmError::InvalidOutput(_))));
    }

    #[test]
    fn rewrite_timeout_defaults_and_clamps() {
        let mut c = Config::default();
        assert_eq!(c.ai_cleanup.rewrite_timeout_ms, 10_000);
        assert_eq!(rewrite_timeout_ms(&c), 10_000);
        c.ai_cleanup.rewrite_timeout_ms = 0;
        assert_eq!(rewrite_timeout_ms(&c), DEFAULT_REWRITE_TIMEOUT_MS, "0 = default");
        c.ai_cleanup.rewrite_timeout_ms = 3_000;
        assert_eq!(rewrite_timeout_ms(&c), 3_000);
        c.ai_cleanup.rewrite_timeout_ms = 5;
        assert_eq!(rewrite_timeout_ms(&c), REWRITE_TIMEOUT_MIN_MS);
        c.ai_cleanup.rewrite_timeout_ms = 3_600_000;
        assert_eq!(rewrite_timeout_ms(&c), REWRITE_TIMEOUT_MAX_MS);
        // The cleanup timeout is independent.
        c.ai_cleanup.timeout_ms = 1_234;
        c.ai_cleanup.rewrite_timeout_ms = 0;
        assert_eq!(rewrite_timeout_ms(&c), 10_000);
    }

    #[test]
    fn rewrite_uses_its_own_timeout_not_the_cleanup_one() {
        // Cleanup would give up after 300 ms; the rewrite waits for its 3 s budget.
        let (base, _rx) = serve_once(200, &chat_body(CONCISE), Duration::from_millis(900));
        let mut c = cfg(LlmProviderId::Groq, "m");
        c.ai_cleanup.timeout_ms = 300;
        c.ai_cleanup.rewrite_timeout_ms = 3_000;
        assert_eq!(rewrite_with(&c, WORDY, &[], Some("gsk_testkey_123"), Some(&base)).unwrap(), CONCISE);
    }

    #[test]
    fn rewrite_hard_timeout_fires_and_does_not_wait_for_the_server() {
        let (base, _rx) = serve_once(200, &chat_body(CONCISE), Duration::from_millis(2500));
        let mut c = cfg(LlmProviderId::Groq, "m");
        c.ai_cleanup.rewrite_timeout_ms = 300;
        let t = std::time::Instant::now();
        let r = rewrite_with(&c, WORDY, &[], Some("gsk_testkey_123"), Some(&base));
        let ms = t.elapsed().as_millis();
        assert_eq!(r, Err(LlmError::Timeout));
        assert!(ms < 1500, "returned after {ms} ms");
    }

    #[test]
    fn rewrite_rejects_refusals_preambles_only_and_too_long_replies() {
        let c = cfg(LlmProviderId::Groq, "m");
        let cases = [
            "I'm sorry, but I can't help with that request.".to_string(),
            "Here is the concise version:".to_string(),
            format!("{WORDY} and also something else the speaker never said at all today"),
            format!("{CONCISE} {CONCISE}"),
            String::new(),
            "Fine.".to_string(),
        ];
        for reply in cases {
            let (base, _rx) = serve_once(200, &chat_body(&reply), Duration::ZERO);
            let r = rewrite_with(&c, WORDY, &[], Some("gsk_testkey_123"), Some(&base));
            assert!(matches!(r, Err(LlmError::InvalidOutput(_))), "{reply:?} -> {r:?}");
        }
    }

    #[test]
    fn rewrite_http_errors_map_without_retry() {
        for (status, body) in [(500, r#"{"error":{"message":"boom"}}"#), (401, r#"{"error":{"message":"bad key"}}"#), (429, r#"{"error":{"message":"slow down"}}"#)] {
            let (base, rx) = serve_once(status, body, Duration::ZERO);
            let r = rewrite_with(&cfg(LlmProviderId::Groq, "m"), WORDY, &[], Some("gsk_testkey_123"), Some(&base));
            assert!(matches!(r, Err(LlmError::Http { status: s, .. }) if s == status), "{r:?}");
            assert!(rx.recv_timeout(Duration::from_secs(2)).is_ok());
        }
    }

    #[test]
    fn rewrite_connection_refused_is_a_network_error() {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", l.local_addr().unwrap());
        drop(l);
        let r = rewrite_with(&cfg(LlmProviderId::Groq, "m"), WORDY, &[], Some("gsk_testkey_123"), Some(&base));
        assert!(matches!(r, Err(LlmError::Network(_)) | Err(LlmError::Timeout)), "{r:?}");
    }

    /// Live: OpenRouter free text models only (never Groq, never a paid model).
    /// `OPENTALK_SECRET_OPENROUTER` supplies the key; without it (or without network) the test
    /// reports UNVERIFIED and returns. Run: `cargo test live_openrouter_rewrite -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn live_openrouter_rewrite() {
        let Ok(key) = std::env::var("OPENTALK_SECRET_OPENROUTER") else {
            eprintln!("[live] UNVERIFIED: OPENTALK_SECRET_OPENROUTER not set; skipping");
            return;
        };
        let models = [
            "nvidia/nemotron-3.5-lightning:free",
            "google/gemma-4-31b-it:free",
            "nvidia/nemotron-3-super-120b-a12b:free",
            "poolside/laguna-s-2.1:free",
            "apodex/apodex-1.1-mini:free",
        ];
        let wordy = "So I was just thinking, you know, that maybe we could possibly move the team meeting from Thursday over to Friday afternoon, if that works for everybody, because the client is actually out of the office on Thursday and I would really like them to be there.";
        let (mut verified, mut unreachable) = (0, 0);
        for model in models {
            assert!(model.ends_with(":free"), "live tests use free OpenRouter models only");
            let mut c = cfg(LlmProviderId::Openrouter, model);
            c.ai_cleanup.rewrite_timeout_ms = 30_000;
            for attempt in 0..3 {
                let t = std::time::Instant::now();
                let r = rewrite_with(&c, wordy, &["OpenTalk".to_string()], Some(&key), None);
                let ms = t.elapsed().as_millis();
                match r {
                    Ok(out) => {
                        eprintln!("[live] {model}: {ms} ms -> {out:?}");
                        assert!(!out.trim().is_empty());
                        assert!(out.split_whitespace().count() < wordy.split_whitespace().count(), "not shorter: {out}");
                        assert!(out.contains("Friday") && out.contains("Thursday"), "lost a fact: {out}");
                        verified += 1;
                        break;
                    }
                    Err(LlmError::Http { status: 429, detail }) => {
                        eprintln!("[live] {model}: 429 after {ms} ms (attempt {attempt}): {detail}");
                        std::thread::sleep(Duration::from_secs(2 << attempt));
                    }
                    Err(LlmError::Network(e)) => {
                        eprintln!("[live] {model}: network error after {ms} ms: {e}");
                        unreachable += 1;
                        break;
                    }
                    Err(e) => {
                        eprintln!("[live] {model}: {ms} ms -> {e}");
                        break;
                    }
                }
            }
        }
        if verified == 0 {
            eprintln!("[live] UNVERIFIED: no free model produced a verified rewrite ({unreachable} unreachable)");
            if std::env::var("OPENTALK_LIVE_REQUIRE").is_ok_and(|v| v == "1") {
                panic!("OPENTALK_LIVE_REQUIRE=1 and nothing was verified");
            }
        } else {
            eprintln!("[live] VERIFIED: {verified}/{} free models rewrote the dictation", models.len());
        }
    }

    /// Live: OpenRouter free text models only. `OPENTALK_SECRET_OPENROUTER` supplies the key.
    /// Run: `cargo test live_openrouter_cleanup -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn live_openrouter_cleanup() {
        let Ok(key) = std::env::var("OPENTALK_SECRET_OPENROUTER") else {
            eprintln!("OPENTALK_SECRET_OPENROUTER not set; skipping");
            return;
        };
        // The three registry suggestions first, then other free text models to rotate through.
        let models = [
            "nvidia/nemotron-3.5-lightning:free",
            "google/gemma-4-31b-it:free",
            "nvidia/nemotron-3-super-120b-a12b:free",
            "poolside/laguna-s-2.1:free",
            "apodex/apodex-1.1-mini:free",
        ];
        let raw = "Um, so I think, uh, we should like ship the new release on Friday, you know, because the the client is waiting.";
        let mut successes = 0;
        for model in models {
            assert!(model.ends_with(":free"));
            let mut c = cfg(LlmProviderId::Openrouter, model);
            c.ai_cleanup.timeout_ms = 20_000;
            for attempt in 0..3 {
                let t = std::time::Instant::now();
                let r = cleanup_with(&c, raw, &["OpenTalk".to_string()], Some(&key), None);
                let ms = t.elapsed().as_millis();
                match r {
                    Ok(out) => {
                        eprintln!("[live] {model}: {ms} ms -> {out:?}");
                        assert!(!out.to_lowercase().contains("um,"), "filler left: {out}");
                        successes += 1;
                        break;
                    }
                    Err(LlmError::Http { status: 429, detail }) => {
                        eprintln!("[live] {model}: 429 after {ms} ms (attempt {attempt}): {detail}");
                        std::thread::sleep(Duration::from_secs(2 << attempt));
                    }
                    Err(e) => {
                        eprintln!("[live] {model}: {ms} ms -> {e}");
                        break;
                    }
                }
            }
        }
        // Upstream rate limits are reported, not failed.
        eprintln!("[live] {successes}/{} free models answered", models.len());
    }
}
