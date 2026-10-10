//! Speech-to-Text: provider tabs, API key, model cards, custom endpoint, streaming, Test button.
use super::{SettingsState, api_key_field, group};
use crate::core::config::SttProviderId;
use crate::core::secrets::SecretName;
use crate::stt::registry::{self, PROVIDERS};
use crate::ui::fonts;
use crate::ui::theme::{Palette, mix};
use crate::ui::widgets::{self as w, Btn, ChipKind, Variant};
use crate::ui::{Env, icons};
use eframe::egui::{self, Align2, Color32, Sense, Ui, pos2, vec2};
use std::sync::mpsc::{Receiver, channel};
use std::time::Instant;

/// Short clean sample ("Hey, can you send the quarterly report to Priya by end of day? Thanks.").
const SAMPLE_WAV: &[u8] = include_bytes!("../../../tests/fixtures/tts_clean.wav");

#[derive(Clone, Debug, PartialEq)]
pub enum TestOutcome {
    Ok { text: String, latency_ms: u64 },
    Err(String),
}

#[derive(Default)]
pub struct SttState {
    rx: Option<Receiver<TestOutcome>>,
    pub outcome: Option<TestOutcome>,
}

impl SttState {
    pub fn testing(&self) -> bool {
        self.rx.is_some()
    }

    /// Pull a finished result from the background thread.
    pub fn poll(&mut self) {
        if let Some(rx) = &self.rx {
            if let Ok(o) = rx.try_recv() {
                self.outcome = Some(o);
                self.rx = None;
            }
        }
    }
}

/// Run the embedded sample through the active provider on a worker thread.
fn start_test(ctx: &egui::Context, cfg: &crate::core::config::Config, st: &mut SttState) {
    let (tx, rx) = channel();
    st.rx = Some(rx);
    st.outcome = None;
    let cfg = cfg.clone();
    let ctx = ctx.clone();
    let spawned = std::thread::Builder::new().name("stt-test".into()).spawn(move || {
        let outcome = run_sample(&cfg);
        let _ = tx.send(outcome);
        ctx.request_repaint();
    });
    if spawned.is_err() {
        st.rx = None;
        st.outcome = Some(TestOutcome::Err("could not start a worker thread".into()));
    }
}

pub fn run_sample(cfg: &crate::core::config::Config) -> TestOutcome {
    let Some(audio) = crate::stt::Audio::from_wav(SAMPLE_WAV) else {
        return TestOutcome::Err("embedded sample is unreadable".into());
    };
    let started = Instant::now();
    match crate::ui::safe(|| crate::stt::transcribe(cfg, &audio)) {
        Some(Ok(t)) => TestOutcome::Ok { text: t.text, latency_ms: started.elapsed().as_millis() as u64 },
        Some(Err(e)) => TestOutcome::Err(e.to_string()),
        None => TestOutcome::Err("the speech provider is not available in this build".into()),
    }
}

pub fn show(ui: &mut Ui, env: &mut Env, st: &mut SettingsState) -> bool {
    let p = *env.p;
    let mut changed = false;
    st.stt.poll();
    let provider = env.cfg.stt.provider;
    let info = registry::provider(provider);

    w::section_title(ui, &p, "Provider", "Audio is sent to the provider you choose, using your own API key.");
    let labels: Vec<(&str, &str)> = PROVIDERS.iter().map(|pi| (pi.name, if pi.id == SttProviderId::Openai { "Recommended" } else { "" })).collect();
    let sel = PROVIDERS.iter().position(|pi| pi.id == provider).unwrap_or(0);
    if let Some(i) = w::pill_tabs(ui, &p, sel, &labels) {
        if PROVIDERS[i].id != provider {
            env.cfg.stt.provider = PROVIDERS[i].id;
            st.key_edit = None;
            st.stt.outcome = None;
            changed = true;
        }
    }
    let provider = env.cfg.stt.provider;
    let info = if provider == info.id { info } else { registry::provider(provider) };

    // ---- credentials
    ui.add_space(16.0);
    w::panel(ui, &p, |ui| {
        w::panel_pad(ui, 16, 14, |ui| {
            ui.spacing_mut().item_spacing = vec2(0.0, 10.0);
            if provider == SttProviderId::Corti {
                changed |= api_key_field(ui, env, st, SecretName::CORTI_CLIENT_ID, "Client ID", info.key_url);
                changed |= api_key_field(ui, env, st, SecretName::CORTI_CLIENT_SECRET, "Client secret", "");
            } else {
                changed |= api_key_field(ui, env, st, info.secret, "API key", info.key_url);
            }
            if !info.note.is_empty() {
                w::text(ui, info.note, 12.0, p.muted_a(75));
            }
        });
    });

    // ---- provider specifics
    if provider == SttProviderId::Custom {
        group(ui, &p, "Custom endpoint", "Any server that implements the OpenAI /audio/transcriptions API, including Azure OpenAI deployments.");
        w::panel(ui, &p, |ui| {
            w::row_stacked(ui, &p, "Base URL", "For example https://my-server/v1 (the path /audio/transcriptions is added).", |ui| {
                let wd = ui.available_width();
                let r = w::text_input(ui, &p, &mut env.cfg.stt.custom_base_url, "https://api.example.com/v1", wd, false);
                changed |= r.changed();
            });
            w::divider(ui, &p);
            w::row_stacked(ui, &p, "Model", "Sent as the model field (or the deployment name on Azure).", |ui| {
                let wd = ui.available_width();
                let r = w::text_input(ui, &p, &mut env.cfg.stt.custom_model, "whisper-1", wd, false);
                changed |= r.changed();
            });
        });
    } else if provider == SttProviderId::Corti {
        group(ui, &p, "Corti environment", "");
        w::panel(ui, &p, |ui| {
            w::row(ui, &p, "Region", "Data residency of the Corti tenant.", 140.0, |ui| {
                let opts = ["us".to_string(), "eu".to_string()];
                let cur = opts.iter().position(|o| *o == env.cfg.stt.corti_environment);
                let r = w::select(ui, &p, "corti-env", &env.cfg.stt.corti_environment.to_uppercase(), &["US".to_string(), "EU".to_string()], cur, 140.0, false);
                if let Some(i) = r.chosen {
                    env.cfg.stt.corti_environment = opts[i].clone();
                    changed = true;
                }
            });
            w::divider(ui, &p);
            w::row(ui, &p, "Tenant", "Your Corti tenant name.", 200.0, |ui| {
                let r = w::text_input(ui, &p, &mut env.cfg.stt.corti_tenant, "base", 200.0, false);
                changed |= r.changed();
            });
        });
    }

    // ---- models
    if !info.models.is_empty() {
        group(ui, &p, "Model", "");
        let active = env.cfg.stt_model();
        let mut chosen: Option<&'static str> = None;
        w::panel(ui, &p, |ui| {
            for (i, m) in info.models.iter().enumerate() {
                if i > 0 {
                    w::divider(ui, &p);
                }
                let mut chips: Vec<(&str, ChipKind)> = Vec::new();
                if i == 0 {
                    chips.push(("Recommended", ChipKind::Primary));
                }
                if m.streaming_only {
                    chips.push(("Streaming only", ChipKind::Neutral));
                } else if m.streaming {
                    chips.push(("Streaming", ChipKind::Neutral));
                }
                let is_active = m.id == active;
                if model_card(ui, &p, m.name, m.description, &chips, is_active, (i == 0, i + 1 == info.models.len())) && !is_active {
                    chosen = Some(m.id);
                }
            }
        });
        if let Some(id) = chosen {
            env.cfg.stt.models.insert(provider, id.to_string());
            changed = true;
        }
    }

    // ---- transport
    if info.models.iter().any(|m| m.streaming) {
        group(ui, &p, "Transport", "");
        w::panel(ui, &p, |ui| {
            changed |= super::preferences::toggle_row(
                ui,
                &p,
                "Prefer streaming",
                "Send audio while you speak so the text is ready sooner. Falls back to a single upload when the model does not stream.",
                &mut env.cfg.stt.prefer_streaming,
            );
        });
    }

    // ---- test
    group(ui, &p, "Test", "Transcribes a short built-in sample with the settings above, so you can check the key and measure latency.");
    let key_ready = if provider == SttProviderId::Corti { st.secrets.has(SecretName::CORTI_CLIENT_ID) } else { st.secrets.has(info.secret) || provider == SttProviderId::Custom };
    let busy = st.stt.testing();
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 10.0;
        let label = if busy { "Testing..." } else { "Test transcription" };
        if w::button(ui, &p, Btn::new(label, Variant::Primary).icon(icons::PLAY).small().enabled(!busy && key_ready)).clicked() {
            start_test(ui.ctx(), env.cfg, &mut st.stt);
        }
        if busy {
            ui.add(egui::Spinner::new().size(16.0).color(p.primary));
        } else if !key_ready {
            w::text(ui, "Save an API key first.", 12.0, p.muted_a(75));
        }
    });
    match st.stt.outcome.clone() {
        Some(TestOutcome::Ok { text, latency_ms }) => {
            ui.add_space(10.0);
            result_box(ui, &p, true, &format!("{} ms", latency_ms), &text);
        }
        Some(TestOutcome::Err(e)) => {
            ui.add_space(10.0);
            result_box(ui, &p, false, "Failed", &e);
        }
        None => {}
    }
    if busy {
        ui.ctx().request_repaint_after(std::time::Duration::from_millis(100));
    }
    changed
}

fn result_box(ui: &mut Ui, p: &Palette, ok: bool, badge: &str, body: &str) {
    let col = if ok { p.success } else { p.destructive };
    w::notice(ui, p, col, |ui| {
        ui.spacing_mut().item_spacing = vec2(0.0, 4.0);
        ui.horizontal(|ui| {
            w::text_semi(ui, if ok { "Transcribed" } else { "Error" }, 12.0, col);
            w::text(ui, badge, 12.0, p.muted_a(90));
        });
        w::text(ui, body, 13.0, p.foreground);
    });
}

/// One selectable model row of the models panel: a wash on hover, a primary wash when active.
/// Returns true when clicked.
fn model_card(ui: &mut Ui, p: &Palette, name: &str, desc: &str, chips: &[(&str, ChipKind)], active: bool, (first, last): (bool, bool)) -> bool {
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 48.0), Sense::click());
    w::pointer(&resp);
    let hov = w::hover_anim(ui, resp.id.with("h"), resp.hovered());
    let t = ui.ctx().animate_bool_with_time(resp.id.with("a"), active, 0.15);
    let painter = ui.painter();
    painter.rect_filled(rect, w::edge_rounding(first, last), mix(mix(Color32::TRANSPARENT, p.wash(0), hov), p.primary_wash(0), t));
    // status dot
    painter.circle_filled(pos2(rect.min.x + 20.0, rect.center().y), 3.0, mix(p.muted_a(30), p.success, t));
    painter.text(pos2(rect.min.x + 34.0, rect.center().y), Align2::LEFT_CENTER, name, fonts::semibold(13.0), p.foreground);
    let nw = w::measure(ui, name, fonts::semibold(13.0)).x;
    ui.painter().with_clip_rect(egui::Rect::from_min_max(rect.min, pos2(rect.max.x - 150.0, rect.max.y))).text(
        pos2(rect.min.x + 34.0 + nw + 10.0, rect.center().y),
        Align2::LEFT_CENTER,
        desc,
        fonts::regular(12.0),
        p.muted_a(85),
    );
    // chips from the right edge
    let mut x = rect.max.x - 16.0;
    let mut all: Vec<(&str, ChipKind)> = chips.to_vec();
    if active {
        all.push(("Active", ChipKind::Primary));
    }
    for (label, kind) in all.iter().rev() {
        let tw = w::chip_width(ui, label);
        let r = egui::Rect::from_min_size(pos2(x - tw, rect.center().y - 10.0), vec2(tw, 20.0));
        w::paint_chip(ui, p, r, label, *kind);
        x -= tw + 6.0;
    }
    resp.clicked()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sample_is_a_valid_short_wav() {
        let a = crate::stt::Audio::from_wav(SAMPLE_WAV).expect("tts_clean.wav parses");
        assert_eq!(a.sample_rate, 16_000);
        assert!(a.duration_ms() > 1000 && a.duration_ms() < 15_000, "{} ms", a.duration_ms());
    }

    #[test]
    fn poll_moves_result_out_of_the_channel() {
        let (tx, rx) = channel();
        let mut s = SttState { rx: Some(rx), outcome: None };
        assert!(s.testing());
        s.poll();
        assert!(s.testing(), "nothing sent yet");
        tx.send(TestOutcome::Ok { text: "hi".into(), latency_ms: 5 }).unwrap();
        s.poll();
        assert!(!s.testing());
        assert_eq!(s.outcome, Some(TestOutcome::Ok { text: "hi".into(), latency_ms: 5 }));
    }
}

#[cfg(test)]
mod live_tests {
    use super::*;
    use crate::core::config::{Config, SttProviderId};

    /// Verdict logic shared with `tests/e2e.rs` (one definition: `tests/common/live.rs`).
    #[allow(dead_code)]
    mod live {
        include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/common/live.rs"));
    }
    use live::{Live, report_live, try_model};

    /// Free OpenRouter audio-input models (PLAN section 10); the live policy allows `:free` only.
    const FREE_MODELS: [&str; 3] =
        ["thinkingmachines/inkling-small:free", "thinkingmachines/inkling:free", "nvidia/nemotron-3-nano-omni-30b-a3b-reasoning:free"];

    /// Words of the sample ("...send the quarterly report to Priya by end of day"); a reply
    /// without any of them is not a transcript of it (e.g. "I need the audio file.").
    const SPOKEN: [&str; 2] = ["report", "priya"];

    /// Runs the Test button's pipeline (`run_sample`) on `model` and judges the reply.
    fn judge(cfg: &Config, model: &str, backoff_s: &[u64], sleep: impl FnMut(std::time::Duration)) -> Live {
        let mut cfg = cfg.clone();
        cfg.stt.models.insert(SttProviderId::Openrouter, model.to_string());
        try_model(
            model,
            &SPOKEN,
            backoff_s,
            || match run_sample(&cfg) {
                TestOutcome::Ok { text, latency_ms } => {
                    eprintln!("{model}: {latency_ms} ms: {text:?}");
                    Ok(text)
                }
                TestOutcome::Err(e) => {
                    eprintln!("{model}: {e}");
                    Err(e)
                }
            },
            sleep,
        )
    }

    /// A 200 reply that does not contain the spoken words must never count as VERIFIED.
    #[test]
    fn reply_without_the_spoken_words_is_not_verified() {
        let mut calls = 0;
        let v = try_model(
            FREE_MODELS[2],
            &SPOKEN,
            &[0, 3],
            || {
                calls += 1;
                Ok("I need the audio file.".to_string())
            },
            |_| {},
        );
        assert_eq!(calls, 1);
        assert_eq!(v, live::dropped_audio(FREE_MODELS[2]));
        assert!(FREE_MODELS.iter().all(|m| m.ends_with(":free")));
    }

    /// The Test button's pipeline against the real OpenRouter free tier. Needs
    /// `OPENTALK_SECRET_OPENROUTER`. Prints `VERIFIED` / `UNVERIFIED - <reason>` / `FAILED`;
    /// upstream limits are UNVERIFIED (fails under `OPENTALK_LIVE_REQUIRE=1`), a bad key or
    /// request fails.
    #[test]
    #[ignore = "network: OpenRouter free model"]
    fn live_stt_test_button_openrouter() {
        const TEST: &str = "live_stt_test_button_openrouter";
        if std::env::var("OPENTALK_SECRET_OPENROUTER").is_err() {
            return report_live(TEST, Live::Unverified("OPENTALK_SECRET_OPENROUTER not set".into()));
        }
        let mut cfg = Config::default();
        cfg.stt.provider = SttProviderId::Openrouter;
        let mut notes = Vec::new();
        for model in FREE_MODELS {
            assert!(model.ends_with(":free"), "live tests may only use :free models");
            match judge(&cfg, model, &[0, 3, 8], std::thread::sleep) {
                Live::Unverified(why) => notes.push(why),
                done => return report_live(TEST, done),
            }
        }
        report_live(TEST, Live::Unverified(notes.join("; ")));
    }
}
