//! Settings > Cleanup & AI: the AI rewrite trigger (first group), on-device cleanup options,
//! the AI cleanup provider (also used by the rewrite) and the prompt editor.
use super::keyfield::{KeyField, SecretStore, SysSecrets};
use super::kit::{ToggleDef, apply_toggle, button, choice, group, note, saved, segmented, text_field};
use crate::core::config::{Config, DEFAULT_REWRITE_TRIGGER, FillerMode, LlmProviderId};
use crate::core::rewrite;
use crate::core::secrets::SecretName;
use crate::core::settings_logic::{editor_text, filler_from_index, filler_index, stored_prompt};
use crate::daemon::tray::page::{BtnStyle, ConfirmId, Cx, Effect, Event, FieldKind, Item, ItemId, PageModel, PickerId, Route, TestKind, TestResult};
use crate::llm::registry::{self, LlmProviderInfo};

// AI rewrite
pub const ID_RW_TOGGLE: ItemId = 1;
pub const ID_RW_TRIGGER: ItemId = 2;
pub const ID_RW_RESET: ItemId = 3;
// On-device cleanup
pub const ID_FILLER: ItemId = 10;
pub const ID_FUZZY: ItemId = 11;
pub const ID_LINEBREAKS: ItemId = 12;
// AI cleanup and provider
pub const ID_AI_ENABLE: ItemId = 20;
pub const ID_AI_PROVIDER: ItemId = 21;
pub const ID_AI_MODEL: ItemId = 22;
pub const ID_AI_SUGGEST: ItemId = 23;
pub const ID_BASE_URL: ItemId = 24;
pub const ID_AZ_ENDPOINT: ItemId = 25;
pub const ID_AZ_DEPLOYMENT: ItemId = 26;
pub const ID_AZ_VERSION: ItemId = 27;
pub const ID_AZ_MODEL: ItemId = 28;
pub const ID_BEDROCK_REGION: ItemId = 29;
pub const ID_VERTEX_PROJECT: ItemId = 30;
pub const ID_VERTEX_LOCATION: ItemId = 31;
// Prompt and test
pub const ID_PROMPT: ItemId = 40;
pub const ID_PROMPT_RESET: ItemId = 41;
pub const ID_TEST: ItemId = 42;
pub const KEY_A: ItemId = 100;
pub const KEY_B: ItemId = 110;

/// Longest trigger phrase (the daemon's matcher refuses longer ones too).
pub const TRIGGER_MAX_CHARS: usize = rewrite::MAX_PHRASE_CHARS;

const OPTIONS: &[ToggleDef] = &[
    ToggleDef {
        id: ID_FUZZY,
        label: "Fuzzy dictionary correction",
        desc: "Fix near-misses of your dictionary words, for example \"open whisper\" to \"OpenWhispr\".",
        get: |c| c.fuzzy_dictionary,
        set: |c, v| c.fuzzy_dictionary = v,
    },
    ToggleDef {
        id: ID_LINEBREAKS,
        label: "Spoken line breaks",
        desc: "Turn \"new line\" and \"new paragraph\" into real line breaks.",
        get: |c| c.spoken_line_breaks,
        set: |c, v| c.spoken_line_breaks = v,
    },
    ToggleDef {
        id: ID_AI_ENABLE,
        label: "Enable AI cleanup",
        desc: "Send the transcript (never the audio) to the model below. It adds a moment of latency; a failure falls back to the on-device result.",
        get: |c| c.ai_cleanup.enabled,
        set: |c, v| c.ai_cleanup.enabled = v,
    },
    ToggleDef {
        id: ID_RW_TOGGLE,
        label: "Enable AI rewrite",
        desc: "Uses the AI provider below, even when AI cleanup is off.",
        get: |c| c.ai_cleanup.rewrite_enabled,
        set: |c, v| c.ai_cleanup.rewrite_enabled = v,
    },
];

/// Collapse whitespace, drop control characters and trim: what is stored as the trigger.
pub fn normalize_trigger(s: &str) -> String {
    let cleaned: String = s.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
    cleaned.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Why `phrase` cannot be a trigger (`None` = usable, or empty = the rewrite is off). This is the
/// matcher's own rule ([`rewrite::Trigger::check`]): two or more words, six or more letters, at
/// most 80 characters, after the same filler removal the dictation gets.
pub fn trigger_error(phrase: &str, filler: FillerMode, language: &str) -> Option<String> {
    let p = normalize_trigger(phrase);
    if p.is_empty() {
        return None;
    }
    let clean = |s: &str| {
        let ctx = crate::core::cleanup::CleanupCtx { language, filler_mode: filler, dictionary: &[], snippets: &[], speech_ms: None, fuzzy_dictionary: false, spoken_line_breaks: false };
        crate::core::cleanup::process_pre_llm(s, &ctx)
    };
    rewrite::Trigger::check(&p, clean).err().map(|e| e.message().to_string())
}

pub struct CleanupPage {
    store: Box<dyn SecretStore>,
    provider: LlmProviderId,
    keys: Vec<KeyField>,
    /// The trigger text the user typed that is not stored (refused), shown with its error.
    draft: Option<String>,
    error: Option<String>,
    /// Built-in cleanup prompt for the current language (loaded on enter).
    default_prompt: String,
    busy: bool,
    result: Option<TestResult>,
}

impl CleanupPage {
    pub fn boxed() -> Box<dyn PageModel> {
        Box::new(CleanupPage::new(Box::new(SysSecrets), LlmProviderId::default()))
    }

    pub fn new(store: Box<dyn SecretStore>, provider: LlmProviderId) -> CleanupPage {
        let mut p = CleanupPage { store, provider, keys: Vec::new(), draft: None, error: None, default_prompt: String::new(), busy: false, result: None };
        p.rebuild_keys(provider);
        p
    }

    fn rebuild_keys(&mut self, provider: LlmProviderId) {
        let info = registry::provider(provider);
        let s = self.store.as_ref();
        self.provider = provider;
        self.keys = match provider {
            LlmProviderId::Bedrock => vec![
                KeyField::new(s, SecretName::AWS_ACCESS_KEY_ID, "Access key ID", "").with_hint("Paste your access key ID").with_noun("access key ID"),
                KeyField::new(s, SecretName::AWS_SECRET_ACCESS_KEY, "Secret access key", "").with_hint("Paste your secret access key").with_noun("secret access key"),
            ],
            LlmProviderId::Vertex => vec![KeyField::new(s, SecretName::VERTEX_SERVICE_ACCOUNT_JSON, "Service account JSON", "").with_hint("Paste the contents of the JSON key file").with_noun("service account")],
            LlmProviderId::Custom => vec![KeyField::new(s, info.secret, "API key (optional)", "")],
            _ => vec![KeyField::new(s, info.secret, "API key", info.key_url)],
        };
    }

    fn key_base(i: usize) -> ItemId {
        if i == 0 { KEY_A } else { KEY_B }
    }

    fn invalidate(&mut self) {
        self.busy = false;
        self.result = None;
    }

    fn rewrite_on(cfg: &Config) -> bool {
        cfg.rewrite_trigger_effective().is_some()
    }

    /// Does the cleanup provider have what it needs to answer a request?
    pub fn ai_ready(&self, cfg: &Config) -> bool {
        let a = &cfg.ai_cleanup;
        let keys = self.keys.iter().all(KeyField::has_key);
        let has_model = !cfg.llm_model().trim().is_empty();
        match self.provider {
            LlmProviderId::Custom => !a.custom_base_url.trim().is_empty() && has_model,
            LlmProviderId::AzureOpenai => keys && !a.azure_endpoint.trim().is_empty() && !a.azure_deployment.trim().is_empty(),
            LlmProviderId::Bedrock => keys && !a.bedrock_region.trim().is_empty() && has_model,
            LlmProviderId::Vertex => keys && !a.vertex_project.trim().is_empty() && !a.vertex_location.trim().is_empty() && has_model,
            _ => keys && has_model,
        }
    }

    /// What is missing before the provider can answer, in words for the warning banner (`None`
    /// when [`Self::ai_ready`]).
    fn setup_gap(&self, cfg: &Config) -> Option<&'static str> {
        if self.ai_ready(cfg) {
            return None;
        }
        let keys = self.provider == LlmProviderId::Custom || self.keys.iter().all(KeyField::has_key);
        Some(if keys {
            "The AI provider below is not fully set up (a model or an address is missing). Finish it, or the rewrite pastes your plain text."
        } else {
            "No API key is saved for the AI provider below. Add one, or the rewrite pastes your plain text."
        })
    }

    fn provider_items(&self, cfg: &Config, info: &LlmProviderInfo) -> Vec<Item> {
        let a = &cfg.ai_cleanup;
        let mut v = vec![group("AI provider")];
        v.push(choice(ID_AI_PROVIDER, "Provider", info.name));
        for (i, k) in self.keys.iter().enumerate() {
            v.extend(k.items(Self::key_base(i)));
        }
        match self.provider {
            LlmProviderId::Custom => {
                v.push(note("An OpenAI-compatible chat completions endpoint (LAN or self-hosted)."));
                v.push(text_field(ID_BASE_URL, "Base URL", "http://localhost:11434/v1", &a.custom_base_url));
            }
            LlmProviderId::AzureOpenai => {
                v.push(text_field(ID_AZ_ENDPOINT, "Endpoint", "https://my-resource.openai.azure.com", &a.azure_endpoint));
                v.push(text_field(ID_AZ_DEPLOYMENT, "Deployment", "gpt-5-mini", &a.azure_deployment));
                v.push(text_field(ID_AZ_VERSION, "API version", "2024-10-21", &a.azure_api_version));
                v.push(text_field(ID_AZ_MODEL, "Underlying model (optional)", "gpt-5-mini", &a.azure_model));
                v.push(note("Set the model behind the deployment when its name does not contain it (a deployment called \"cleanup\" serving gpt-5), so the right request parameters are used."));
            }
            LlmProviderId::Bedrock => v.push(text_field(ID_BEDROCK_REGION, "Region", "us-east-1", &a.bedrock_region)),
            LlmProviderId::Vertex => {
                v.push(text_field(ID_VERTEX_PROJECT, "Project ID", "my-gcp-project", &a.vertex_project));
                v.push(text_field(ID_VERTEX_LOCATION, "Location", "us-central1", &a.vertex_location));
            }
            _ => {}
        }
        if self.provider != LlmProviderId::AzureOpenai {
            v.push(text_field(ID_AI_MODEL, "Model", "Model id", &cfg.llm_model()));
            if !info.models.is_empty() {
                v.push(choice(ID_AI_SUGGEST, "Suggested models", "Choose..."));
            }
        }
        v
    }
}

impl PageModel for CleanupPage {
    fn title(&self) -> String {
        "Cleanup & AI".into()
    }

    fn enter(&mut self, cx: &mut Cx) {
        if cx.cfg.ai_cleanup.provider != self.provider {
            self.invalidate();
        }
        self.rebuild_keys(cx.cfg.ai_cleanup.provider);
        let lang = cx.cfg.language.clone();
        self.default_prompt = std::panic::catch_unwind(|| crate::llm::prompt::system_prompt("", &lang, &[])).unwrap_or_default();
    }

    fn items(&self, cx: &Cx) -> Vec<Item> {
        let cfg = &*cx.cfg;
        let a = &cfg.ai_cleanup;
        let toggle = |id| OPTIONS.iter().find(|d| d.id == id).expect("known toggle").item(cfg);
        // The toggle shows the phrase field; the provider and the test matter only while the rewrite
        // can actually fire (toggle on and a phrase set).
        let rewrite_toggle = a.rewrite_enabled;
        let rewrite_live = Self::rewrite_on(cfg);
        let mut v = vec![group("AI rewrite")];
        v.push(note("Say a trigger phrase at the end of a dictation. The text before it is rewritten concisely by your AI provider and pasted instead."));
        v.push(toggle(ID_RW_TOGGLE));
        if rewrite_toggle {
            v.push(Item::Field {
                id: ID_RW_TRIGGER,
                label: "Trigger phrase".into(),
                hint: DEFAULT_REWRITE_TRIGGER.into(),
                value: self.draft.clone().unwrap_or_else(|| cfg.rewrite_trigger.clone()),
                kind: FieldKind::Text,
                error: self.error.clone(),
                max_chars: TRIGGER_MAX_CHARS,
            });
            v.push(button(ID_RW_RESET, "Reset to default", BtnStyle::Link, cfg.rewrite_trigger != DEFAULT_REWRITE_TRIGGER || self.draft.is_some()));
            v.push(note("Pick words you would not say otherwise; leave it empty to turn the rewrite off. Avoid digits and filler words such as \"um\"."));
            if rewrite_live && let Some(gap) = self.setup_gap(cfg) {
                v.push(Item::Banner { tone: crate::daemon::tray::page::Tone::Warn, text: gap.into() });
            }
        }

        v.push(group("On-device cleanup"));
        v.push(note("Always on and instant: runs on your device before anything is pasted."));
        v.push(segmented(ID_FILLER, "Filler word removal", &["Off", "Standard", "Aggressive"], filler_index(cfg.filler_mode)));
        v.push(note("Removes um, uh, stutters and false starts. Aggressive also drops \"basically\", \"actually\" and similar."));
        v.push(toggle(ID_FUZZY));
        v.push(toggle(ID_LINEBREAKS));

        v.push(group("AI cleanup"));
        v.push(note("An optional language model polishes punctuation and grammar after the on-device pass."));
        v.push(toggle(ID_AI_ENABLE));

        if a.enabled || rewrite_live {
            let info = registry::provider(self.provider);
            v.extend(self.provider_items(cfg, info));
        }
        if a.enabled {
            v.push(group("Prompt Studio"));
            let custom = !a.custom_prompt.trim().is_empty();
            v.push(note(if custom { "Using your custom prompt." } else { "Using the built-in prompt. Edit it to change tone or rules." }));
            v.push(Item::Field {
                id: ID_PROMPT,
                label: "Cleanup prompt".into(),
                hint: String::new(),
                value: editor_text(&a.custom_prompt, &self.default_prompt),
                kind: FieldKind::Multiline { rows: 8 },
                error: None,
                max_chars: 0,
            });
            v.push(button(ID_PROMPT_RESET, "Reset to default", BtnStyle::Link, custom));
        }
        if a.enabled || rewrite_live {
            v.push(group("Test"));
            v.push(note("Runs a built-in rough sentence through the model above."));
            v.push(button(ID_TEST, if self.busy { "Running..." } else { "Run on sample" }, BtnStyle::Primary, self.ai_ready(cfg) && !self.busy));
            if let Some(r) = &self.result {
                v.push(Item::Result { ok: r.ok, badge: r.badge.clone(), text: r.text.clone() });
            }
        }
        v
    }

    fn event(&mut self, id: ItemId, ev: Event, cx: &mut Cx) -> Vec<Effect> {
        for i in 0..self.keys.len() {
            let base = Self::key_base(i);
            if KeyField::owns(base, id) {
                let out = self.keys[i].event(base, id, ev);
                if out.changed {
                    self.invalidate();
                }
                return out.effects;
            }
        }
        if apply_toggle(OPTIONS, id, &ev, cx.cfg) {
            if id == ID_AI_ENABLE {
                self.invalidate();
            }
            return saved();
        }
        let provider = self.provider;
        let ai = &mut cx.cfg.ai_cleanup;
        let commit = |dst: &mut String, t: &str| *dst = t.trim().to_string();
        match (id, ev) {
            (ID_RW_TRIGGER, Event::Edited(t)) => {
                self.error = trigger_error(&t, cx.cfg.filler_mode, &cx.cfg.language);
                self.draft = Some(t);
                Vec::new()
            }
            (ID_RW_TRIGGER, Event::Committed(t)) => {
                let norm = normalize_trigger(&t);
                self.error = trigger_error(&norm, cx.cfg.filler_mode, &cx.cfg.language);
                if self.error.is_some() {
                    self.draft = Some(norm);
                    return Vec::new();
                }
                self.draft = None;
                if cx.cfg.rewrite_trigger == norm {
                    return Vec::new();
                }
                cx.cfg.rewrite_trigger = norm;
                saved()
            }
            (ID_RW_RESET, Event::Pressed) => {
                self.draft = None;
                self.error = None;
                cx.cfg.rewrite_trigger = DEFAULT_REWRITE_TRIGGER.into();
                saved()
            }
            (ID_FILLER, Event::Picked(i)) => {
                cx.cfg.filler_mode = filler_from_index(i);
                saved()
            }
            (ID_AI_PROVIDER, Event::Activated) => vec![Effect::Navigate(Route::Picker(PickerId::LlmProvider))],
            (ID_AI_SUGGEST, Event::Activated) => vec![Effect::Navigate(Route::Picker(PickerId::LlmModel))],
            (ID_AI_MODEL, Event::Committed(t)) => {
                let t = t.trim().to_string();
                if t.is_empty() {
                    ai.models.remove(&provider);
                } else {
                    ai.models.insert(provider, t);
                }
                self.invalidate();
                saved()
            }
            (ID_BASE_URL, Event::Committed(t)) => {
                commit(&mut ai.custom_base_url, &t);
                self.invalidate();
                saved()
            }
            (ID_AZ_ENDPOINT, Event::Committed(t)) => {
                commit(&mut ai.azure_endpoint, &t);
                self.invalidate();
                saved()
            }
            (ID_AZ_DEPLOYMENT, Event::Committed(t)) => {
                commit(&mut ai.azure_deployment, &t);
                self.invalidate();
                saved()
            }
            (ID_AZ_VERSION, Event::Committed(t)) => {
                commit(&mut ai.azure_api_version, &t);
                self.invalidate();
                saved()
            }
            (ID_AZ_MODEL, Event::Committed(t)) => {
                commit(&mut ai.azure_model, &t);
                self.invalidate();
                saved()
            }
            (ID_BEDROCK_REGION, Event::Committed(t)) => {
                commit(&mut ai.bedrock_region, &t);
                self.invalidate();
                saved()
            }
            (ID_VERTEX_PROJECT, Event::Committed(t)) => {
                commit(&mut ai.vertex_project, &t);
                self.invalidate();
                saved()
            }
            (ID_VERTEX_LOCATION, Event::Committed(t)) => {
                commit(&mut ai.vertex_location, &t);
                self.invalidate();
                saved()
            }
            (ID_PROMPT, Event::Committed(t)) => {
                let new = stored_prompt(&t, &self.default_prompt);
                if new == ai.custom_prompt {
                    return Vec::new();
                }
                ai.custom_prompt = new;
                saved()
            }
            (ID_PROMPT_RESET, Event::Pressed) => {
                ai.custom_prompt.clear();
                saved()
            }
            (ID_TEST, Event::Pressed) if !self.busy && self.ai_ready(cx.cfg) => {
                self.busy = true;
                self.result = None;
                vec![Effect::Saved, Effect::RunTest(TestKind::Llm)]
            }
            _ => Vec::new(),
        }
    }

    fn confirmed(&mut self, id: ConfirmId, _cx: &mut Cx) -> Vec<Effect> {
        if let ConfirmId::RemoveKey(base) = id {
            for i in 0..self.keys.len() {
                if Self::key_base(i) == base {
                    let out = self.keys[i].remove();
                    self.invalidate();
                    return out.effects;
                }
            }
        }
        Vec::new()
    }

    fn test_done(&mut self, result: TestResult, _cx: &mut Cx) {
        if result.kind == TestKind::Llm {
            self.busy = false;
            self.result = Some(result);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::tray::page::settings::keyfield::tests::Fake;
    use crate::daemon::tray::page::settings::kit::{dump, snapshot};
    use crate::daemon::tray::page::{PopupAction, Tone};

    fn page(provider: LlmProviderId, store: Fake) -> CleanupPage {
        let mut p = CleanupPage::new(Box::new(store), provider);
        p.default_prompt = "DEFAULT PROMPT".into();
        p
    }

    fn snap(cfg: &mut Config, p: &CleanupPage) -> String {
        let cx = Cx::new(cfg, false, 0);
        dump(&p.title(), &p.items(&cx))
    }

    #[test]
    fn page_text_snapshots() {
        let mut cfg = Config::default();
        let p = page(LlmProviderId::Openrouter, Fake::default());
        snapshot!("cleanup_default", snap(&mut cfg, &p));
        cfg.ai_cleanup.enabled = true;
        let p = page(LlmProviderId::Openrouter, Fake::with("openrouter", "sk-or-...abcd"));
        snapshot!("cleanup_ai_on_key_saved", snap(&mut cfg, &p));
        cfg.ai_cleanup.provider = LlmProviderId::AzureOpenai;
        let p = page(LlmProviderId::AzureOpenai, Fake::default());
        snapshot!("cleanup_azure", snap(&mut cfg, &p));
        cfg.ai_cleanup.rewrite_enabled = false;
        cfg.ai_cleanup.enabled = false;
        let p = page(LlmProviderId::AzureOpenai, Fake::default());
        snapshot!("cleanup_everything_off", snap(&mut cfg, &p));
    }

    #[test]
    fn ai_rewrite_is_the_first_group() {
        let mut cfg = Config::default();
        let p = page(LlmProviderId::Openrouter, Fake::default());
        let cx = Cx::new(&mut cfg, false, 0);
        let items = p.items(&cx);
        assert_eq!(items[0], Item::Group("AI rewrite".into()));
        assert!(matches!(items.iter().find_map(Item::id), Some(ID_RW_TOGGLE)), "the toggle is the first control");
    }

    #[test]
    fn rewrite_toggle_saves_and_hides_the_trigger_field() {
        let mut cfg = Config::default();
        let mut p = page(LlmProviderId::Openrouter, Fake::default());
        let mut cx = Cx::new(&mut cfg, false, 0);
        let has_field = |p: &CleanupPage, cx: &Cx| p.items(cx).iter().any(|i| matches!(i, Item::Field { id: ID_RW_TRIGGER, .. }));
        assert!(has_field(&p, &cx));
        assert_eq!(p.event(ID_RW_TOGGLE, Event::Toggled(false), &mut cx), vec![Effect::Saved]);
        assert!(!cx.cfg.ai_cleanup.rewrite_enabled);
        assert!(!has_field(&p, &cx));
        assert_eq!(cx.cfg.rewrite_trigger, DEFAULT_REWRITE_TRIGGER, "the phrase is kept while the toggle is off");
    }

    #[test]
    fn trigger_commit_normalises_validates_and_saves() {
        let mut cfg = Config::default();
        let mut p = page(LlmProviderId::Openrouter, Fake::default());
        let mut cx = Cx::new(&mut cfg, false, 0);
        let fx = p.event(ID_RW_TRIGGER, Event::Committed("  make   this\tshort  ".into()), &mut cx);
        assert_eq!(fx, vec![Effect::Saved]);
        assert_eq!(cx.cfg.rewrite_trigger, "make this short");
        assert!(p.error.is_none() && p.draft.is_none());
        // Same text again: nothing to save.
        assert!(p.event(ID_RW_TRIGGER, Event::Committed("make this short".into()), &mut cx).is_empty());
    }

    #[test]
    fn invalid_triggers_are_refused_with_a_reason_and_the_draft_is_kept() {
        let mut cfg = Config::default();
        let mut p = page(LlmProviderId::Openrouter, Fake::default());
        let mut cx = Cx::new(&mut cfg, false, 0);
        for (bad, why) in [("rewrite", "at least two words"), ("a b c", "longer phrase"), (&"word ".repeat(30), "at most 80")] {
            assert!(p.event(ID_RW_TRIGGER, Event::Committed(bad.to_string()), &mut cx).is_empty(), "{bad}");
            assert_eq!(cx.cfg.rewrite_trigger, DEFAULT_REWRITE_TRIGGER, "an invalid phrase never replaces the stored one");
            assert!(p.error.as_deref().is_some_and(|e| e.contains(why)), "{bad}: {:?}", p.error);
            let items = p.items(&cx);
            let Some(Item::Field { value, error, .. }) = items.iter().find(|i| matches!(i, Item::Field { id: ID_RW_TRIGGER, .. })) else { panic!() };
            assert_eq!(value, &normalize_trigger(bad), "the draft stays visible next to its error");
            assert!(error.is_some());
        }
        // Typing a valid phrase clears the error live; committing saves.
        p.event(ID_RW_TRIGGER, Event::Edited("tighten this up".into()), &mut cx);
        assert!(p.error.is_none());
        assert_eq!(p.event(ID_RW_TRIGGER, Event::Committed("tighten this up".into()), &mut cx), vec![Effect::Saved]);
        assert_eq!(cx.cfg.rewrite_trigger, "tighten this up");
    }

    #[test]
    fn an_empty_trigger_turns_the_rewrite_off_and_is_kept_as_written() {
        let mut cfg = Config::default();
        let mut p = page(LlmProviderId::Openrouter, Fake::default());
        let mut cx = Cx::new(&mut cfg, false, 0);
        assert_eq!(p.event(ID_RW_TRIGGER, Event::Committed("   ".into()), &mut cx), vec![Effect::Saved]);
        assert_eq!(cx.cfg.rewrite_trigger, "");
        assert!(cx.cfg.rewrite_trigger_effective().is_none());
        assert!(p.error.is_none());
        // Reset brings the default back.
        assert_eq!(p.event(ID_RW_RESET, Event::Pressed, &mut cx), vec![Effect::Saved]);
        assert_eq!(cx.cfg.rewrite_trigger, DEFAULT_REWRITE_TRIGGER);
        assert!(cx.cfg.rewrite_trigger_effective().is_some());
    }

    #[test]
    fn reset_is_offered_only_when_the_phrase_differs() {
        let mut cfg = Config::default();
        let mut p = page(LlmProviderId::Openrouter, Fake::default());
        let reset_enabled = |p: &CleanupPage, cfg: &mut Config| {
            let cx = Cx::new(cfg, false, 0);
            p.items(&cx).iter().find_map(|i| match i {
                Item::Button { id: ID_RW_RESET, enabled, .. } => Some(*enabled),
                _ => None,
            })
        };
        assert_eq!(reset_enabled(&p, &mut cfg), Some(false));
        cfg.rewrite_trigger = "shorten this".into();
        assert_eq!(reset_enabled(&p, &mut cfg), Some(true));
        cfg.rewrite_trigger = DEFAULT_REWRITE_TRIGGER.into();
        p.draft = Some("x y".into());
        assert_eq!(reset_enabled(&p, &mut cfg), Some(true), "a refused draft can be reset too");
    }

    #[test]
    fn warning_banner_follows_the_provider_state() {
        let banner = |p: &CleanupPage, cfg: &mut Config| {
            let cx = Cx::new(cfg, false, 0);
            p.items(&cx).iter().any(|i| matches!(i, Item::Banner { tone: Tone::Warn, .. }))
        };
        let mut cfg = Config::default();
        let p = page(LlmProviderId::Openrouter, Fake::default());
        assert!(banner(&p, &mut cfg), "no key: warn, even though AI cleanup is off");
        let p = page(LlmProviderId::Openrouter, Fake::with("openrouter", "sk-...abcd"));
        assert!(!banner(&p, &mut cfg), "key + default model: ready");
        let mut p = page(LlmProviderId::Openrouter, Fake::with("openrouter", "sk-...abcd"));
        {
            let mut cx = Cx::new(&mut cfg, false, 0);
            p.event(ID_RW_TRIGGER, Event::Committed("".into()), &mut cx);
        }
        assert!(!banner(&p, &mut cfg), "no trigger, no rewrite, no warning");
        cfg.rewrite_trigger = DEFAULT_REWRITE_TRIGGER.into();
        cfg.ai_cleanup.rewrite_enabled = false;
        let p = page(LlmProviderId::Openrouter, Fake::default());
        assert!(!banner(&p, &mut cfg), "rewrite toggled off");
    }

    #[test]
    fn the_banner_names_what_is_missing() {
        let text = |p: &CleanupPage, cfg: &mut Config| {
            let cx = Cx::new(cfg, false, 0);
            p.items(&cx).iter().find_map(|i| match i {
                Item::Banner { text, .. } => Some(text.clone()),
                _ => None,
            })
        };
        let mut cfg = Config::default();
        let p = page(LlmProviderId::Openrouter, Fake::default());
        assert!(text(&p, &mut cfg).is_some_and(|t| t.contains("No API key")), "the key is what is missing, not the model");
        cfg.ai_cleanup.models.insert(LlmProviderId::Openrouter, "  ".into());
        let p = page(LlmProviderId::Openrouter, Fake::with("openrouter", "sk-...abcd"));
        assert!(text(&p, &mut cfg).is_some_and(|t| t.contains("not fully set up")), "a key but no model");
    }

    #[test]
    fn the_provider_group_follows_a_rewrite_that_can_fire() {
        let groups = |p: &CleanupPage, cfg: &mut Config| {
            let cx = Cx::new(cfg, false, 0);
            p.items(&cx).iter().filter_map(|i| if let Item::Group(g) = i { Some(g.clone()) } else { None }).collect::<Vec<_>>()
        };
        let p = page(LlmProviderId::Openrouter, Fake::default());
        let mut cfg = Config::default();
        assert!(groups(&p, &mut cfg).contains(&"AI provider".to_string()), "the rewrite reuses the provider even with AI cleanup off");
        cfg.rewrite_trigger.clear();
        let g = groups(&p, &mut cfg);
        assert!(!g.contains(&"AI provider".to_string()) && !g.contains(&"Test".to_string()), "no phrase, no rewrite, no provider needed: {g:?}");
        cfg.ai_cleanup.enabled = true;
        assert!(groups(&p, &mut cfg).contains(&"AI provider".to_string()), "AI cleanup needs it");
    }

    #[test]
    fn readiness_per_provider() {
        let mut cfg = Config::default();
        // Custom needs a URL and a model; the key is optional.
        let p = page(LlmProviderId::Custom, Fake::default());
        cfg.ai_cleanup.provider = LlmProviderId::Custom;
        assert!(!p.ai_ready(&cfg) || p.keys.iter().all(KeyField::has_key));
        cfg.ai_cleanup.custom_base_url = "http://localhost:11434/v1".into();
        cfg.ai_cleanup.models.insert(LlmProviderId::Custom, "llama".into());
        let p2 = page(LlmProviderId::Custom, Fake::with("custom_llm", "x"));
        assert!(p2.ai_ready(&cfg));
        // Azure: key + endpoint + deployment, no model field.
        let p = page(LlmProviderId::AzureOpenai, Fake::with("azure_openai", "k..y"));
        assert!(!p.ai_ready(&cfg));
        cfg.ai_cleanup.azure_endpoint = "https://x.openai.azure.com".into();
        cfg.ai_cleanup.azure_deployment = "dep".into();
        assert!(p.ai_ready(&cfg));
        // Bedrock needs both keys.
        let store = Fake::with("aws_access_key_id", "AKIA...");
        let p = page(LlmProviderId::Bedrock, store);
        assert!(!p.ai_ready(&cfg));
    }

    #[test]
    fn filler_segmented_maps_modes() {
        let mut cfg = Config::default();
        let mut p = page(LlmProviderId::Openrouter, Fake::default());
        let mut cx = Cx::new(&mut cfg, false, 0);
        for (i, m) in [(0, FillerMode::Off), (1, FillerMode::Standard), (2, FillerMode::Aggressive)] {
            assert_eq!(p.event(ID_FILLER, Event::Picked(i), &mut cx), vec![Effect::Saved]);
            assert_eq!(cx.cfg.filler_mode, m);
        }
    }

    #[test]
    fn option_toggles_save_their_fields() {
        let mut cfg = Config::default();
        let mut p = page(LlmProviderId::Openrouter, Fake::default());
        let mut cx = Cx::new(&mut cfg, false, 0);
        assert_eq!(p.event(ID_FUZZY, Event::Toggled(false), &mut cx), vec![Effect::Saved]);
        assert_eq!(p.event(ID_LINEBREAKS, Event::Toggled(true), &mut cx), vec![Effect::Saved]);
        assert_eq!(p.event(ID_AI_ENABLE, Event::Toggled(true), &mut cx), vec![Effect::Saved]);
        assert!(!cx.cfg.fuzzy_dictionary && cx.cfg.spoken_line_breaks && cx.cfg.ai_cleanup.enabled);
    }

    #[test]
    fn provider_section_appears_when_cleanup_or_rewrite_needs_it() {
        let has = |cfg: &mut Config| {
            let p = page(LlmProviderId::Openrouter, Fake::default());
            let cx = Cx::new(cfg, false, 0);
            p.items(&cx).iter().any(|i| matches!(i, Item::Group(g) if g == "AI provider"))
        };
        let mut cfg = Config::default();
        assert!(has(&mut cfg), "rewrite is on by default and needs a provider");
        cfg.ai_cleanup.rewrite_enabled = false;
        assert!(!has(&mut cfg));
        cfg.ai_cleanup.enabled = true;
        assert!(has(&mut cfg));
    }

    #[test]
    fn prompt_editor_stores_only_real_changes() {
        let mut cfg = Config::default();
        cfg.ai_cleanup.enabled = true;
        let mut p = page(LlmProviderId::Openrouter, Fake::default());
        let mut cx = Cx::new(&mut cfg, false, 0);
        assert!(p.event(ID_PROMPT, Event::Committed("DEFAULT PROMPT\n".into()), &mut cx).is_empty(), "the default text is not a custom prompt");
        assert_eq!(p.event(ID_PROMPT, Event::Committed("Be terse.".into()), &mut cx), vec![Effect::Saved]);
        assert_eq!(cx.cfg.ai_cleanup.custom_prompt, "Be terse.");
        let shown = p.items(&cx).iter().find_map(|i| match i {
            Item::Field { id: ID_PROMPT, value, .. } => Some(value.clone()),
            _ => None,
        });
        assert_eq!(shown.as_deref(), Some("Be terse."));
        assert_eq!(p.event(ID_PROMPT_RESET, Event::Pressed, &mut cx), vec![Effect::Saved]);
        assert_eq!(cx.cfg.ai_cleanup.custom_prompt, "");
    }

    #[test]
    fn model_field_sets_the_per_provider_model_and_empty_restores_the_default() {
        let mut cfg = Config::default();
        let mut p = page(LlmProviderId::Openrouter, Fake::default());
        let mut cx = Cx::new(&mut cfg, false, 0);
        assert_eq!(p.event(ID_AI_MODEL, Event::Committed(" meta/llama ".into()), &mut cx), vec![Effect::Saved]);
        assert_eq!(cx.cfg.llm_model(), "meta/llama");
        p.event(ID_AI_MODEL, Event::Committed("".into()), &mut cx);
        assert_eq!(cx.cfg.llm_model(), registry::default_model(LlmProviderId::Openrouter));
        assert_eq!(p.event(ID_AI_SUGGEST, Event::Activated, &mut cx), vec![Effect::Navigate(Route::Picker(PickerId::LlmModel))]);
        assert_eq!(p.event(ID_AI_PROVIDER, Event::Activated, &mut cx), vec![Effect::Navigate(Route::Picker(PickerId::LlmProvider))]);
    }

    #[test]
    fn provider_specific_fields_commit_to_their_config_fields() {
        let mut cfg = Config::default();
        let mut p = page(LlmProviderId::Openrouter, Fake::default());
        let mut cx = Cx::new(&mut cfg, false, 0);
        type Getter = fn(&Config) -> String;
        let cases: [(ItemId, Getter); 8] = [
            (ID_BASE_URL, |c| c.ai_cleanup.custom_base_url.clone()),
            (ID_AZ_ENDPOINT, |c| c.ai_cleanup.azure_endpoint.clone()),
            (ID_AZ_DEPLOYMENT, |c| c.ai_cleanup.azure_deployment.clone()),
            (ID_AZ_VERSION, |c| c.ai_cleanup.azure_api_version.clone()),
            (ID_AZ_MODEL, |c| c.ai_cleanup.azure_model.clone()),
            (ID_BEDROCK_REGION, |c| c.ai_cleanup.bedrock_region.clone()),
            (ID_VERTEX_PROJECT, |c| c.ai_cleanup.vertex_project.clone()),
            (ID_VERTEX_LOCATION, |c| c.ai_cleanup.vertex_location.clone()),
        ];
        for (id, get) in cases {
            assert_eq!(p.event(id, Event::Committed(format!("  v{id}  ")), &mut cx), vec![Effect::Saved], "{id}");
            assert_eq!(get(cx.cfg), format!("v{id}"));
        }
    }

    #[test]
    fn llm_test_runs_on_a_saved_config_and_shows_its_result() {
        let mut cfg = Config::default();
        cfg.ai_cleanup.enabled = true;
        let mut p = page(LlmProviderId::Openrouter, Fake::with("openrouter", "sk-...abcd"));
        let mut cx = Cx::new(&mut cfg, false, 0);
        assert_eq!(p.event(ID_TEST, Event::Pressed, &mut cx), vec![Effect::Saved, Effect::RunTest(TestKind::Llm)]);
        assert!(p.event(ID_TEST, Event::Pressed, &mut cx).is_empty());
        p.test_done(TestResult { kind: TestKind::Stt, ok: true, badge: "x".into(), text: "ignored".into() }, &mut cx);
        assert!(p.busy, "an STT result is not ours");
        p.test_done(TestResult { kind: TestKind::Llm, ok: true, badge: "420 ms".into(), text: "Ship the release on Friday.".into() }, &mut cx);
        assert!(!p.busy);
        assert!(p.items(&cx).iter().any(|i| matches!(i, Item::Result { ok: true, .. })));
        // Removing the key invalidates it.
        let fx = p.event(KEY_A + 6, Event::Pressed, &mut cx);
        assert!(matches!(&fx[0], Effect::Confirm { .. }));
        let fx = p.confirmed(ConfirmId::RemoveKey(KEY_A), &mut cx);
        assert_eq!(fx[0], Effect::Do(PopupAction::SetSecret { name: "openrouter".into(), value: None }));
        assert!(p.result.is_none());
    }

    #[test]
    fn trigger_validation_rules() {
        use FillerMode::Standard as S;
        assert_eq!(trigger_error("", S, "auto"), None, "empty turns the rewrite off");
        assert!(trigger_error("rewrite", S, "auto").unwrap().contains("two words"));
        assert!(trigger_error("ab cd", S, "auto").unwrap().contains("six letters"));
        assert!(trigger_error(&"x".repeat(81), S, "auto").unwrap().contains("80"));
        assert_eq!(trigger_error("rewrite this concisely", S, "auto"), None);
        assert_eq!(trigger_error(DEFAULT_REWRITE_TRIGGER, S, "auto"), None, "the default phrase is usable");
        assert_eq!(trigger_error("请用简洁的方式改写", S, "auto"), None, "CJK characters count as words");
        assert_eq!(trigger_error("re-write this", S, "auto"), None);
        // Filler removal runs on the phrase as on the dictation.
        assert!(trigger_error("um uh", S, "auto").is_some());
        // Exactly the matcher's verdict.
        for t in ["rewrite this concisely", "make it short", "um, rewrite", "ab cd", "please shorten this"] {
            let lang = "auto";
            let clean = |s: &str| {
                let ctx = crate::core::cleanup::CleanupCtx { language: lang, filler_mode: S, dictionary: &[], snippets: &[], speech_ms: None, fuzzy_dictionary: false, spoken_line_breaks: false };
                crate::core::cleanup::process_pre_llm(s, &ctx)
            };
            assert_eq!(trigger_error(t, S, lang).is_none(), rewrite::Trigger::new(t, clean).is_some(), "{t}");
        }
    }

    #[test]
    fn normalize_trigger_collapses_and_strips() {
        assert_eq!(normalize_trigger("  a\u{0}b \n c\t"), "a b c");
        assert_eq!(normalize_trigger(""), "");
    }

    #[test]
    fn every_llm_provider_builds_a_page_with_unique_ids() {
        for pi in registry::PROVIDERS {
            let mut cfg = Config::default();
            cfg.ai_cleanup.enabled = true;
            cfg.ai_cleanup.provider = pi.id;
            let p = page(pi.id, Fake::default());
            let cx = Cx::new(&mut cfg, false, 0);
            let mut ids: Vec<ItemId> = p.items(&cx).iter().filter_map(Item::id).collect();
            let n = ids.len();
            ids.sort_unstable();
            ids.dedup();
            assert_eq!(ids.len(), n, "{:?}", pi.id);
        }
    }
}
