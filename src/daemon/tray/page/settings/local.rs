//! Settings > Speech-to-Text > Use a local model: the downloadable models, recommended ones first,
//! each with its tags as badges, its size and languages, and what it is doing (downloading,
//! installed, in use). Tap a model to download it (asked first) or, once it is there, to use it.
//! Installs run in the daemon (`stt::local::downloads`) and survive closing the popup; this page
//! only shows them and asks to be woken while one runs.
use super::kit::{group, note, saved};
use crate::core::config::SttEngine;
use crate::daemon::tray::page::{ConfirmId, Cx, Effect, Event, Icon, Item, ItemId, Lead, PageModel, Row, TestKind, TestResult, Tone, Trailing};
use crate::stt::local::catalog::{self, LocalModel, Tag};
use crate::stt::local::downloads::{self, Status};
use crate::stt::local::install;

/// Model rows are `ID_MODEL0 + catalog index`.
pub const ID_MODEL0: ItemId = 100;
pub const ID_TEST: ItemId = 10;
const ROW_H: f32 = 56.0;

pub struct LocalModelsPage {
    /// Waker subscription while the page is shown.
    token: Option<u64>,
    /// The model to switch to when its download finishes.
    pending: Option<&'static str>,
    busy: bool,
    result: Option<TestResult>,
}

impl LocalModelsPage {
    pub fn boxed() -> Box<dyn PageModel> {
        Box::new(LocalModelsPage { token: None, pending: None, busy: false, result: None })
    }

    fn select(&mut self, m: &LocalModel, cx: &mut Cx) -> Vec<Effect> {
        if cx.cfg.stt.engine == SttEngine::Local && cx.cfg.stt.local_model == m.id {
            return Vec::new();
        }
        cx.cfg.stt.engine = SttEngine::Local;
        cx.cfg.stt.local_model = m.id.to_string();
        self.busy = false;
        self.result = None;
        let mut fx = saved();
        fx.push(Effect::Toast(format!("Using {}", m.name)));
        fx
    }
}

pub fn tag_tone(t: Tag) -> Tone {
    match t {
        Tag::BestOverall => Tone::Ok,
        Tag::Community => Tone::Warn,
        _ => Tone::Info,
    }
}

/// The row of model `i` for the current config and install state.
fn model_row(i: usize, m: &LocalModel, cfg: &crate::core::config::Config) -> Item {
    let in_use = cfg.stt.engine == SttEngine::Local && cfg.stt.local_model == m.id;
    let installed = install::installed(m.id).is_some();
    let status = downloads::status(m.id);
    let mut r = Row::new(ID_MODEL0 + i as ItemId, m.name, ROW_H);
    r.chips = m.tags.iter().map(|t| (t.label().to_string(), tag_tone(*t))).collect();
    let (lead, secondary, trailing) = match &status {
        Some(s) if s.active() => (Icon::Download, s.label(), Trailing::None),
        Some(Status::Failed(_)) => (Icon::Retry, status.as_ref().map(Status::label).unwrap_or_default(), Trailing::None),
        _ if installed && in_use => (Icon::Check, format!("In use \u{b7} {}", m.languages), Trailing::Badge { text: "In use".into(), tone: Tone::Ok }),
        _ if installed => (Icon::Check, format!("Downloaded \u{b7} {}", m.languages), Trailing::Delete),
        _ => (Icon::Download, m.summary(), Trailing::None),
    };
    r.lead = Lead::Icon(lead);
    r.secondary = secondary;
    r.trailing = trailing;
    Item::Row(r)
}

impl PageModel for LocalModelsPage {
    fn title(&self) -> String {
        "Local models".into()
    }

    fn enter(&mut self, cx: &mut Cx) {
        if self.token.is_none() {
            let w = cx.waker.clone();
            self.token = Some(downloads::subscribe(std::sync::Arc::new(move || w())));
        }
    }

    fn leave(&mut self, _cx: &mut Cx) {
        if let Some(t) = self.token.take() {
            downloads::unsubscribe(t);
        }
    }

    fn items(&self, cx: &Cx) -> Vec<Item> {
        let cfg = &*cx.cfg;
        let mut v = vec![note("Runs on this PC: no API key, and your audio never leaves it. Tap a model to download it once.")];
        if !install::runtime_ready() && !downloads::any_active() {
            v.push(note(&format!("The first download also fetches the local engine ({} MB).", install::RUNTIME_DOWNLOAD_MB)));
        }
        if cfg.stt.engine == SttEngine::Local
            && let Some(m) = catalog::get(&cfg.stt.local_model)
            && install::installed(m.id).is_some()
        {
            let sub = if self.busy { "Transcribing a short sample\u{2026}" } else { "Transcribes a short sample with the model in use" };
            v.push(super::kit::test_line(ID_TEST, "Test transcription", sub, self.busy, true));
            if let Some(r) = &self.result {
                v.push(Item::Result { ok: r.ok, badge: r.badge.clone(), text: r.text.clone() });
            }
        }
        let (rec, rest): (Vec<_>, Vec<_>) = catalog::MODELS.iter().enumerate().partition(|(_, m)| m.tags.iter().any(|t| t.is_recommendation()));
        v.push(group("Recommended"));
        v.extend(rec.iter().map(|(i, m)| model_row(*i, m, cfg)));
        v.push(group("More models"));
        v.extend(rest.iter().map(|(i, m)| model_row(*i, m, cfg)));
        v
    }

    fn event(&mut self, id: ItemId, ev: Event, cx: &mut Cx) -> Vec<Effect> {
        if id == ID_TEST && matches!(ev, Event::Pressed | Event::Picked(0)) && !self.busy {
            self.busy = true;
            self.result = None;
            return vec![Effect::Saved, Effect::RunTest(TestKind::Stt)];
        }
        let Some(i) = id.checked_sub(ID_MODEL0).map(|i| i as usize) else { return Vec::new() };
        let Some(m) = catalog::MODELS.get(i) else { return Vec::new() };
        let idx = i as u16;
        let status = downloads::status(m.id);
        match ev {
            Event::Activated => {
                if status.as_ref().is_some_and(Status::active) {
                    return vec![Effect::Confirm {
                        title: format!("Stop downloading {}?", m.name),
                        body: "What was downloaded so far is deleted.".into(),
                        action: "Stop".into(),
                        destructive: true,
                        on_confirm: ConfirmId::LocalCancel(idx),
                    }];
                }
                if install::installed(m.id).is_some() {
                    downloads::clear(m.id);
                    return self.select(m, cx);
                }
                let runtime = if install::runtime_ready() { String::new() } else { format!(" plus the {} MB local engine", install::RUNTIME_DOWNLOAD_MB) };
                let community = if m.tags.contains(&Tag::Community) { " It is a community build, not packaged by sherpa-onnx itself." } else { "" };
                vec![Effect::Confirm {
                    title: format!("Download {}?", m.name),
                    body: format!("About {}{runtime}. It runs on this PC, and OpenTalk switches to it when the download finishes.{community}", m.size_label()),
                    action: "Download".into(),
                    destructive: false,
                    on_confirm: ConfirmId::LocalDownload(idx),
                }]
            }
            Event::Deleted if install::installed(m.id).is_some() => vec![Effect::Confirm {
                title: format!("Delete {}?", m.name),
                body: format!("Frees about {}. You can download it again later.", catalog::size_label(install::installed_mb(m.id).max(1) as u32)),
                action: "Delete".into(),
                destructive: true,
                on_confirm: ConfirmId::LocalRemove(idx),
            }],
            _ => Vec::new(),
        }
    }

    fn confirmed(&mut self, id: ConfirmId, cx: &mut Cx) -> Vec<Effect> {
        let model = |i: u16| catalog::MODELS.get(i as usize);
        match id {
            ConfirmId::LocalDownload(i) => {
                let Some(m) = model(i) else { return Vec::new() };
                downloads::clear(m.id);
                downloads::start(m);
                self.pending = Some(m.id);
                Vec::new()
            }
            ConfirmId::LocalCancel(i) => {
                if let Some(m) = model(i) {
                    downloads::cancel(m.id);
                    if self.pending == Some(m.id) {
                        self.pending = None;
                    }
                }
                Vec::new()
            }
            ConfirmId::LocalRemove(i) => {
                let Some(m) = model(i) else { return Vec::new() };
                let in_use = cx.cfg.stt.engine == SttEngine::Local && cx.cfg.stt.local_model == m.id;
                if in_use || crate::stt::local::server::running().is_some_and(|k| k.model == m.id) {
                    crate::stt::local::server::stop();
                }
                match install::remove(m.id) {
                    Ok(()) if in_use => {
                        // nothing to transcribe with any more: back to the cloud provider
                        cx.cfg.stt.engine = SttEngine::Cloud;
                        let mut fx = saved();
                        fx.push(Effect::Toast(format!("Deleted {}; using the cloud provider", m.name)));
                        fx
                    }
                    Ok(()) => vec![Effect::Toast(format!("Deleted {}", m.name))],
                    Err(e) => vec![Effect::Toast(format!("Could not delete {}: {e}", m.name))],
                }
            }
            _ => Vec::new(),
        }
    }

    fn wake(&mut self, cx: &mut Cx) {
        // A download this page started has finished: use the model (the pager saves the change).
        if let Some(id) = self.pending
            && downloads::status(id) == Some(Status::Done)
        {
            self.pending = None;
            downloads::clear(id);
            if let Some(m) = catalog::get(id) {
                cx.cfg.stt.engine = SttEngine::Local;
                cx.cfg.stt.local_model = m.id.to_string();
            }
        }
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
    use crate::daemon::tray::page::settings::kit::{dump, snapshot};

    fn page() -> LocalModelsPage {
        LocalModelsPage { token: None, pending: None, busy: false, result: None }
    }

    #[test]
    fn page_text_snapshot() {
        let mut cfg = Config::default();
        let p = page();
        snapshot!("local_models", dump(&p.title(), &p.items(&Cx::new(&mut cfg, 0))));
    }

    #[test]
    fn recommended_first_with_their_badges() {
        let mut cfg = Config::default();
        let items = page().items(&Cx::new(&mut cfg, 0));
        let groups: Vec<_> = items.iter().filter_map(|i| if let Item::Group(g) = i { Some(g.as_str()) } else { None }).collect();
        assert_eq!(groups, vec!["Recommended", "More models"]);
        let rows: Vec<&Row> = items.iter().filter_map(|i| if let Item::Row(r) = i { Some(r) } else { None }).collect();
        assert_eq!(rows.len(), 15);
        assert_eq!(rows[0].primary, "NVIDIA Parakeet TDT 0.6B v3");
        assert_eq!(rows[0].chips, vec![("Best overall".to_string(), Tone::Ok)]);
        assert_eq!(rows[0].secondary, "465 MB \u{b7} 25 European languages");
        let qwen = rows.iter().find(|r| r.primary == "Qwen3-ASR 1.7B").unwrap();
        assert_eq!(qwen.chips, vec![("Most accurate".to_string(), Tone::Info), ("Community build".to_string(), Tone::Warn)]);
        let rec = items.iter().position(|i| *i == Item::Group("More models".into())).unwrap();
        let tagged_after = items[rec..].iter().any(|i| matches!(i, Item::Row(r) if r.chips.iter().any(|c| c.1 != Tone::Warn)));
        assert!(!tagged_after, "every recommended model is in the first group");
    }

    #[test]
    fn a_model_that_is_not_downloaded_asks_before_downloading() {
        let mut cfg = Config::default();
        let mut p = page();
        let mut cx = Cx::new(&mut cfg, 0);
        let i = catalog::MODELS.iter().position(|m| m.id == "moonshine-tiny").unwrap();
        let fx = p.event(ID_MODEL0 + i as ItemId, Event::Activated, &mut cx);
        let [Effect::Confirm { title, body, on_confirm, destructive, .. }] = fx.as_slice() else { panic!("{fx:?}") };
        assert_eq!(title, "Download Moonshine Tiny?");
        assert!(body.starts_with("About 103 MB"), "{body}");
        assert_eq!(*on_confirm, ConfirmId::LocalDownload(i as u16));
        assert!(!destructive);
        assert_eq!(cx.cfg.stt.engine, SttEngine::Cloud, "nothing switches before the model is there");
        assert_eq!(p.event(ID_MODEL0 + 99, Event::Activated, &mut cx), vec![], "out of range");
    }

    #[test]
    fn a_finished_download_this_page_started_is_selected_on_wake() {
        let mut cfg = Config::default();
        let mut p = page();
        p.pending = Some("moonshine-base");
        let mut cx = Cx::new(&mut cfg, 0);
        p.wake(&mut cx);
        assert_eq!(cx.cfg.stt.engine, SttEngine::Cloud, "not finished yet: nothing happens");
        assert_eq!(p.pending, Some("moonshine-base"));
    }
}
