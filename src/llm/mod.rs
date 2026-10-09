//! Optional AI cleanup through a BYOK LLM. TASK llm implements providers (openai Responses API,
//! anthropic Messages, gemini, groq/openrouter/custom chat-completions) + prompt + validation;
//! TASK enterprise implements azure/bedrock (SigV4)/vertex (service-account JWT) in llm/enterprise/.
//!
//! Contract: the deterministic pipeline (`core::cleanup::process_pre_llm`) runs first; this module
//! only polishes. Any failure/timeout/invalid output => caller keeps the deterministic text.
pub mod prompt;
pub mod registry;

use crate::core::config::Config;

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

/// Clean `text` with the configured LLM. Uses `cfg.ai_cleanup.timeout_ms` (default 4 s), no retries.
pub fn cleanup(cfg: &Config, text: &str, hints: &[String]) -> Result<String, LlmError> {
    let _ = (cfg, text, hints);
    todo!("llm")
}

/// Validate model output against input (port of OpenWhispr cleanupOutput.ts + ratio/preamble checks).
/// Returns the sanitized output.
pub fn validate_output(raw: &str, output: &str) -> Result<String, LlmError> {
    let _ = (raw, output);
    todo!("llm")
}
