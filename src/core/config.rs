//! Persistent settings (`config.json`). Secrets are NOT stored here — see `secrets.rs`.
//!
//! Every field has a default (`#[serde(default)]`), so old/partial files always load.
//! The settings UI writes this file and then calls `ipc::notify_daemon(IpcMsg::Reload)`.
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const CONFIG_VERSION: u32 = 1;

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "kebab-case")]
pub enum Theme { #[default] Auto, Light, Dark }

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "kebab-case")]
pub enum PanelPosition { #[default] BottomRight, BottomCenter, BottomLeft }

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "kebab-case")]
pub enum ActivationMode {
    /// Press toggles recording (OpenWhispr default).
    #[default]
    Tap,
    /// Hold to record; recording starts after 150 ms, release stops, shorter press cancels.
    Hold,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "kebab-case")]
pub enum FillerMode { Off, #[default] Standard, Aggressive }

/// Cloud speech-to-text providers (OpenWhispr BYOK parity + OpenRouter).
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
#[serde(rename_all = "kebab-case")]
pub enum SttProviderId {
    #[default]
    Openai,
    Groq,
    Xai,
    Mistral,
    Gemini,
    Deepgram,
    Assemblyai,
    Corti,
    Tinfoil,
    Openrouter,
    Custom,
}

/// LLM providers for optional AI cleanup.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
#[serde(rename_all = "kebab-case")]
pub enum LlmProviderId {
    #[default]
    Openai,
    Anthropic,
    Gemini,
    Groq,
    Openrouter,
    Custom,
    AzureOpenai,
    Bedrock,
    Vertex,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Snippet { pub trigger: String, pub replacement: String }

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct SttConfig {
    pub provider: SttProviderId,
    /// Selected model per provider (so switching tabs remembers choices).
    pub models: BTreeMap<SttProviderId, String>,
    /// Prefer realtime/streaming transport when the selected model supports it.
    pub prefer_streaming: bool,
    /// Custom OpenAI-compatible endpoint (also Azure OpenAI deployments).
    pub custom_base_url: String,
    pub custom_model: String,
    /// Corti environment ("us" | "eu") and tenant.
    pub corti_environment: String,
    pub corti_tenant: String,
    /// Request timeout for batch uploads.
    pub timeout_ms: u64,
}

impl Default for SttConfig {
    fn default() -> Self {
        Self {
            provider: SttProviderId::Openai,
            models: BTreeMap::new(),
            prefer_streaming: false,
            custom_base_url: String::new(),
            custom_model: "whisper-1".into(),
            corti_environment: "us".into(),
            corti_tenant: "base".into(),
            timeout_ms: 30_000,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct AiCleanupConfig {
    pub enabled: bool,
    pub provider: LlmProviderId,
    pub models: BTreeMap<LlmProviderId, String>,
    pub custom_base_url: String,
    /// Empty = OpenWhispr default cleanup prompt.
    pub custom_prompt: String,
    pub timeout_ms: u64,
    /// Enterprise: Azure OpenAI endpoint + deployment + api-version.
    pub azure_endpoint: String,
    pub azure_deployment: String,
    pub azure_api_version: String,
    /// Optional underlying model name (e.g. `gpt-5-mini`) behind the deployment. Decides the
    /// request shape (reasoning vs classic) when the deployment name does not say. Empty = guess
    /// from the deployment name.
    pub azure_model: String,
    /// Enterprise: AWS Bedrock region; Vertex project/location.
    pub bedrock_region: String,
    pub vertex_project: String,
    pub vertex_location: String,
}

impl Default for AiCleanupConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            provider: LlmProviderId::Openrouter,
            models: BTreeMap::new(),
            custom_base_url: String::new(),
            custom_prompt: String::new(),
            timeout_ms: 4_000,
            azure_endpoint: String::new(),
            azure_deployment: String::new(),
            azure_api_version: "2024-10-21".into(),
            azure_model: String::new(),
            bedrock_region: "us-east-1".into(),
            vertex_project: String::new(),
            vertex_location: "us-central1".into(),
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Config {
    pub version: u32,
    // ---- Preferences ----
    pub theme: Theme,
    /// Paint an opaque window background instead of the translucent Windows backdrop (Settings).
    pub reduce_transparency: bool,
    pub audio_cues: bool,
    pub pause_media: bool,
    pub auto_paste: bool,
    pub keep_in_clipboard: bool,
    pub smart_spacing: bool,
    pub floating_icon_auto_hide: bool,
    pub panel_position: PanelPosition,
    pub show_app_bubble: bool,
    /// BCP-47-ish language code or "auto".
    pub language: String,
    pub start_minimized: bool,
    /// WASAPI endpoint id; None = system default.
    pub mic_device_id: Option<String>,
    pub mic_device_label: Option<String>,
    /// Keep the mic stream open N seconds after a dictation (0 = off).
    pub mic_warm_hold_secs: u32,
    // ---- Hotkeys ----
    /// Electron-style accelerator, e.g. "Control+Super", "F8", "Control+Shift+Space".
    pub hotkey: String,
    pub activation_mode: ActivationMode,
    pub suppress_start_menu: bool,
    // ---- Speech-to-text ----
    pub stt: SttConfig,
    // ---- Cleanup ----
    pub filler_mode: FillerMode,
    pub fuzzy_dictionary: bool,
    pub spoken_line_breaks: bool,
    pub ai_cleanup: AiCleanupConfig,
    // ---- Dictionary ----
    pub dictionary: Vec<String>,
    pub snippets: Vec<Snippet>,
    // ---- Privacy & data ----
    pub history_enabled: bool,
    /// 0 = keep forever.
    pub history_retention_days: u32,
    /// Keep the last failed recording in memory for "Retry".
    pub keep_failed_audio: bool,
    pub save_discarded: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            version: CONFIG_VERSION,
            theme: Theme::Auto,
            reduce_transparency: false,
            audio_cues: true,
            pause_media: false,
            auto_paste: true,
            keep_in_clipboard: false,
            smart_spacing: true,
            floating_icon_auto_hide: false,
            panel_position: PanelPosition::BottomRight,
            show_app_bubble: true,
            language: "auto".into(),
            start_minimized: false,
            mic_device_id: None,
            mic_device_label: None,
            mic_warm_hold_secs: 0,
            hotkey: "Control+Super".into(),
            activation_mode: ActivationMode::Tap,
            suppress_start_menu: true,
            stt: SttConfig::default(),
            filler_mode: FillerMode::Standard,
            fuzzy_dictionary: true,
            spoken_line_breaks: false,
            ai_cleanup: AiCleanupConfig::default(),
            dictionary: Vec::new(),
            snippets: Vec::new(),
            history_enabled: true,
            history_retention_days: 0,
            keep_failed_audio: true,
            save_discarded: false,
        }
    }
}

impl Config {
    /// Load from disk; missing/corrupt file -> defaults (corrupt file is backed up as config.json.bad).
    pub fn load() -> Self {
        let path = super::paths::config_file();
        match std::fs::read_to_string(&path) {
            Ok(s) => match serde_json::from_str::<Config>(&s) {
                Ok(mut c) => {
                    c.migrate();
                    c
                }
                Err(e) => {
                    crate::log_warn!("config parse error: {e}; using defaults");
                    let _ = std::fs::rename(&path, path.with_extension("json.bad"));
                    Config::default()
                }
            },
            Err(_) => Config::default(),
        }
    }

    /// Atomic save (write temp + rename).
    pub fn save(&self) -> std::io::Result<()> {
        let path = super::paths::config_file();
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self).expect("config serializes"))?;
        std::fs::rename(tmp, path)
    }

    fn migrate(&mut self) {
        if self.version < CONFIG_VERSION {
            self.version = CONFIG_VERSION;
        }
    }

    /// Model for the active STT provider (falls back to the registry default).
    pub fn stt_model(&self) -> String {
        if self.stt.provider == SttProviderId::Custom {
            return if self.stt.custom_model.trim().is_empty() { "whisper-1".into() } else { self.stt.custom_model.clone() };
        }
        self.stt
            .models
            .get(&self.stt.provider)
            .filter(|m| !m.is_empty())
            .cloned()
            .unwrap_or_else(|| crate::stt::registry::default_model(self.stt.provider).to_string())
    }

    /// Model for the active cleanup LLM provider.
    pub fn llm_model(&self) -> String {
        self.ai_cleanup
            .models
            .get(&self.ai_cleanup.provider)
            .filter(|m| !m.is_empty())
            .cloned()
            .unwrap_or_else(|| crate::llm::registry::default_model(self.ai_cleanup.provider).to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn partial_json_loads_with_defaults() {
        let c: Config = serde_json::from_str(r#"{"hotkey":"F8","stt":{"provider":"groq"}}"#).unwrap();
        assert_eq!(c.hotkey, "F8");
        assert_eq!(c.stt.provider, SttProviderId::Groq);
        assert!(c.auto_paste);
        assert_eq!(c.stt.timeout_ms, 30_000);
    }
    #[test]
    fn azure_model_defaults_empty_for_old_files() {
        // A config written before `azure_model` existed still loads, with the field empty.
        let c: Config = serde_json::from_str(r#"{"version":1,"ai_cleanup":{"provider":"azure-openai","azure_deployment":"cleanup"}}"#).unwrap();
        assert_eq!(c.ai_cleanup.azure_deployment, "cleanup");
        assert_eq!(c.ai_cleanup.azure_model, "");
        assert_eq!(c.ai_cleanup.azure_api_version, "2024-10-21");
        assert_eq!(Config::default().ai_cleanup.azure_model, "");
        let mut m = c;
        m.ai_cleanup.azure_model = "gpt-5".into();
        let back: Config = serde_json::from_str(&serde_json::to_string(&m).unwrap()).unwrap();
        assert_eq!(back.ai_cleanup.azure_model, "gpt-5");
    }
    #[test]
    fn reduce_transparency_defaults_off_for_old_files() {
        // A config written before the glass Settings UI (no `reduce_transparency`) loads with it off,
        // keeps every other field, and the new field round-trips once set.
        let c: Config = serde_json::from_str(r#"{"version":1,"theme":"dark","audio_cues":false}"#).unwrap();
        assert!(!c.reduce_transparency);
        assert_eq!(c.theme, Theme::Dark);
        assert!(!c.audio_cues);
        assert!(!Config::default().reduce_transparency);
        let mut m = c;
        m.reduce_transparency = true;
        let back: Config = serde_json::from_str(&serde_json::to_string(&m).unwrap()).unwrap();
        assert!(back.reduce_transparency);
    }
    #[test]
    fn roundtrip() {
        let c = Config::default();
        let s = serde_json::to_string(&c).unwrap();
        assert_eq!(serde_json::from_str::<Config>(&s).unwrap(), c);
    }
}
