//! Static STT provider/model catalogue (mirrors OpenWhispr modelRegistryData.json
//! `transcriptionProviders`, plus OpenRouter). Used by the settings UI and dispatch.
use crate::core::config::SttProviderId;

#[derive(Clone, Copy, Debug)]
pub struct ModelInfo {
    pub id: &'static str,
    pub name: &'static str,
    pub description: &'static str,
    /// Realtime/WebSocket transport available.
    pub streaming: bool,
    /// Only a streaming transport exists (no batch HTTP API).
    pub streaming_only: bool,
}

#[derive(Clone, Copy, Debug)]
pub struct ProviderInfo {
    pub id: SttProviderId,
    pub name: &'static str,
    pub base_url: &'static str,
    pub key_url: &'static str,
    /// Secret name in `secrets` (see SecretName).
    pub secret: &'static str,
    pub models: &'static [ModelInfo],
    pub note: &'static str,
}

const fn m(id: &'static str, name: &'static str, description: &'static str, streaming: bool, streaming_only: bool) -> ModelInfo {
    ModelInfo { id, name, description, streaming, streaming_only }
}

pub const PROVIDERS: &[ProviderInfo] = &[
    ProviderInfo {
        id: SttProviderId::Openai, name: "OpenAI", base_url: "https://api.openai.com/v1",
        key_url: "https://platform.openai.com/api-keys", secret: "openai",
        models: &[
            m("gpt-4o-mini-transcribe", "GPT-4o Mini Transcribe", "Fast, low cost", true, false),
            m("gpt-4o-transcribe", "GPT-4o Transcribe", "Highest accuracy", true, false),
            m("gpt-transcribe", "GPT Transcribe", "Latest, keyword biasing", false, false),
            m("whisper-1", "Whisper", "Classic Whisper", false, false),
        ],
        note: "",
    },
    ProviderInfo {
        id: SttProviderId::Groq, name: "Groq", base_url: "https://api.groq.com/openai/v1",
        key_url: "https://console.groq.com/keys", secret: "groq",
        models: &[
            m("whisper-large-v3-turbo", "Whisper Large v3 Turbo", "216x real-time speed", false, false),
            m("whisper-large-v3", "Whisper Large v3", "Most accurate Whisper", false, false),
        ],
        note: "",
    },
    ProviderInfo {
        id: SttProviderId::Xai, name: "xAI", base_url: "https://api.x.ai/v1",
        key_url: "https://console.x.ai", secret: "xai",
        models: &[m("grok-stt", "Grok STT", "xAI speech-to-text", false, false)],
        note: "",
    },
    ProviderInfo {
        id: SttProviderId::Mistral, name: "Mistral", base_url: "https://api.mistral.ai/v1",
        key_url: "https://console.mistral.ai/api-keys", secret: "mistral",
        models: &[m("voxtral-mini-latest", "Voxtral Mini", "Mistral speech model", false, false)],
        note: "",
    },
    ProviderInfo {
        id: SttProviderId::Gemini, name: "Gemini", base_url: "https://generativelanguage.googleapis.com/v1beta",
        key_url: "https://aistudio.google.com/apikey", secret: "gemini",
        models: &[
            m("gemini-3.5-transcribe", "Gemini 3.5 Transcribe", "Batch transcription", false, false),
            m("gemini-3.5-transcribe-live", "Gemini 3.5 Transcribe Live", "Realtime", true, true),
        ],
        note: "",
    },
    ProviderInfo {
        id: SttProviderId::Deepgram, name: "Deepgram", base_url: "https://api.deepgram.com/v1",
        key_url: "https://console.deepgram.com", secret: "deepgram",
        models: &[m("nova-3", "Nova-3", "Realtime streaming", true, false)],
        note: "",
    },
    ProviderInfo {
        id: SttProviderId::Assemblyai, name: "AssemblyAI", base_url: "https://api.assemblyai.com/v2",
        key_url: "https://www.assemblyai.com/dashboard", secret: "assemblyai",
        models: &[
            m("universal-3-5-pro", "Universal 3.5 Pro", "Streaming, multilingual", true, true),
            m("universal-streaming-english", "Universal Streaming (English)", "Streaming", true, true),
            m("universal-streaming-multilingual", "Universal Streaming (Multilingual)", "Streaming", true, true),
        ],
        note: "",
    },
    ProviderInfo {
        id: SttProviderId::Corti, name: "Corti", base_url: "https://api.us.corti.app/v2",
        key_url: "https://console.corti.app", secret: "corti_client_id",
        models: &[m("corti-transcribe", "Corti Transcribe", "Medical dictation", true, false)],
        note: "Client ID + secret (OAuth)",
    },
    ProviderInfo {
        id: SttProviderId::Tinfoil, name: "Tinfoil", base_url: "https://inference.tinfoil.sh/v1",
        key_url: "https://tinfoil.sh", secret: "tinfoil",
        models: &[m("voxtral-small-24b", "Voxtral", "Private inference", false, false)],
        note: "Plain HTTPS - enclave attestation is not verified",
    },
    ProviderInfo {
        id: SttProviderId::Openrouter, name: "OpenRouter", base_url: "https://openrouter.ai/api/v1",
        key_url: "https://openrouter.ai/keys", secret: "openrouter",
        models: &[
            m("google/gemini-3.5-flash-lite", "Gemini 3.5 Flash Lite", "Audio-capable LLM", false, false),
            m("thinkingmachines/inkling-small:free", "Inkling Small (free)", "Free tier, rate-limited", false, false),
        ],
        note: "Audio-input chat models",
    },
    ProviderInfo {
        id: SttProviderId::Custom, name: "Custom", base_url: "", key_url: "", secret: "custom_stt",
        models: &[],
        note: "Any OpenAI-compatible /audio/transcriptions endpoint (incl. Azure)",
    },
];

pub fn provider(id: SttProviderId) -> &'static ProviderInfo {
    PROVIDERS.iter().find(|p| p.id == id).expect("every provider id is registered")
}

pub fn default_model(id: SttProviderId) -> &'static str {
    provider(id).models.first().map(|m| m.id).unwrap_or("whisper-1")
}

pub fn model(id: SttProviderId, model_id: &str) -> Option<&'static ModelInfo> {
    provider(id).models.iter().find(|m| m.id == model_id)
}
