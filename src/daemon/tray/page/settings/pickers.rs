//! List pickers (there are no native dropdowns in the popup): a page of radio rows, optionally
//! with a filter field. Choosing a row applies it, saves and goes back.
use std::sync::{Arc, Mutex};

use super::kit::{button, group, note, radio, text_field};
use crate::core::config::Config;
use crate::core::settings_logic::{LANGUAGES, RETENTION, WARM_CHOICES, retention_index, warm_index};
use crate::daemon::tray::page::{BtnStyle, Cx, Effect, Event, Item, ItemId, PageModel, PickerId};

const ID_FILTER: ItemId = 1;
const ID_REFRESH: ItemId = 2;
/// Option rows are `ID_OPT0 + index`.
const ID_OPT0: ItemId = 100;

/// Capture devices as (endpoint id, display name).
pub type Devices = Vec<(String, String)>;

fn system_devices() -> Devices {
    std::panic::catch_unwind(crate::daemon::audio::list_devices).unwrap_or_default().into_iter().map(|d| (d.id, d.name)).collect()
}

pub struct PickerPage {
    id: PickerId,
    filter: String,
    /// Input devices once listed (`None` while the worker runs).
    devices: Option<Devices>,
    slot: Arc<Mutex<Option<Devices>>>,
    loader: fn() -> Devices,
}

impl PickerPage {
    pub fn boxed(id: PickerId) -> Box<dyn PageModel> {
        Box::new(PickerPage::new(id, system_devices))
    }

    pub fn new(id: PickerId, loader: fn() -> Devices) -> PickerPage {
        PickerPage { id, filter: String::new(), devices: None, slot: Arc::default(), loader }
    }

    /// List the capture devices on a worker (WASAPI enumeration can take a moment) and wake the page.
    fn load_devices(&mut self, cx: &Cx) {
        self.devices = None;
        let slot = self.slot.clone();
        let wake = cx.waker.clone();
        let loader = self.loader;
        let spawned = std::thread::Builder::new().name("device-list".into()).spawn(move || {
            let list = loader();
            *slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(list);
            wake();
        });
        if spawned.is_err() {
            self.devices = Some(Vec::new());
        }
    }

    /// (label, description, selected) rows of the current picker, after filtering.
    fn options(&self, cfg: &Config) -> Vec<(usize, String, String, bool)> {
        let mut rows: Vec<(usize, String, String, bool)> = Vec::new();
        match self.id {
            PickerId::Language => {
                let q = self.filter.trim().to_lowercase();
                for (i, (code, label)) in LANGUAGES.iter().enumerate() {
                    if q.is_empty() || label.to_lowercase().contains(&q) || code.to_lowercase().contains(&q) {
                        rows.push((i, label.to_string(), String::new(), *code == cfg.language));
                    }
                }
            }
            PickerId::InputDevice => {
                rows.push((0, "System default".into(), "Follows the Windows default input device.".into(), cfg.mic_device_id.is_none()));
                let list = self.devices.as_deref().unwrap_or(&[]);
                for (i, (id, name)) in list.iter().enumerate() {
                    rows.push((i + 1, name.clone(), String::new(), cfg.mic_device_id.as_deref() == Some(id)));
                }
                if let (Some(id), Some(_)) = (&cfg.mic_device_id, &self.devices)
                    && !list.iter().any(|(d, _)| d == id)
                {
                    let name = cfg.mic_device_label.clone().filter(|l| !l.trim().is_empty()).unwrap_or_else(|| "Selected device".into());
                    rows.push((list.len() + 1, format!("{name} (not connected)"), "Dictation uses the system default until it is back.".into(), true));
                }
            }
            PickerId::MicWarm => {
                let cur = warm_index(cfg.mic_warm_hold_secs);
                rows.extend(WARM_CHOICES.iter().enumerate().map(|(i, (l, _))| (i, l.to_string(), String::new(), i == cur)));
            }
            PickerId::Retention => {
                let cur = retention_index(cfg.history_retention_days);
                rows.extend(RETENTION.iter().enumerate().map(|(i, (l, _))| (i, l.to_string(), String::new(), i == cur)));
            }
            PickerId::SttProvider => {
                rows.extend(crate::stt::registry::PROVIDERS.iter().enumerate().map(|(i, p)| {
                    let desc = if p.id == crate::core::config::SttProviderId::Openai { "Recommended" } else { "" };
                    (i, p.name.to_string(), desc.to_string(), p.id == cfg.stt.provider)
                }));
            }
            PickerId::LlmProvider => {
                rows.extend(crate::llm::registry::PROVIDERS.iter().enumerate().map(|(i, p)| {
                    (i, p.name.to_string(), if p.enterprise { "Enterprise".into() } else { String::new() }, p.id == cfg.ai_cleanup.provider)
                }));
            }
            PickerId::LlmModel => {
                let info = crate::llm::registry::provider(cfg.ai_cleanup.provider);
                let cur = cfg.llm_model();
                rows.extend(info.models.iter().enumerate().map(|(i, m)| (i, m.to_string(), String::new(), *m == cur)));
            }
        }
        rows
    }

    /// Apply the chosen option `index` (an index from [`Self::options`]).
    fn choose(&mut self, index: usize, cfg: &mut Config) -> bool {
        match self.id {
            PickerId::Language => match LANGUAGES.get(index) {
                Some((code, _)) => cfg.language = code.to_string(),
                None => return false,
            },
            PickerId::InputDevice => {
                if index == 0 {
                    cfg.mic_device_id = None;
                    cfg.mic_device_label = None;
                } else {
                    let Some((id, name)) = self.devices.as_ref().and_then(|d| d.get(index - 1)) else { return false };
                    cfg.mic_device_id = Some(id.clone());
                    cfg.mic_device_label = Some(name.clone());
                }
            }
            PickerId::MicWarm => match WARM_CHOICES.get(index) {
                Some((_, s)) => cfg.mic_warm_hold_secs = *s,
                None => return false,
            },
            PickerId::Retention => match RETENTION.get(index) {
                Some((_, d)) => cfg.history_retention_days = *d,
                None => return false,
            },
            PickerId::SttProvider => match crate::stt::registry::PROVIDERS.get(index) {
                Some(p) => cfg.stt.provider = p.id,
                None => return false,
            },
            PickerId::LlmProvider => match crate::llm::registry::PROVIDERS.get(index) {
                Some(p) => cfg.ai_cleanup.provider = p.id,
                None => return false,
            },
            PickerId::LlmModel => {
                let info = crate::llm::registry::provider(cfg.ai_cleanup.provider);
                match info.models.get(index) {
                    Some(m) => {
                        cfg.ai_cleanup.models.insert(cfg.ai_cleanup.provider, m.to_string());
                    }
                    None => return false,
                }
            }
        }
        true
    }
}

impl PageModel for PickerPage {
    fn title(&self) -> String {
        match self.id {
            PickerId::Language => "Language",
            PickerId::InputDevice => "Input device",
            PickerId::MicWarm => "Keep microphone warm",
            PickerId::Retention => "Keep transcripts for",
            PickerId::SttProvider => "Speech provider",
            PickerId::LlmProvider => "AI provider",
            PickerId::LlmModel => "AI model",
        }
        .into()
    }

    fn enter(&mut self, cx: &mut Cx) {
        if self.id == PickerId::InputDevice {
            self.load_devices(cx);
        }
    }

    fn wake(&mut self, _cx: &mut Cx) {
        if let Some(list) = self.slot.lock().unwrap_or_else(|e| e.into_inner()).take() {
            self.devices = Some(list);
        }
    }

    fn items(&self, cx: &Cx) -> Vec<Item> {
        let mut v = Vec::new();
        match self.id {
            PickerId::Language => v.push(text_field(ID_FILTER, "Filter", "Type a language", &self.filter)),
            PickerId::InputDevice => {
                v.push(button(ID_REFRESH, if self.devices.is_none() { "Looking for microphones..." } else { "Refresh devices" }, BtnStyle::Outline, self.devices.is_some()));
            }
            PickerId::LlmModel => v.push(note("Suggestions for the current provider. Type any other model id in the Model field.")),
            _ => {}
        }
        let rows = self.options(cx.cfg);
        if rows.is_empty() {
            v.push(note(match self.id {
                PickerId::Language => "No language matches the filter.",
                PickerId::LlmModel => "This provider has no suggestions. Type the model id in the Model field.",
                _ => "Nothing to choose from.",
            }));
        } else {
            v.push(group(match self.id {
                PickerId::Language => "Languages",
                PickerId::InputDevice => "Microphones",
                PickerId::LlmModel => "Models",
                PickerId::SttProvider | PickerId::LlmProvider => "Providers",
                _ => "Options",
            }));
        }
        v.extend(rows.into_iter().map(|(i, label, desc, on)| radio(ID_OPT0 + i as ItemId, &label, &desc, on)));
        v
    }

    fn event(&mut self, id: ItemId, ev: Event, cx: &mut Cx) -> Vec<Effect> {
        match (id, ev) {
            (ID_FILTER, Event::Edited(t) | Event::Committed(t)) => {
                self.filter = t;
                Vec::new()
            }
            (ID_REFRESH, Event::Pressed) if self.id == PickerId::InputDevice => {
                self.load_devices(cx);
                Vec::new()
            }
            (o, Event::Toggled(true) | Event::Activated) if o >= ID_OPT0 => {
                if self.choose((o - ID_OPT0) as usize, cx.cfg) {
                    vec![Effect::Saved, Effect::Back]
                } else {
                    Vec::new()
                }
            }
            _ => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::config::{LlmProviderId, SttProviderId};
    use crate::daemon::tray::page::settings::kit::{dump, snapshot};

    fn fake_devices() -> Devices {
        vec![("{0.0.1}.a".into(), "Headset Microphone".into()), ("{0.0.1}.b".into(), "Studio USB".into())]
    }

    fn picker(id: PickerId) -> PickerPage {
        let mut p = PickerPage::new(id, fake_devices);
        p.devices = Some(fake_devices());
        p
    }

    fn on_ids(p: &PickerPage, cfg: &mut Config) -> Vec<ItemId> {
        let cx = Cx::new(cfg, 0);
        p.items(&cx).iter().filter_map(|i| match i {
            Item::Radio { id, on: true, .. } => Some(*id),
            _ => None,
        }).collect()
    }

    #[test]
    fn page_text_snapshots() {
        let mut cfg = Config::default();
        for (name, id) in [("picker_warm", PickerId::MicWarm), ("picker_retention", PickerId::Retention), ("picker_stt_provider", PickerId::SttProvider), ("picker_llm_provider", PickerId::LlmProvider), ("picker_llm_model", PickerId::LlmModel), ("picker_devices", PickerId::InputDevice)] {
            let p = picker(id);
            let cx = Cx::new(&mut cfg, 0);
            let text = dump(&p.title(), &p.items(&cx));
            match name {
                "picker_warm" => snapshot!("picker_warm", text),
                "picker_retention" => snapshot!("picker_retention", text),
                "picker_stt_provider" => snapshot!("picker_stt_provider", text),
                "picker_llm_provider" => snapshot!("picker_llm_provider", text),
                "picker_llm_model" => snapshot!("picker_llm_model", text),
                _ => snapshot!("picker_devices", text),
            }
        }
        let p = picker(PickerId::Language);
        let cx = Cx::new(&mut cfg, 0);
        let text: String = dump(&p.title(), &p.items(&cx)).lines().take(6).collect::<Vec<_>>().join("\n");
        snapshot!("picker_language_head", text);
    }

    #[test]
    fn choosing_applies_saves_and_goes_back_for_every_picker() {
        let mut cfg = Config::default();
        let cases: [(PickerId, usize); 6] = [(PickerId::Language, 15), (PickerId::MicWarm, 3), (PickerId::Retention, 2), (PickerId::SttProvider, 1), (PickerId::LlmProvider, 4), (PickerId::LlmModel, 1)];
        for (id, idx) in cases {
            let mut p = picker(id);
            let mut cx = Cx::new(&mut cfg, 0);
            let fx = p.event(ID_OPT0 + idx as ItemId, Event::Activated, &mut cx);
            assert_eq!(fx, vec![Effect::Saved, Effect::Back], "{id:?}");
        }
        assert_eq!(cfg.language, "en-US");
        assert_eq!(cfg.mic_warm_hold_secs, 300);
        assert_eq!(cfg.history_retention_days, 30);
        assert_eq!(cfg.stt.provider, SttProviderId::Groq);
        assert_eq!(cfg.ai_cleanup.provider, LlmProviderId::Openrouter);
        assert_eq!(cfg.llm_model(), crate::llm::registry::provider(LlmProviderId::Openrouter).models[1]);
    }

    #[test]
    fn the_current_value_is_the_selected_radio() {
        let mut cfg = Config { language: "de".into(), mic_warm_hold_secs: 100, history_retention_days: 45, ..Config::default() };
        let idx_de = LANGUAGES.iter().position(|(c, _)| *c == "de").unwrap();
        assert_eq!(on_ids(&picker(PickerId::Language), &mut cfg), vec![ID_OPT0 + idx_de as ItemId]);
        assert_eq!(on_ids(&picker(PickerId::MicWarm), &mut cfg), vec![ID_OPT0 + 2], "nearest to 100 s");
        assert_eq!(on_ids(&picker(PickerId::Retention), &mut cfg), vec![ID_OPT0 + 3], "45 days rounds up to 90");
        assert_eq!(on_ids(&picker(PickerId::SttProvider), &mut cfg), vec![ID_OPT0]);
        assert_eq!(on_ids(&picker(PickerId::LlmModel), &mut cfg), vec![ID_OPT0], "the default model");
    }

    #[test]
    fn language_filter_narrows_the_list_and_selection_maps_back_to_the_code() {
        let mut cfg = Config::default();
        let mut p = picker(PickerId::Language);
        let mut cx = Cx::new(&mut cfg, 0);
        assert!(p.event(ID_FILTER, Event::Edited("ger".into()), &mut cx).is_empty());
        let radios: Vec<_> = p.items(&cx).into_iter().filter_map(|i| match i {
            Item::Radio { id, label, .. } => Some((id, label)),
            _ => None,
        }).collect();
        assert_eq!(radios.len(), 1);
        assert_eq!(radios[0].1, "German");
        assert_eq!(p.event(radios[0].0, Event::Activated, &mut cx), vec![Effect::Saved, Effect::Back]);
        assert_eq!(cx.cfg.language, "de", "the id maps to the registry index, not the filtered position");
        p.event(ID_FILTER, Event::Edited("zzzz".into()), &mut cx);
        assert!(p.items(&cx).iter().any(|i| matches!(i, Item::Note(n) if n.contains("No language"))));
        p.event(ID_FILTER, Event::Edited("".into()), &mut cx);
        assert_eq!(p.items(&cx).iter().filter(|i| matches!(i, Item::Radio { .. })).count(), 61);
    }

    #[test]
    fn input_device_default_specific_and_missing() {
        let mut cfg = Config::default();
        let mut p = picker(PickerId::InputDevice);
        assert_eq!(on_ids(&p, &mut cfg), vec![ID_OPT0], "system default");
        {
            let mut cx = Cx::new(&mut cfg, 0);
            assert_eq!(p.event(ID_OPT0 + 2, Event::Activated, &mut cx), vec![Effect::Saved, Effect::Back]);
            assert_eq!(cx.cfg.mic_device_id.as_deref(), Some("{0.0.1}.b"));
            assert_eq!(cx.cfg.mic_device_label.as_deref(), Some("Studio USB"));
        }
        assert_eq!(on_ids(&p, &mut cfg), vec![ID_OPT0 + 2]);
        cfg.mic_device_id = Some("{unplugged}".into());
        cfg.mic_device_label = Some("Old Headset".into());
        let items = {
            let cx = Cx::new(&mut cfg, 0);
            p.items(&cx)
        };
        assert!(items.iter().any(|i| matches!(i, Item::Radio { label, on: true, .. } if label == "Old Headset (not connected)")));
        let mut cx = Cx::new(&mut cfg, 0);
        assert_eq!(p.event(ID_OPT0, Event::Activated, &mut cx), vec![Effect::Saved, Effect::Back]);
        assert_eq!((cx.cfg.mic_device_id.clone(), cx.cfg.mic_device_label.clone()), (None, None));
        // A row that no longer exists (stale event after a refresh) is ignored.
        assert!(p.event(ID_OPT0 + 9, Event::Activated, &mut cx).is_empty());
    }

    #[test]
    fn device_list_loads_on_a_worker_and_wake_picks_it_up() {
        let mut cfg = Config::default();
        let mut p = PickerPage::new(PickerId::InputDevice, fake_devices);
        let mut cx = Cx::new(&mut cfg, 0);
        let woken = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let w = woken.clone();
        cx.waker = Arc::new(move || w.store(true, std::sync::atomic::Ordering::SeqCst));
        p.enter(&mut cx);
        assert!(p.devices.is_none());
        assert!(p.items(&cx).iter().any(|i| matches!(i, Item::Button { id: ID_REFRESH, enabled: false, .. })), "refresh is disabled while listing");
        for _ in 0..200 {
            if woken.load(std::sync::atomic::Ordering::SeqCst) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(woken.load(std::sync::atomic::Ordering::SeqCst), "the worker woke the page");
        p.wake(&mut cx);
        assert_eq!(p.devices.as_ref().map(Vec::len), Some(2));
        p.wake(&mut cx); // a spurious wake changes nothing
        assert_eq!(p.devices.as_ref().map(Vec::len), Some(2));
        assert!(p.items(&cx).iter().any(|i| matches!(i, Item::Button { id: ID_REFRESH, enabled: true, .. })));
    }

    #[test]
    fn non_device_pickers_do_not_start_workers() {
        let mut cfg = Config::default();
        let mut cx = Cx::new(&mut cfg, 0);
        let mut p = PickerPage::new(PickerId::MicWarm, || panic!("must not list devices"));
        p.enter(&mut cx);
        assert!(p.devices.is_none() && p.slot.lock().unwrap().is_none());
    }

    #[test]
    fn llm_model_picker_without_suggestions_explains_itself() {
        let mut cfg = Config::default();
        cfg.ai_cleanup.provider = LlmProviderId::Custom;
        let p = picker(PickerId::LlmModel);
        let cx = Cx::new(&mut cfg, 0);
        assert!(p.items(&cx).iter().any(|i| matches!(i, Item::Note(n) if n.contains("no suggestions"))));
    }

    #[test]
    fn ids_are_unique_in_every_picker() {
        let mut cfg = Config::default();
        for id in PickerId::ALL {
            let p = picker(id);
            let cx = Cx::new(&mut cfg, 0);
            let mut ids: Vec<ItemId> = p.items(&cx).iter().filter_map(Item::id).collect();
            let n = ids.len();
            ids.sort_unstable();
            ids.dedup();
            assert_eq!(ids.len(), n, "{id:?}");
        }
    }
}
