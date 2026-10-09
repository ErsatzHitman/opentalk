//! Optional AI cleanup through a BYOK LLM. TASK llm implements providers (openai Responses API,
//! anthropic Messages, gemini, groq/openrouter/custom chat-completions) + prompt + validation;
//! TASK enterprise implements azure/bedrock (SigV4)/vertex (service-account JWT) in llm/enterprise/.
//!
//! Contract: the deterministic pipeline (`core::cleanup::process_pre_llm`) runs first; this module
//! only polishes. Any failure/timeout/invalid output => caller keeps the deterministic text.
pub mod enterprise;
pub mod prompt;
pub mod providers;
pub mod registry;

use crate::core::config::{Config, LlmProviderId};
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

/// Env override of the API base URL for every non-enterprise provider (mock servers, proxies).
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
    let provider = ai.provider;
    let info = registry::provider(provider);
    let timeout_ms = if ai.timeout_ms == 0 { DEFAULT_TIMEOUT_MS } else { ai.timeout_ms };
    let system = prompt::system_prompt(&ai.custom_prompt, &cfg.language, hints);
    let user = prompt::user_message(text);

    let output = if info.enterprise {
        enterprise::complete(cfg, &system, &user, timeout_ms)?
    } else {
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
            system: &system,
            user: &user,
            max_tokens: providers::max_tokens(text.chars().count(), reasoning),
            timeout: Duration::from_millis(timeout_ms),
        };
        providers::complete(&req)?
    };
    validate_output(text, &output)
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
        c.ai_cleanup.timeout_ms = 3_000;
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
            "liquid/lfm-2.5-2.6b:free",
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
