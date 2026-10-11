//! Settings > Speech-to-Text > Bring your own key: provider (picker), API key(s), provider
//! specifics, model, transport and the Test button (the embedded sample through the active
//! provider, on a worker). While a local model is in use, a line at the top switches back.
use super::keyfield::{KeyField, SecretStore, SysSecrets};
use super::kit::{choice, group, note, radio, saved, segmented, text_field};
use crate::core::config::{SttEngine, SttProviderId};
use crate::core::secrets::SecretName;
use crate::daemon::tray::page::{ConfirmId, Cx, Effect, Event, Icon, Item, ItemId, PageModel, PickerId, Route, TestKind, TestResult, Tool, ToolTone};
use crate::stt::registry::{self, ProviderInfo};

pub const ID_PROVIDER: ItemId = 1;
pub const ID_USE_CLOUD: ItemId = 2;
pub const ID_REGION: ItemId = 20;
pub const ID_TENANT: ItemId = 21;
pub const ID_CUSTOM_URL: ItemId = 22;
pub const ID_CUSTOM_MODEL: ItemId = 23;
pub const ID_STREAMING: ItemId = 30;
pub const ID_TEST: ItemId = 31;
/// Model radios are `ID_MODEL0 + index in the provider's model list`.
pub const ID_MODEL0: ItemId = 40;
/// First key field (the API key, or the Corti client id); a second one follows for Corti.
pub const KEY_A: ItemId = 100;
pub const KEY_B: ItemId = 110;

pub struct SttPage {
    store: Box<dyn SecretStore>,
    /// Provider the key fields were built for.
    provider: SttProviderId,
    keys: Vec<KeyField>,
    busy: bool,
    result: Option<TestResult>,
}

impl SttPage {
    pub fn boxed() -> Box<dyn PageModel> {
        Box::new(SttPage::new(Box::new(SysSecrets), SttProviderId::default()))
    }

    pub fn new(store: Box<dyn SecretStore>, provider: SttProviderId) -> SttPage {
        let mut p = SttPage { store, provider, keys: Vec::new(), busy: false, result: None };
        p.rebuild_keys(provider);
        p
    }

    fn rebuild_keys(&mut self, provider: SttProviderId) {
        let info = registry::provider(provider);
        self.provider = provider;
        self.keys = match provider {
            SttProviderId::Corti => vec![
                KeyField::new(self.store.as_ref(), SecretName::CORTI_CLIENT_ID, "Client ID", info.key_url).with_hint("Paste your client ID").with_noun("client ID"),
                KeyField::new(self.store.as_ref(), SecretName::CORTI_CLIENT_SECRET, "Client secret", "").with_hint("Paste your client secret").with_noun("client secret"),
            ],
            SttProviderId::Custom => vec![KeyField::new(self.store.as_ref(), info.secret, "API key (optional)", "")],
            _ => vec![KeyField::new(self.store.as_ref(), info.secret, "API key", info.key_url)],
        };
    }

    fn invalidate(&mut self) {
        self.busy = false;
        self.result = None;
    }

    fn key_base(i: usize) -> ItemId {
        if i == 0 { KEY_A } else { KEY_B }
    }

    /// Can the test run: every required key is present (a custom endpoint needs a URL instead).
    fn ready(&self, cx: &Cx) -> bool {
        match self.provider {
            SttProviderId::Custom => !cx.cfg.stt.custom_base_url.trim().is_empty(),
            SttProviderId::Corti => self.keys.iter().all(KeyField::has_key),
            _ => self.keys.iter().all(KeyField::has_key),
        }
    }

    fn model_radios(&self, info: &ProviderInfo, active: &str) -> Vec<Item> {
        info.models
            .iter()
            .enumerate()
            .map(|(i, m)| {
                let mut tags: Vec<&str> = Vec::new();
                if i == 0 {
                    tags.push("Recommended");
                }
                if m.streaming_only {
                    tags.push("Streaming only");
                } else if m.streaming {
                    tags.push("Streaming");
                }
                // tags first: the line is cut to fit, and they matter most
                let desc = if tags.is_empty() { m.description.to_string() } else { format!("{} · {}", tags.join(" · "), m.description) };
                radio(ID_MODEL0 + i as ItemId, m.name, &desc, m.id == active)
            })
            .collect()
    }
}

impl PageModel for SttPage {
    fn title(&self) -> String {
        "Bring your own key".into()
    }

    fn enter(&mut self, cx: &mut Cx) {
        let provider = cx.cfg.stt.provider;
        if provider != self.provider {
            self.invalidate();
        }
        // Rebuild: re-reads every key (it may have changed on another page or in the environment).
        self.rebuild_keys(provider);
    }

    fn items(&self, cx: &Cx) -> Vec<Item> {
        let cfg = &*cx.cfg;
        let provider = cfg.stt.provider;
        let info = registry::provider(provider);
        let local = cfg.stt.engine == SttEngine::Local;
        let mut v = Vec::new();
        if local {
            v.push(Item::Tools {
                id: ID_USE_CLOUD,
                label: "A local model is in use".into(),
                sub: crate::stt::local::display_name(cfg),
                tools: vec![Tool::new(Icon::Check, "Use the cloud provider", ToolTone::Primary, true)],
            });
        }
        v.push(choice(ID_PROVIDER, "Provider", info.name));
        for (i, k) in self.keys.iter().enumerate() {
            v.extend(k.items(Self::key_base(i)));
        }
        if !info.note.is_empty() && provider != SttProviderId::Custom {
            v.push(note(info.note));
        }
        match provider {
            SttProviderId::Custom => {
                v.push(group("Custom endpoint"));
                v.push(note("Any OpenAI-compatible /audio/transcriptions server (Azure OpenAI too)."));
                v.push(text_field(ID_CUSTOM_URL, "Base URL", "https://api.example.com/v1", &cfg.stt.custom_base_url));
                v.push(text_field(ID_CUSTOM_MODEL, "Model", "whisper-1", &cfg.stt.custom_model));
            }
            SttProviderId::Corti => {
                v.push(group("Corti environment"));
                v.push(segmented(ID_REGION, "Region", &["US", "EU"], usize::from(cfg.stt.corti_environment == "eu")));
                v.push(text_field(ID_TENANT, "Tenant", "base", &cfg.stt.corti_tenant));
            }
            _ => {}
        }
        if !info.models.is_empty() {
            v.push(group("Model"));
            v.extend(self.model_radios(info, &cfg.stt_model()));
        }
        if info.models.iter().any(|m| m.streaming) {
            v.push(super::kit::toggle(ID_STREAMING, "Prefer streaming", "Text is ready sooner", cfg.stt.prefer_streaming));
        }
        v.push(group("Test"));
        let ready = self.ready(cx) && !local;
        let sub = if local {
            "Switch to the cloud provider first"
        } else if ready {
            "Transcribes a short sample"
        } else {
            match provider {
                SttProviderId::Custom => "Enter the base URL first",
                SttProviderId::Corti => "Save the client ID and secret first",
                _ => "Save an API key first",
            }
        };
        v.push(super::kit::test_line(ID_TEST, "Test transcription", sub, self.busy, ready));
        if let Some(r) = &self.result {
            v.push(Item::Result { ok: r.ok, badge: r.badge.clone(), text: r.text.clone() });
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
        let provider = cx.cfg.stt.provider;
        match (id, ev) {
            (ID_PROVIDER, Event::Activated) => vec![Effect::Navigate(Route::Picker(PickerId::SttProvider))],
            (ID_USE_CLOUD, Event::Pressed | Event::Activated | Event::Picked(0)) if cx.cfg.stt.engine == SttEngine::Local => {
                cx.cfg.stt.engine = SttEngine::Cloud;
                self.invalidate();
                saved()
            }
            (ID_REGION, Event::Picked(i)) => {
                cx.cfg.stt.corti_environment = if i == 1 { "eu" } else { "us" }.into();
                self.invalidate();
                saved()
            }
            (ID_TENANT, Event::Committed(t)) => {
                cx.cfg.stt.corti_tenant = t.trim().to_string();
                self.invalidate();
                saved()
            }
            (ID_CUSTOM_URL, Event::Committed(t)) => {
                cx.cfg.stt.custom_base_url = t.trim().to_string();
                self.invalidate();
                saved()
            }
            (ID_CUSTOM_MODEL, Event::Committed(t)) => {
                cx.cfg.stt.custom_model = t.trim().to_string();
                self.invalidate();
                saved()
            }
            (ID_STREAMING, Event::Toggled(v)) => {
                cx.cfg.stt.prefer_streaming = v;
                saved()
            }
            (m, Event::Toggled(true) | Event::Activated) if (ID_MODEL0..ID_MODEL0 + 16).contains(&m) => {
                let info = registry::provider(provider);
                let Some(model) = info.models.get((m - ID_MODEL0) as usize) else { return Vec::new() };
                if cx.cfg.stt_model() == model.id {
                    return Vec::new();
                }
                cx.cfg.stt.models.insert(provider, model.id.to_string());
                self.invalidate();
                saved()
            }
            (ID_TEST, Event::Pressed | Event::Picked(0)) if self.ready(cx) && !self.busy && cx.cfg.stt.engine == SttEngine::Cloud => {
                self.busy = true;
                self.result = None;
                // Saved first: the worker tests the config the daemon holds, which must be this one.
                vec![Effect::Saved, Effect::RunTest(TestKind::Stt)]
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
        if result.kind == TestKind::Stt {
            self.busy = false;
            self.result = Some(result);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::config::Config;
    use crate::daemon::tray::page::PopupAction;
    use crate::daemon::tray::page::settings::keyfield::tests::Fake;
    use crate::daemon::tray::page::settings::kit::{dump, snapshot};

    fn page(cfg: &Config, store: Fake) -> SttPage {
        SttPage::new(Box::new(store), cfg.stt.provider)
    }

    fn snap(name: &str, cfg: &mut Config, store: Fake) -> String {
        let p = page(cfg, store);
        let cx = Cx::new(cfg, 0);
        let _ = name;
        dump(&p.title(), &p.items(&cx))
    }

    #[test]
    fn a_local_model_in_use_offers_the_way_back() {
        let mut cfg = Config::default();
        let p = page(&cfg, Fake::with("openai", "sk-...abcd"));
        assert!(!p.items(&Cx::new(&mut cfg, 0)).iter().any(|i| i.id() == Some(ID_USE_CLOUD)), "cloud in use: no switch");
        cfg.stt.engine = SttEngine::Local;
        cfg.stt.local_model = "moonshine-tiny".into();
        let mut p = page(&cfg, Fake::with("openai", "sk-...abcd"));
        let mut cx = Cx::new(&mut cfg, 0);
        let items = p.items(&cx);
        let Some(Item::Tools { sub, .. }) = items.first() else { panic!("{items:?}") };
        assert_eq!(sub, "Moonshine Tiny (local)");
        assert_eq!(p.event(ID_TEST, Event::Pressed, &mut cx), vec![], "the cloud test waits for the switch");
        assert_eq!(p.event(ID_USE_CLOUD, Event::Picked(0), &mut cx), vec![Effect::Saved]);
        assert_eq!(cx.cfg.stt.engine, SttEngine::Cloud);
        assert_eq!(cx.cfg.stt.local_model, "moonshine-tiny", "the local choice is remembered");
    }

    #[test]
    fn page_text_snapshots() {
        let mut cfg = Config::default();
        snapshot!("stt_openai_no_key", snap("a", &mut cfg, Fake::default()));
        snapshot!("stt_openai_key_saved", snap("b", &mut cfg, Fake::with("openai", "sk-...wxyz")));
        cfg.stt.provider = SttProviderId::Corti;
        snapshot!("stt_corti", snap("c", &mut cfg, Fake::default()));
        cfg.stt.provider = SttProviderId::Custom;
        snapshot!("stt_custom", snap("d", &mut cfg, Fake::default()));
    }

    #[test]
    fn provider_choice_opens_the_picker() {
        let mut cfg = Config::default();
        let mut p = page(&cfg, Fake::default());
        let mut cx = Cx::new(&mut cfg, 0);
        assert_eq!(p.event(ID_PROVIDER, Event::Activated, &mut cx), vec![Effect::Navigate(Route::Picker(PickerId::SttProvider))]);
    }

    #[test]
    fn model_radios_select_exactly_one_and_save() {
        let mut cfg = Config::default();
        let mut p = page(&cfg, Fake::default());
        let mut cx = Cx::new(&mut cfg, 0);
        let on = |p: &SttPage, cx: &Cx| p.items(cx).iter().filter_map(|i| match i {
            Item::Radio { id, on: true, .. } => Some(*id),
            _ => None,
        }).collect::<Vec<_>>();
        assert_eq!(on(&p, &cx), vec![ID_MODEL0], "the first model is the default");
        assert_eq!(p.event(ID_MODEL0 + 3, Event::Toggled(true), &mut cx), vec![Effect::Saved]);
        assert_eq!(cx.cfg.stt.models.get(&SttProviderId::Openai).map(String::as_str), Some("whisper-1"));
        assert_eq!(on(&p, &cx), vec![ID_MODEL0 + 3]);
        assert!(p.event(ID_MODEL0 + 3, Event::Activated, &mut cx).is_empty(), "already selected");
        assert!(p.event(ID_MODEL0 + 15, Event::Activated, &mut cx).is_empty(), "beyond the list");
    }

    #[test]
    fn streaming_toggle_only_shows_when_a_model_streams() {
        let mut cfg = Config::default();
        let mut p = page(&cfg, Fake::default());
        let mut cx = Cx::new(&mut cfg, 0);
        assert!(p.items(&cx).iter().any(|i| matches!(i, Item::Toggle { id: ID_STREAMING, .. })));
        assert_eq!(p.event(ID_STREAMING, Event::Toggled(true), &mut cx), vec![Effect::Saved]);
        assert!(cx.cfg.stt.prefer_streaming);
        cx.cfg.stt.provider = SttProviderId::Custom;
        p.enter(&mut cx);
        assert!(!p.items(&cx).iter().any(|i| matches!(i, Item::Toggle { id: ID_STREAMING, .. })));
    }

    #[test]
    fn corti_shows_two_credentials_region_and_tenant() {
        let mut cfg = Config::default();
        cfg.stt.provider = SttProviderId::Corti;
        let mut p = page(&cfg, Fake::default());
        let mut cx = Cx::new(&mut cfg, 0);
        let items = p.items(&cx);
        let secrets = items.iter().filter(|i| matches!(i, Item::Field { kind: crate::daemon::tray::page::FieldKind::Secret, .. })).count();
        assert_eq!(secrets, 2);
        assert_eq!(p.event(ID_REGION, Event::Picked(1), &mut cx), vec![Effect::Saved]);
        assert_eq!(cx.cfg.stt.corti_environment, "eu");
        assert_eq!(p.event(ID_REGION, Event::Picked(0), &mut cx), vec![Effect::Saved]);
        assert_eq!(cx.cfg.stt.corti_environment, "us");
        assert_eq!(p.event(ID_TENANT, Event::Committed("  acme ".into()), &mut cx), vec![Effect::Saved]);
        assert_eq!(cx.cfg.stt.corti_tenant, "acme");
    }

    #[test]
    fn custom_endpoint_fields_commit_trimmed() {
        let mut cfg = Config::default();
        cfg.stt.provider = SttProviderId::Custom;
        let mut p = page(&cfg, Fake::default());
        let mut cx = Cx::new(&mut cfg, 0);
        assert!(p.event(ID_CUSTOM_URL, Event::Edited("http://x".into()), &mut cx).is_empty(), "typing alone saves nothing");
        assert_eq!(p.event(ID_CUSTOM_URL, Event::Committed(" http://localhost:8000/v1 ".into()), &mut cx), vec![Effect::Saved]);
        assert_eq!(cx.cfg.stt.custom_base_url, "http://localhost:8000/v1");
        assert_eq!(p.event(ID_CUSTOM_MODEL, Event::Committed("whisper-large".into()), &mut cx), vec![Effect::Saved]);
        assert_eq!(cx.cfg.stt.custom_model, "whisper-large");
    }

    #[test]
    fn test_button_needs_a_key_then_runs_and_shows_the_result() {
        let mut cfg = Config::default();
        let mut p = page(&cfg, Fake::default());
        let mut cx = Cx::new(&mut cfg, 0);
        let enabled = |p: &SttPage, cx: &Cx| p.items(cx).iter().find_map(|i| match i {
            Item::Tools { id: ID_TEST, tools, .. } => Some(tools[0].enabled),
            _ => None,
        });
        assert_eq!(enabled(&p, &cx), Some(false));
        assert!(p.event(ID_TEST, Event::Pressed, &mut cx).is_empty(), "no key: pressing does nothing");
        // Save a key through the field.
        p.event(KEY_A, Event::Edited("sk-test-abcdefghij".into()), &mut cx);
        let fx = p.event(KEY_A + 1, Event::Pressed, &mut cx);
        assert!(matches!(&fx[0], Effect::Do(PopupAction::SetSecret { value: Some(_), .. })));
        assert_eq!(enabled(&p, &cx), Some(true));
        let fx = p.event(ID_TEST, Event::Pressed, &mut cx);
        assert_eq!(fx, vec![Effect::Saved, Effect::RunTest(TestKind::Stt)], "config is saved before the worker reads it");
        assert_eq!(enabled(&p, &cx), Some(false), "busy");
        assert!(p.event(ID_TEST, Event::Pressed, &mut cx).is_empty(), "no second run while one is in flight");
        p.test_done(TestResult { kind: TestKind::Stt, ok: true, badge: "Transcribed in 840 ms".into(), text: "Hey, can you send the report?".into() }, &mut cx);
        assert_eq!(enabled(&p, &cx), Some(true));
        assert!(p.items(&cx).iter().any(|i| matches!(i, Item::Result { ok: true, .. })));
    }

    #[test]
    fn a_changed_setting_discards_the_old_result_and_busy_state() {
        let mut cfg = Config::default();
        let mut p = page(&cfg, Fake::with("openai", "sk-...abcd"));
        let mut cx = Cx::new(&mut cfg, 0);
        p.event(ID_TEST, Event::Pressed, &mut cx);
        assert!(p.busy);
        p.event(ID_MODEL0 + 1, Event::Activated, &mut cx);
        assert!(!p.busy && p.result.is_none(), "a late result of the old model must not look current");
        p.test_done(TestResult { kind: TestKind::Stt, ok: false, badge: "Error".into(), text: "x".into() }, &mut cx);
        assert!(p.result.is_some());
        // Back from the provider picker with another provider: state resets, keys rebuilt.
        cx.cfg.stt.provider = SttProviderId::Groq;
        p.enter(&mut cx);
        assert!(p.result.is_none());
        assert_eq!(p.provider, SttProviderId::Groq);
        assert!(!p.keys[0].has_key(), "the Groq key is separate from the OpenAI key");
    }

    #[test]
    fn llm_results_are_ignored_here() {
        let mut cfg = Config::default();
        let mut p = page(&cfg, Fake::default());
        let mut cx = Cx::new(&mut cfg, 0);
        p.test_done(TestResult { kind: TestKind::Llm, ok: true, badge: String::new(), text: String::new() }, &mut cx);
        assert!(p.result.is_none());
    }

    #[test]
    fn removing_a_key_is_confirmed_and_clears_the_result() {
        let mut cfg = Config::default();
        let mut p = page(&cfg, Fake::with("openai", "sk-...abcd"));
        let mut cx = Cx::new(&mut cfg, 0);
        let fx = p.event(KEY_A + 6, Event::Pressed, &mut cx);
        assert!(matches!(&fx[0], Effect::Confirm { on_confirm: ConfirmId::RemoveKey(KEY_A), .. }));
        let fx = p.confirmed(ConfirmId::RemoveKey(KEY_A), &mut cx);
        assert_eq!(fx[0], Effect::Do(PopupAction::SetSecret { name: "openai".into(), value: None }));
        assert!(!p.ready(&cx));
    }

    #[test]
    fn every_provider_builds_a_page_with_unique_ids() {
        for pi in registry::PROVIDERS {
            let mut cfg = Config::default();
            cfg.stt.provider = pi.id;
            cfg.stt.custom_base_url = "http://x".into();
            let p = page(&cfg, Fake::default());
            let cx = Cx::new(&mut cfg, 0);
            let mut ids: Vec<ItemId> = p.items(&cx).iter().filter_map(Item::id).collect();
            let n = ids.len();
            ids.sort_unstable();
            ids.dedup();
            assert_eq!(ids.len(), n, "{:?}", pi.id);
        }
    }
}
