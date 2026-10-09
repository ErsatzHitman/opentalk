//! Enterprise LLM clouds for AI cleanup. TASK enterprise implements azure.rs (Azure OpenAI chat
//! completions, `api-key` header), bedrock.rs (Converse API + hand-rolled SigV4 with sha2/hmac),
//! vertex.rs (service-account JSON -> RS256 JWT -> OAuth token, cached; generateContent).
use super::LlmError;
use crate::core::config::Config;

/// One-shot completion for enterprise providers (AzureOpenai | Bedrock | Vertex).
pub fn complete(cfg: &Config, system: &str, user: &str, timeout_ms: u64) -> Result<String, LlmError> {
    let _ = (cfg, system, user, timeout_ms);
    todo!("enterprise")
}
