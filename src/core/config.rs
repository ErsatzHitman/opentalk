//! Persistent settings (`config.json`). Secrets are NOT stored here — see `secrets.rs`.
//!
//! Every field has a default (`#[serde(default)]`), so old/partial files always load.
//! The tray popup's Settings pages hand the edited config to the daemon, which applies it live and
//! saves it (`App::apply_config`); `ipc::notify_daemon(IpcMsg::Reload)` re-reads the file after an
//! external edit.
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// 2: AI rewrite got its own model (`ai_cleanup.rewrite_models`, `rewrite_azure_deployment`).
pub const CONFIG_VERSION: u32 = 2;

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "kebab-case")]
pub enum Theme { #[default] Auto, Light, Dark }

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "kebab-case")]
#[allow(clippy::enum_variant_names)] // the names are the serialized config values
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

/// What an LLM call is for: each has its own model on the shared provider.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum LlmTask {
    Cleanup,
    Rewrite,
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

/// Default spoken trigger for the concise rewrite.
pub const DEFAULT_REWRITE_TRIGGER: &str = "rewrite this concisely";
/// Default timeout of the rewrite LLM call (ms): generative and longer than the 4 s cleanup call.
pub const DEFAULT_REWRITE_TIMEOUT_MS: u64 = 10_000;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct AiCleanupConfig {
    pub enabled: bool,
    /// AI provider shared by AI cleanup and AI rewrite (one key, one endpoint).
    pub provider: LlmProviderId,
    /// AI cleanup's model per provider.
    pub models: BTreeMap<LlmProviderId, String>,
    /// AI rewrite's model per provider, independent of `models` (empty = the provider's default).
    pub rewrite_models: BTreeMap<LlmProviderId, String>,
    /// AI rewrite's Azure deployment (empty = `azure_deployment`).
    pub rewrite_azure_deployment: String,
    pub custom_base_url: String,
    /// Empty = OpenWhispr default cleanup prompt.
    pub custom_prompt: String,
    pub timeout_ms: u64,
    /// Voice rewrite: ending a dictation with [`Config::rewrite_trigger`] sends the text to the
    /// cleanup provider for a concise rewrite. Independent of `enabled`.
    pub rewrite_enabled: bool,
    /// Timeout of the rewrite call (ms); 0 = [`DEFAULT_REWRITE_TIMEOUT_MS`]. Not in the UI.
    pub rewrite_timeout_ms: u64,
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
            rewrite_models: BTreeMap::new(),
            rewrite_azure_deployment: String::new(),
            custom_base_url: String::new(),
            custom_prompt: String::new(),
            timeout_ms: 4_000,
            rewrite_enabled: true,
            rewrite_timeout_ms: DEFAULT_REWRITE_TIMEOUT_MS,
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
    pub audio_cues: bool,
    pub pause_media: bool,
    /// Mute the default playback device while recording (restored afterwards).
    pub mute_system_audio: bool,
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
    /// Spoken phrase that ends a dictation to request a concise rewrite (see `core::rewrite`).
    /// Empty = the feature is off. Switched on/off by `ai_cleanup.rewrite_enabled`.
    pub rewrite_trigger: String,
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
            audio_cues: true,
            pause_media: false,
            mute_system_audio: true,
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
            rewrite_trigger: DEFAULT_REWRITE_TRIGGER.into(),
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
        if self.version < 2 {
            // Before v2 the rewrite used the cleanup model: carry those choices over so an upgrade
            // changes nothing, after which the two are edited separately.
            let ai = &mut self.ai_cleanup;
            if ai.rewrite_models.is_empty() {
                ai.rewrite_models = ai.models.clone();
            }
            if ai.rewrite_azure_deployment.is_empty() {
                ai.rewrite_azure_deployment = ai.azure_deployment.clone();
            }
        }
        if self.version < CONFIG_VERSION {
            self.version = CONFIG_VERSION;
        }
    }

    /// The trigger phrase to detect, or `None` when voice rewrite is off (toggle off or empty
    /// phrase). Whether the phrase is usable (two or more words) is `rewrite::Trigger::new`'s call.
    pub fn rewrite_trigger_effective(&self) -> Option<&str> {
        let t = self.rewrite_trigger.trim();
        (self.ai_cleanup.rewrite_enabled && !t.is_empty()).then_some(t)
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

    /// AI cleanup's model for the active LLM provider.
    pub fn llm_model(&self) -> String {
        self.llm_model_for(LlmTask::Cleanup)
    }

    /// The model `task` uses with the active LLM provider (falls back to the registry default).
    pub fn llm_model_for(&self, task: LlmTask) -> String {
        let ai = &self.ai_cleanup;
        let models = match task {
            LlmTask::Cleanup => &ai.models,
            LlmTask::Rewrite => &ai.rewrite_models,
        };
        models
            .get(&ai.provider)
            .map(|m| m.trim())
            .filter(|m| !m.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| crate::llm::registry::default_model(ai.provider).to_string())
    }

    /// Set (or, with an empty `model`, reset to the default) `task`'s model for the active provider.
    pub fn set_llm_model(&mut self, task: LlmTask, model: &str) {
        let ai = &mut self.ai_cleanup;
        let provider = ai.provider;
        let models = match task {
            LlmTask::Cleanup => &mut ai.models,
            LlmTask::Rewrite => &mut ai.rewrite_models,
        };
        let m = model.trim();
        if m.is_empty() {
            models.remove(&provider);
        } else {
            models.insert(provider, m.to_string());
        }
    }

    /// `task`'s Azure deployment (the rewrite falls back to the cleanup one when it has none).
    pub fn azure_deployment_for(&self, task: LlmTask) -> &str {
        let ai = &self.ai_cleanup;
        match task {
            LlmTask::Rewrite if !ai.rewrite_azure_deployment.trim().is_empty() => ai.rewrite_azure_deployment.trim(),
            _ => ai.azure_deployment.trim(),
        }
    }

    /// This config as `task` sees it: the shared provider settings with `task`'s model and Azure
    /// deployment in the cleanup slots every provider reads.
    pub fn for_llm_task(&self, task: LlmTask) -> Config {
        let mut c = self.clone();
        if task == LlmTask::Rewrite {
            let model = self.llm_model_for(LlmTask::Rewrite);
            c.ai_cleanup.models.insert(self.ai_cleanup.provider, model);
            c.ai_cleanup.azure_deployment = self.azure_deployment_for(LlmTask::Rewrite).to_string();
        }
        c
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
    fn config_written_by_the_glass_build_still_loads() {
        // The glass Settings UI wrote `reduce_transparency`; the field is gone, and a file that still
        // carries it (or any other unknown key) loads, keeps every field it knows, and is rewritten
        // without it.
        let c: Config = serde_json::from_str(r#"{"version":1,"theme":"dark","reduce_transparency":true,"audio_cues":false,"future_key":{"a":1}}"#).unwrap();
        assert_eq!(c.theme, Theme::Dark);
        assert!(!c.audio_cues);
        let back = serde_json::to_string(&c).unwrap();
        assert!(!back.contains("reduce_transparency"));
        assert_eq!(serde_json::from_str::<Config>(&back).unwrap(), c);
    }
    #[test]
    fn mute_system_audio_defaults_on_for_old_files() {
        // A config written before "mute system audio" existed gets it on; an explicit off sticks.
        let old: Config = serde_json::from_str(r#"{"version":1,"pause_media":true,"audio_cues":false}"#).unwrap();
        assert!(old.mute_system_audio);
        assert!(old.pause_media && !old.audio_cues, "other fields untouched");
        assert!(Config::default().mute_system_audio);
        let off: Config = serde_json::from_str(r#"{"mute_system_audio":false}"#).unwrap();
        assert!(!off.mute_system_audio);
        let back: Config = serde_json::from_str(&serde_json::to_string(&off).unwrap()).unwrap();
        assert!(!back.mute_system_audio);
    }
    #[test]
    fn rewrite_settings_default_for_old_files() {
        // A config written before voice rewrite existed gets it on, with the stock phrase.
        let old: Config = serde_json::from_str(r#"{"version":1,"hotkey":"F8","ai_cleanup":{"enabled":true,"timeout_ms":2000}}"#).unwrap();
        assert_eq!(old.rewrite_trigger, "rewrite this concisely");
        assert!(old.ai_cleanup.rewrite_enabled);
        assert_eq!(old.ai_cleanup.rewrite_timeout_ms, 10_000);
        assert!(old.ai_cleanup.enabled && old.ai_cleanup.timeout_ms == 2000, "other fields untouched");
        assert_eq!(old.rewrite_trigger_effective(), Some("rewrite this concisely"));
        let d = Config::default();
        assert_eq!(d.rewrite_trigger, DEFAULT_REWRITE_TRIGGER);
        assert!(d.ai_cleanup.rewrite_enabled);
        assert_eq!(d.ai_cleanup.rewrite_timeout_ms, DEFAULT_REWRITE_TIMEOUT_MS);
    }
    #[test]
    fn rewrite_settings_explicit_values_stick() {
        let c: Config = serde_json::from_str(r#"{"rewrite_trigger":"  ","ai_cleanup":{"rewrite_enabled":false,"rewrite_timeout_ms":3000}}"#).unwrap();
        assert!(!c.ai_cleanup.rewrite_enabled);
        assert_eq!(c.ai_cleanup.rewrite_timeout_ms, 3000);
        assert_eq!(c.rewrite_trigger_effective(), None, "off and blank");
        let empty: Config = serde_json::from_str(r#"{"rewrite_trigger":""}"#).unwrap();
        assert_eq!(empty.rewrite_trigger, "", "an explicit empty phrase is kept, not reset");
        assert_eq!(empty.rewrite_trigger_effective(), None);
        let custom: Config = serde_json::from_str(r#"{"rewrite_trigger":" make it short "}"#).unwrap();
        assert_eq!(custom.rewrite_trigger_effective(), Some("make it short"));
        let back: Config = serde_json::from_str(&serde_json::to_string(&c).unwrap()).unwrap();
        assert_eq!(back, c);
    }
    #[test]
    fn rewrite_and_cleanup_models_are_independent() {
        let mut c = Config::default();
        c.ai_cleanup.provider = LlmProviderId::Openrouter;
        let default = crate::llm::registry::default_model(LlmProviderId::Openrouter).to_string();
        assert_eq!(c.llm_model_for(LlmTask::Rewrite), default);
        assert_eq!(c.llm_model_for(LlmTask::Cleanup), default);
        c.set_llm_model(LlmTask::Cleanup, "a/cleanup");
        assert_eq!(c.llm_model_for(LlmTask::Cleanup), "a/cleanup");
        assert_eq!(c.llm_model_for(LlmTask::Rewrite), default, "changing cleanup leaves the rewrite alone");
        c.set_llm_model(LlmTask::Rewrite, " b/rewrite ");
        assert_eq!(c.llm_model_for(LlmTask::Rewrite), "b/rewrite");
        assert_eq!(c.llm_model_for(LlmTask::Cleanup), "a/cleanup", "and the other way round");
        let rw = c.for_llm_task(LlmTask::Rewrite);
        assert_eq!(rw.llm_model(), "b/rewrite", "the rewrite's view puts its model in the shared slot");
        assert_eq!(c.for_llm_task(LlmTask::Cleanup).llm_model(), "a/cleanup");
        c.set_llm_model(LlmTask::Rewrite, "");
        assert_eq!(c.llm_model_for(LlmTask::Rewrite), default, "empty resets to the default");
        // Azure: the rewrite deployment falls back to the cleanup one
        c.ai_cleanup.azure_deployment = "clean-dep".into();
        assert_eq!(c.azure_deployment_for(LlmTask::Rewrite), "clean-dep");
        c.ai_cleanup.rewrite_azure_deployment = "rw-dep".into();
        assert_eq!(c.for_llm_task(LlmTask::Rewrite).ai_cleanup.azure_deployment, "rw-dep");
        assert_eq!(c.for_llm_task(LlmTask::Cleanup).ai_cleanup.azure_deployment, "clean-dep");
    }

    #[test]
    fn v1_configs_keep_the_rewrite_on_the_cleanup_model() {
        let mut c: Config = serde_json::from_str(r#"{"version":1,"ai_cleanup":{"provider":"openrouter","models":{"openrouter":"x/old"},"azure_deployment":"dep"}}"#).unwrap();
        c.migrate();
        assert_eq!(c.version, CONFIG_VERSION);
        assert_eq!(c.llm_model_for(LlmTask::Rewrite), "x/old");
        assert_eq!(c.ai_cleanup.rewrite_azure_deployment, "dep");
        c.set_llm_model(LlmTask::Cleanup, "y/new");
        assert_eq!(c.llm_model_for(LlmTask::Rewrite), "x/old", "after the upgrade they are separate");
        // a v2 file is not migrated again
        let mut v2: Config = serde_json::from_str(r#"{"version":2,"ai_cleanup":{"models":{"openrouter":"x/old"}}}"#).unwrap();
        v2.migrate();
        assert!(v2.ai_cleanup.rewrite_models.is_empty());
    }

    #[test]
    fn roundtrip() {
        let c = Config::default();
        let s = serde_json::to_string(&c).unwrap();
        assert_eq!(serde_json::from_str::<Config>(&s).unwrap(), c);
    }
}
