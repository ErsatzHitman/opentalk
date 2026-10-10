//! Cleanup: deterministic filler removal options, optional AI cleanup (provider, key, model) and
//! Prompt Studio (view / edit / reset the prompt, try it on sample text).
use super::{SettingsState, api_key_field, group};
use crate::core::config::{FillerMode, LlmProviderId};
use crate::core::secrets::SecretName;
use crate::llm::registry::{self, PROVIDERS};
use crate::ui::fonts;
use crate::ui::widgets::{self as w, Btn, Variant};
use crate::ui::{Env, icons};
use eframe::egui::{self, Ui, vec2};
use std::sync::mpsc::{Receiver, channel};
use std::time::Instant;

pub const SAMPLE_TEXT: &str = "um so i think we should uh ship the new release on friday you know because the the client is waiting";

#[derive(Clone, Debug, PartialEq)]
pub enum CleanOutcome {
    Ok { text: String, latency_ms: u64 },
    Err(String),
}

pub struct CleanupState {
    /// Editable prompt text; `None` until first shown (initialised from the saved prompt).
    prompt_buf: Option<String>,
    default_prompt: Option<String>,
    sample: String,
    rx: Option<Receiver<CleanOutcome>>,
    outcome: Option<CleanOutcome>,
}

impl Default for CleanupState {
    fn default() -> Self {
        Self { prompt_buf: None, default_prompt: None, sample: SAMPLE_TEXT.to_string(), rx: None, outcome: None }
    }
}

const PROMPT_UNAVAILABLE: &str = "(the default cleanup prompt is not available in this build)";

/// What the editor shows: the saved custom prompt, else the default.
pub fn editor_text(custom: &str, default: &str) -> String {
    if custom.trim().is_empty() { default.to_string() } else { custom.to_string() }
}

/// What to store after an edit: nothing (use the default) when the text equals the default.
pub fn stored_prompt(edited: &str, default: &str) -> String {
    if edited.trim().is_empty() || edited.trim() == default.trim() { String::new() } else { edited.to_string() }
}

pub fn filler_index(m: FillerMode) -> usize {
    match m {
        FillerMode::Off => 0,
        FillerMode::Standard => 1,
        FillerMode::Aggressive => 2,
    }
}

pub fn filler_from_index(i: usize) -> FillerMode {
    [FillerMode::Off, FillerMode::Standard, FillerMode::Aggressive][i.min(2)]
}

pub fn show(ui: &mut Ui, env: &mut Env, st: &mut SettingsState) -> bool {
    let p = *env.p;
    let mut changed = false;
    if let Some(rx) = &st.cleanup.rx {
        if let Ok(o) = rx.try_recv() {
            st.cleanup.outcome = Some(o);
            st.cleanup.rx = None;
        }
    }

    w::section_title(ui, &p, "Text cleanup", "Always on and instant: runs on your device before anything is pasted.");
    w::panel(ui, &p, |ui| {
        w::row(ui, &p, "Filler word removal", "Removes um, uh, stutters and false starts. Aggressive also drops \"basically\", \"actually\" and similar.", 230.0, |ui| {
            let mut sel = filler_index(env.cfg.filler_mode);
            if w::segmented(ui, &p, &mut sel, &["Off", "Standard", "Aggressive"]) {
                env.cfg.filler_mode = filler_from_index(sel);
                changed = true;
            }
        });
        w::divider(ui, &p);
        changed |= super::preferences::toggle_row(ui, &p, "Fuzzy dictionary correction", "Fix near-misses of your dictionary words, for example \"open whisper\" to \"OpenWhispr\".", &mut env.cfg.fuzzy_dictionary);
        w::divider(ui, &p);
        changed |= super::preferences::toggle_row(ui, &p, "Spoken line breaks", "Turn \"new line\" and \"new paragraph\" into real line breaks.", &mut env.cfg.spoken_line_breaks);
    });

    group(ui, &p, "AI cleanup", "An optional language model polishes punctuation and grammar after the on-device pass. It adds a moment of latency; failures fall back to the on-device result.");
    w::panel(ui, &p, |ui| {
        changed |= super::preferences::toggle_row(ui, &p, "Enable AI cleanup", "Send the transcript (never the audio) to the model below.", &mut env.cfg.ai_cleanup.enabled);
    });
    if !env.cfg.ai_cleanup.enabled {
        return changed;
    }

    ui.add_space(14.0);
    let provider = env.cfg.ai_cleanup.provider;
    let labels: Vec<(&str, &str)> = PROVIDERS.iter().map(|pi| (pi.name, if pi.enterprise { "Enterprise" } else { "" })).collect();
    let sel = PROVIDERS.iter().position(|pi| pi.id == provider).unwrap_or(0);
    if let Some(i) = w::pill_tabs(ui, &p, sel, &labels) {
        if PROVIDERS[i].id != provider {
            env.cfg.ai_cleanup.provider = PROVIDERS[i].id;
            st.key_edit = None;
            st.cleanup.outcome = None;
            changed = true;
        }
    }
    let provider = env.cfg.ai_cleanup.provider;
    let info = registry::provider(provider);

    ui.add_space(14.0);
    w::panel(ui, &p, |ui| {
        w::panel_pad(ui, 16, 14, |ui| {
            ui.spacing_mut().item_spacing = vec2(0.0, 12.0);
            match provider {
                LlmProviderId::Bedrock => {
                    changed |= api_key_field(ui, env, st, SecretName::AWS_ACCESS_KEY_ID, "Access key ID", "");
                    changed |= api_key_field(ui, env, st, SecretName::AWS_SECRET_ACCESS_KEY, "Secret access key", "");
                }
                LlmProviderId::Vertex => {
                    changed |= api_key_field(ui, env, st, SecretName::VERTEX_SERVICE_ACCOUNT_JSON, "Service account JSON", "");
                }
                _ => {
                    changed |= api_key_field(ui, env, st, info.secret, "API key", info.key_url);
                }
            }
        });
    });

    // Provider specific fields.
    let cfgc = &mut env.cfg.ai_cleanup;
    match provider {
        LlmProviderId::Custom => {
            ui.add_space(10.0);
            w::panel(ui, &p, |ui| {
                w::row_stacked(ui, &p, "Base URL", "An OpenAI-compatible chat completions endpoint (LAN or self-hosted).", |ui| {
                    let wd = ui.available_width();
                    changed |= w::text_input(ui, &p, &mut cfgc.custom_base_url, "http://localhost:11434/v1", wd, false).changed();
                });
            });
        }
        LlmProviderId::AzureOpenai => {
            ui.add_space(10.0);
            w::panel(ui, &p, |ui| {
                let rows = [
                    ("Endpoint", "", "https://my-resource.openai.azure.com"),
                    ("Deployment", "", "gpt-5-mini"),
                    ("API version", "", "2024-10-21"),
                    ("Underlying model (optional)", "The model behind the deployment. Set it when the deployment name does not contain it (e.g. a deployment named \"cleanup\" serving gpt-5), so the right request parameters are used.", "gpt-5-mini"),
                ];
                for (i, (label, desc, hint)) in rows.into_iter().enumerate() {
                    if i > 0 {
                        w::divider(ui, &p);
                    }
                    w::row_stacked(ui, &p, label, desc, |ui| {
                        let wd = ui.available_width();
                        let buf = match i {
                            0 => &mut cfgc.azure_endpoint,
                            1 => &mut cfgc.azure_deployment,
                            2 => &mut cfgc.azure_api_version,
                            _ => &mut cfgc.azure_model,
                        };
                        changed |= w::text_input(ui, &p, buf, hint, wd, false).changed();
                    });
                }
            });
        }
        LlmProviderId::Bedrock => {
            ui.add_space(10.0);
            w::panel(ui, &p, |ui| {
                w::row_stacked(ui, &p, "Region", "", |ui| {
                    let wd = ui.available_width();
                    changed |= w::text_input(ui, &p, &mut cfgc.bedrock_region, "us-east-1", wd, false).changed();
                });
            });
        }
        LlmProviderId::Vertex => {
            ui.add_space(10.0);
            w::panel(ui, &p, |ui| {
                w::row_stacked(ui, &p, "Project ID", "", |ui| {
                    let wd = ui.available_width();
                    changed |= w::text_input(ui, &p, &mut cfgc.vertex_project, "my-gcp-project", wd, false).changed();
                });
                w::divider(ui, &p);
                w::row_stacked(ui, &p, "Location", "", |ui| {
                    let wd = ui.available_width();
                    changed |= w::text_input(ui, &p, &mut cfgc.vertex_location, "us-central1", wd, false).changed();
                });
            });
        }
        _ => {}
    }

    // Model: free text with suggestions.
    if provider != LlmProviderId::AzureOpenai {
        group(ui, &p, "Model", "Type any model id, or pick a suggestion.");
        let mut model = env.cfg.llm_model();
        w::panel(ui, &p, |ui| {
            w::panel_pad(ui, 16, 14, |ui| {
                ui.spacing_mut().item_spacing = vec2(0.0, 10.0);
                let wd = ui.available_width();
                let r = w::text_input(ui, &p, &mut model, "Model id", wd, false);
                if r.changed() {
                    env.cfg.ai_cleanup.models.insert(provider, model.clone());
                    changed = true;
                }
                if !info.models.is_empty() {
                    let sugg: Vec<(&str, &str)> = info.models.iter().map(|m| (*m, "")).collect();
                    let cur = info.models.iter().position(|m| *m == model).unwrap_or(usize::MAX);
                    if let Some(i) = w::pill_tabs(ui, &p, cur, &sugg) {
                        env.cfg.ai_cleanup.models.insert(provider, info.models[i].to_string());
                        changed = true;
                    }
                }
            });
        });
    }

    changed |= prompt_studio(ui, env, st);
    changed
}

fn prompt_studio(ui: &mut Ui, env: &mut Env, st: &mut SettingsState) -> bool {
    let p = *env.p;
    let mut changed = false;
    group(ui, &p, "Prompt Studio", "The instructions given to the model. Edit them to change tone or rules; reset to restore the built-in prompt.");
    let lang = env.cfg.language.clone();
    let default = st
        .cleanup
        .default_prompt
        .get_or_insert_with(|| crate::ui::safe(|| crate::llm::prompt::system_prompt("", &lang, &[])).unwrap_or_else(|| PROMPT_UNAVAILABLE.to_string()))
        .clone();
    let buf = st.cleanup.prompt_buf.get_or_insert_with(|| editor_text(&env.cfg.ai_cleanup.custom_prompt, &default));
    let is_custom = !env.cfg.ai_cleanup.custom_prompt.trim().is_empty();
    w::panel(ui, &p, |ui| {
        w::panel_pad(ui, 16, 14, |ui| {
            ui.spacing_mut().item_spacing = vec2(0.0, 10.0);
            ui.horizontal(|ui| {
                w::chip(ui, &p, if is_custom { "Custom prompt" } else { "Built-in prompt" }, if is_custom { w::ChipKind::Primary } else { w::ChipKind::Neutral });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if w::button(ui, &p, Btn::new("Reset to default", Variant::Flat).icon(icons::UNDO).small().enabled(is_custom)).clicked() {
                        *buf = default.clone();
                        env.cfg.ai_cleanup.custom_prompt.clear();
                        changed = true;
                    }
                });
            });
            let r = w::text_area(ui, &p, buf, "", 12, true);
            if r.changed() {
                env.cfg.ai_cleanup.custom_prompt = stored_prompt(buf, &default);
                changed = true;
            }
        });
    });

    ui.add_space(14.0);
    w::text_semi(ui, "Try it", 12.5, p.foreground);
    ui.add_space(6.0);
    let busy = st.cleanup.rx.is_some();
    w::text_area(ui, &p, &mut st.cleanup.sample, "Sample text", 3, false);
    ui.add_space(8.0);
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 10.0;
        let ok = !busy && !st.cleanup.sample.trim().is_empty();
        if w::button(ui, &p, Btn::new(if busy { "Running..." } else { "Run on sample" }, Variant::Primary).icon(icons::PLAY).small().enabled(ok)).clicked() {
            let (tx, rx) = channel();
            st.cleanup.rx = Some(rx);
            st.cleanup.outcome = None;
            let cfg = env.cfg.clone();
            let text = st.cleanup.sample.clone();
            let ctx = ui.ctx().clone();
            let spawned = std::thread::Builder::new().name("llm-test".into()).spawn(move || {
                let _ = tx.send(run_sample(&cfg, &text));
                ctx.request_repaint();
            });
            if spawned.is_err() {
                st.cleanup.rx = None;
            }
        }
        if busy {
            ui.add(egui::Spinner::new().size(16.0).color(p.primary));
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(100));
        }
    });
    match st.cleanup.outcome.clone() {
        Some(CleanOutcome::Ok { text, latency_ms }) => {
            ui.add_space(10.0);
            result(ui, &p, true, &format!("{latency_ms} ms"), &text);
        }
        Some(CleanOutcome::Err(e)) => {
            ui.add_space(10.0);
            result(ui, &p, false, "Failed", &e);
        }
        None => {}
    }
    changed
}

fn result(ui: &mut Ui, p: &crate::ui::theme::Palette, ok: bool, badge: &str, body: &str) {
    let col = if ok { p.success } else { p.destructive };
    w::notice(ui, p, col, |ui| {
        ui.spacing_mut().item_spacing = vec2(0.0, 4.0);
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            w::text_semi(ui, if ok { "Result" } else { "Error" }, 12.0, col);
            w::text(ui, badge, 12.0, p.muted_a(90));
        });
        ui.label(egui::RichText::new(body).font(fonts::regular(13.0)).color(p.foreground));
    });
}

/// Deterministic pass, then the configured LLM (what the daemon does), timed.
pub fn run_sample(cfg: &crate::core::config::Config, text: &str) -> CleanOutcome {
    let started = Instant::now();
    let hints = super::super::dictionary::hint_words(&cfg.dictionary, &cfg.snippets);
    match crate::ui::safe(|| crate::llm::cleanup(cfg, text, &hints)) {
        Some(Ok(t)) => CleanOutcome::Ok { text: t, latency_ms: started.elapsed().as_millis() as u64 },
        Some(Err(e)) => CleanOutcome::Err(match e {
            crate::llm::LlmError::NotConfigured => "Not configured: add an API key and a model.".into(),
            crate::llm::LlmError::Timeout => "The model did not answer in time.".into(),
            crate::llm::LlmError::Http { status, detail } => format!("HTTP {status}: {detail}"),
            crate::llm::LlmError::Network(m) => format!("Network error: {m}"),
            crate::llm::LlmError::InvalidOutput(m) => format!("The reply was rejected: {m}"),
        }),
        None => CleanOutcome::Err("AI cleanup is not available in this build".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn editor_shows_custom_or_default() {
        assert_eq!(editor_text("", "DEF"), "DEF");
        assert_eq!(editor_text("  ", "DEF"), "DEF");
        assert_eq!(editor_text("mine", "DEF"), "mine");
    }

    #[test]
    fn storing_default_text_clears_the_custom_prompt() {
        assert_eq!(stored_prompt("DEF\n", "DEF"), "");
        assert_eq!(stored_prompt("", "DEF"), "");
        assert_eq!(stored_prompt("changed", "DEF"), "changed");
    }

    #[test]
    fn filler_index_roundtrip() {
        for m in [FillerMode::Off, FillerMode::Standard, FillerMode::Aggressive] {
            assert_eq!(filler_from_index(filler_index(m)), m);
        }
    }
}
