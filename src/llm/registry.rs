//! Cleanup-LLM provider/model catalogue (BYOK). Used by the settings UI and dispatch.
use crate::core::config::LlmProviderId;

#[derive(Clone, Copy, Debug)]
pub struct LlmProviderInfo {
    pub id: LlmProviderId,
    pub name: &'static str,
    pub base_url: &'static str,
    pub key_url: &'static str,
    pub secret: &'static str,
    /// Suggested models (first = default). The UI also accepts free text.
    pub models: &'static [&'static str],
    pub enterprise: bool,
}

pub const PROVIDERS: &[LlmProviderInfo] = &[
    LlmProviderInfo { id: LlmProviderId::Openai, name: "OpenAI", base_url: "https://api.openai.com/v1", key_url: "https://platform.openai.com/api-keys", secret: "openai", models: &["gpt-5-nano", "gpt-5-mini", "gpt-4.1-nano", "gpt-4.1-mini"], enterprise: false },
    LlmProviderInfo { id: LlmProviderId::Anthropic, name: "Anthropic", base_url: "https://api.anthropic.com/v1", key_url: "https://console.anthropic.com/settings/keys", secret: "anthropic", models: &["claude-haiku-4-5", "claude-sonnet-5"], enterprise: false },
    LlmProviderInfo { id: LlmProviderId::Gemini, name: "Gemini", base_url: "https://generativelanguage.googleapis.com/v1beta", key_url: "https://aistudio.google.com/apikey", secret: "gemini", models: &["gemini-3.5-flash-lite", "gemini-3.5-flash"], enterprise: false },
    LlmProviderInfo { id: LlmProviderId::Groq, name: "Groq", base_url: "https://api.groq.com/openai/v1", key_url: "https://console.groq.com/keys", secret: "groq", models: &["openai/gpt-oss-20b", "openai/gpt-oss-120b", "qwen/qwen3.8-27b"], enterprise: false },
    LlmProviderInfo { id: LlmProviderId::Openrouter, name: "OpenRouter", base_url: "https://openrouter.ai/api/v1", key_url: "https://openrouter.ai/keys", secret: "openrouter", models: &["nvidia/nemotron-3.5-lightning:free", "google/gemma-4-31b-it:free", "liquid/lfm-2.5-2.6b:free"], enterprise: false },
    LlmProviderInfo { id: LlmProviderId::Custom, name: "Custom", base_url: "", key_url: "", secret: "custom_llm", models: &[], enterprise: false },
    LlmProviderInfo { id: LlmProviderId::AzureOpenai, name: "Azure OpenAI", base_url: "", key_url: "", secret: "azure_openai", models: &[], enterprise: true },
    LlmProviderInfo { id: LlmProviderId::Bedrock, name: "AWS Bedrock", base_url: "", key_url: "", secret: "aws_access_key_id", models: &["anthropic.claude-haiku-4-5"], enterprise: true },
    LlmProviderInfo { id: LlmProviderId::Vertex, name: "Google Vertex AI", base_url: "", key_url: "", secret: "vertex_service_account_json", models: &["gemini-3.5-flash-lite"], enterprise: true },
];

pub fn provider(id: LlmProviderId) -> &'static LlmProviderInfo {
    PROVIDERS.iter().find(|p| p.id == id).expect("every llm provider id is registered")
}

pub fn default_model(id: LlmProviderId) -> &'static str {
    provider(id).models.first().copied().unwrap_or("")
}
