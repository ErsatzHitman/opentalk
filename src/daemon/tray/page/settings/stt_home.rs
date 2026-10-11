//! Settings > Speech-to-Text: where dictation is transcribed. Two rows: "Bring your own key" (a
//! cloud provider, the page that was Speech-to-Text before) and "Use a local model" (the model
//! list). The one in use carries an "In use" chip. Opening a row changes nothing by itself: the
//! switch happens on the sub-page (pick a local model, or "Use the cloud provider").
use crate::core::config::SttEngine;
use crate::daemon::tray::page::{Cx, Effect, Event, Icon, Item, ItemId, Lead, PageModel, Route, Row, Tone, Trailing};
use crate::stt::local::{catalog, install};

pub const ID_CLOUD: ItemId = 1;
pub const ID_LOCAL: ItemId = 2;
const ROW_H: f32 = 56.0;

pub struct SttHomePage;

impl SttHomePage {
    pub fn boxed() -> Box<dyn PageModel> {
        Box::new(SttHomePage)
    }
}

fn row(id: ItemId, icon: Icon, title: &str, sub: String, in_use: bool) -> Item {
    let mut r = Row::new(id, title, ROW_H);
    r.lead = Lead::Icon(icon);
    r.secondary = sub;
    if in_use {
        r.chips = vec![("In use".into(), Tone::Ok)];
    }
    r.trailing = Trailing::Chevron;
    Item::Row(r)
}

impl PageModel for SttHomePage {
    fn title(&self) -> String {
        "Speech-to-Text".into()
    }

    fn items(&self, cx: &Cx) -> Vec<Item> {
        let cfg = &*cx.cfg;
        let local = cfg.stt.engine == SttEngine::Local;
        let provider = crate::stt::registry::provider(cfg.stt.provider).name;
        let cloud_sub = format!("{provider} \u{b7} {}", cfg.stt_model());
        let installed = catalog::MODELS.iter().filter(|m| install::installed(m.id).is_some()).count();
        let local_sub = match catalog::get(&cfg.stt.local_model) {
            Some(m) if local => format!("{} \u{b7} on this PC", m.name),
            _ if installed > 0 => format!("{installed} downloaded \u{b7} no API key"),
            _ => "Runs on this PC \u{b7} no API key, audio stays here".into(),
        };
        vec![
            row(ID_CLOUD, Icon::Link, "Bring your own key", cloud_sub, !local),
            row(ID_LOCAL, Icon::Shield, "Use a local model", local_sub, local),
        ]
    }

    fn event(&mut self, id: ItemId, ev: Event, _cx: &mut Cx) -> Vec<Effect> {
        match (id, ev) {
            (ID_CLOUD, Event::Activated) => vec![Effect::Navigate(Route::SttCloud)],
            (ID_LOCAL, Event::Activated) => vec![Effect::Navigate(Route::SttLocal)],
            _ => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::config::Config;
    use crate::daemon::tray::page::settings::kit::{dump, snapshot};

    #[test]
    fn two_rows_and_the_one_in_use_is_marked() {
        let mut cfg = Config::default();
        let p = SttHomePage;
        snapshot!("stt_home_cloud", dump(&p.title(), &p.items(&Cx::new(&mut cfg, 0))));
        cfg.stt.engine = SttEngine::Local;
        cfg.stt.local_model = "parakeet-tdt-0.6b-v3".into();
        let items = p.items(&Cx::new(&mut cfg, 0));
        let Item::Row(r) = &items[1] else { panic!() };
        assert_eq!(r.chips, vec![("In use".to_string(), Tone::Ok)]);
        assert!(r.secondary.starts_with("NVIDIA Parakeet TDT 0.6B v3"), "{}", r.secondary);
        let Item::Row(c) = &items[0] else { panic!() };
        assert!(c.chips.is_empty());
    }

    #[test]
    fn rows_open_their_pages_without_switching() {
        let mut cfg = Config::default();
        let mut p = SttHomePage;
        let mut cx = Cx::new(&mut cfg, 0);
        assert_eq!(p.event(ID_LOCAL, Event::Activated, &mut cx), vec![Effect::Navigate(Route::SttLocal)]);
        assert_eq!(p.event(ID_CLOUD, Event::Activated, &mut cx), vec![Effect::Navigate(Route::SttCloud)]);
        assert_eq!(cx.cfg.stt.engine, SttEngine::Cloud);
    }
}
