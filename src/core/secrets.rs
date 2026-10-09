//! API keys encrypted at rest with DPAPI (`CryptProtectData`, current-user scope),
//! stored base64 in `secrets.json` as `{ "<name>": "<b64 ciphertext>" }`.
//! TASK core-data implements this. Never log values.
//!
//! Well-known names: see `SecretName` — the UI and providers use these constants.

pub struct SecretName;
impl SecretName {
    pub const OPENAI: &'static str = "openai";
    pub const GROQ: &'static str = "groq";
    pub const XAI: &'static str = "xai";
    pub const MISTRAL: &'static str = "mistral";
    pub const GEMINI: &'static str = "gemini";
    pub const DEEPGRAM: &'static str = "deepgram";
    pub const ASSEMBLYAI: &'static str = "assemblyai";
    pub const TINFOIL: &'static str = "tinfoil";
    pub const OPENROUTER: &'static str = "openrouter";
    pub const ANTHROPIC: &'static str = "anthropic";
    pub const CUSTOM_STT: &'static str = "custom_stt";
    pub const CUSTOM_LLM: &'static str = "custom_llm";
    pub const CORTI_CLIENT_ID: &'static str = "corti_client_id";
    pub const CORTI_CLIENT_SECRET: &'static str = "corti_client_secret";
    pub const AZURE_OPENAI: &'static str = "azure_openai";
    pub const AWS_ACCESS_KEY_ID: &'static str = "aws_access_key_id";
    pub const AWS_SECRET_ACCESS_KEY: &'static str = "aws_secret_access_key";
    pub const VERTEX_SERVICE_ACCOUNT_JSON: &'static str = "vertex_service_account_json";
}

/// Read and decrypt a secret. `None` if absent or undecryptable.
/// Env override for tests: `OPENTALK_SECRET_<NAME_UPPER>` (e.g. OPENTALK_SECRET_OPENROUTER).
pub fn get(name: &str) -> Option<String> {
    let _ = name;
    todo!("core-data: DPAPI decrypt from secrets.json")
}

/// Encrypt and store (empty string deletes).
pub fn set(name: &str, value: &str) -> std::io::Result<()> {
    let _ = (name, value);
    todo!("core-data: DPAPI encrypt into secrets.json (atomic write)")
}

/// Names that currently have a stored value.
pub fn list() -> Vec<String> {
    todo!("core-data")
}

/// "sk-...abcd" style mask for display.
pub fn mask(value: &str) -> String {
    let v = value.trim();
    if v.len() <= 8 { return "•".repeat(v.len()); }
    format!("{}...{}", &v[..3.min(v.len())], &v[v.len() - 4..])
}
