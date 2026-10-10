//! Dictionary + Snippets page. Words feed the STT prompt; snippets expand spoken triggers.
//! Parsing, import/export and the prompt-budget warning are pure (unit-tested below).
use super::fonts;
use super::icons;
use super::theme::mix;
use super::widgets::{self as w, Btn, ChipKind, Variant};
use super::{Cmd, Env};
use crate::core::config::{Config, Snippet, SttProviderId};
use eframe::egui::{self, Align, Color32, Layout, Margin, Sense, Ui, vec2};

// ---------------------------------------------------------------- pure logic

/// Split free text on commas, semicolons and newlines; trims, drops empties and duplicates
/// (case-insensitive, first spelling wins).
pub fn parse_words(input: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for part in input.split([',', ';', '\n', '\r']) {
        let t = part.trim();
        if t.is_empty() {
            continue;
        }
        if seen.insert(t.to_lowercase()) {
            out.push(t.to_string());
        }
    }
    out
}

/// Add `new` to `words`, skipping case-insensitive duplicates. Returns how many were added.
pub fn add_words(words: &mut Vec<String>, new: &[String]) -> usize {
    // One lowercase set instead of a pairwise scan: a bulk import of thousands of words would
    // otherwise allocate O(n^2) lowercase strings on the UI thread.
    let mut seen: std::collections::HashSet<String> = words.iter().map(|o| o.to_lowercase()).collect();
    let mut added = 0;
    for n in new {
        if seen.insert(n.to_lowercase()) {
            words.push(n.clone());
            added += 1;
        }
    }
    added
}

/// Indices of words containing `query` (case-insensitive); everything for an empty query.
pub fn filter_words(words: &[String], query: &str) -> Vec<usize> {
    let q = query.trim().to_lowercase();
    words.iter().enumerate().filter(|(_, w)| q.is_empty() || w.to_lowercase().contains(&q)).map(|(i, _)| i).collect()
}

/// Characters of the prompt string `words.join(", ")`.
pub fn prompt_chars(words: &[String]) -> usize {
    if words.is_empty() { 0 } else { words.iter().map(|w| w.chars().count()).sum::<usize>() + (words.len() - 1) * 2 }
}

/// Dictionary words + snippet triggers, deduplicated (what the provider actually receives).
pub fn hint_words(dictionary: &[String], snippets: &[Snippet]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    add_words(&mut out, dictionary);
    let triggers: Vec<String> = snippets.iter().map(|s| s.trigger.trim().to_string()).filter(|t| !t.is_empty()).collect();
    add_words(&mut out, &triggers);
    out
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Budget {
    pub provider: &'static str,
    pub max_chars: Option<usize>,
    pub max_terms: Option<usize>,
    /// Provider ignores the dictionary entirely.
    pub unused: bool,
}

/// Prompt budget of the active STT provider/model (docs/PLAN.md 4a, OpenWhispr dictionaryPromptCap).
pub fn budget(cfg: &Config) -> Budget {
    let info = crate::stt::registry::provider(cfg.stt.provider);
    let model = cfg.stt_model();
    let (max_chars, max_terms, unused) = match cfg.stt.provider {
        SttProviderId::Openai if model.starts_with("whisper") => (Some(900), None, false),
        SttProviderId::Openai => (Some(8000), None, false),
        SttProviderId::Groq => (Some(890), None, false),
        SttProviderId::Xai | SttProviderId::Mistral | SttProviderId::Deepgram | SttProviderId::Assemblyai => (None, Some(100), false),
        SttProviderId::Gemini | SttProviderId::Openrouter => (Some(8000), None, false),
        SttProviderId::Tinfoil | SttProviderId::Custom => (Some(900), None, false),
        SttProviderId::Corti => (None, None, true),
    };
    Budget { provider: info.name, max_chars, max_terms, unused }
}

/// How many of `words` (in order) fit the budget.
pub fn fitting_count(words: &[String], b: &Budget) -> usize {
    if b.unused {
        return 0;
    }
    let mut n = words.len();
    if let Some(t) = b.max_terms {
        n = n.min(t);
    }
    if let Some(c) = b.max_chars {
        let mut used = 0usize;
        let mut fit = 0usize;
        for w in &words[..n] {
            let add = w.chars().count() + if fit == 0 { 0 } else { 2 };
            if used + add > c {
                break;
            }
            used += add;
            fit += 1;
        }
        n = fit;
    }
    n
}

/// Warning shown under the list when the active provider cannot take every word.
pub fn budget_warning(cfg: &Config) -> Option<String> {
    let b = budget(cfg);
    let words = hint_words(&cfg.dictionary, &cfg.snippets);
    if words.is_empty() {
        return None;
    }
    if b.unused {
        return Some(format!("{} does not use a custom dictionary, so these words only help the local cleanup step.", b.provider));
    }
    let fit = fitting_count(&words, &b);
    if fit < words.len() {
        let limit = match (b.max_chars, b.max_terms) {
            (Some(c), _) => format!("{c} characters"),
            (None, Some(t)) => format!("{t} terms"),
            _ => "its limit".into(),
        };
        Some(format!("{} accepts about {limit} of hints, so only the first {fit} of {} words are sent. Remove rarely used words to stay under the limit.", b.provider, words.len()))
    } else {
        None
    }
}

/// Parse an imported file: JSON (`["a","b"]`, `{"dictionary":[..],"snippets":[{trigger,replacement}]}`,
/// or a bare snippet array) or plain text split on commas/newlines.
pub fn parse_import(text: &str) -> (Vec<String>, Vec<Snippet>) {
    let t = text.trim_start_matches('\u{feff}').trim();
    if t.starts_with('[') || t.starts_with('{') {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(t) {
            let mut words = Vec::new();
            let mut snippets = Vec::new();
            collect_json(&v, &mut words, &mut snippets);
            return (words, snippets);
        }
    }
    (parse_words(t), Vec::new())
}

fn collect_json(v: &serde_json::Value, words: &mut Vec<String>, snippets: &mut Vec<Snippet>) {
    match v {
        serde_json::Value::Array(a) => {
            for x in a {
                collect_json(x, words, snippets);
            }
        }
        serde_json::Value::String(s) => {
            let mut parsed = parse_words(s);
            words.append(&mut parsed);
        }
        serde_json::Value::Object(m) => {
            if let (Some(t), Some(r)) = (m.get("trigger").and_then(|x| x.as_str()), m.get("replacement").and_then(|x| x.as_str())) {
                snippets.push(Snippet { trigger: t.to_string(), replacement: r.to_string() });
            }
            for key in ["dictionary", "words", "snippets"] {
                if let Some(x) = m.get(key) {
                    collect_json(x, words, snippets);
                }
            }
        }
        _ => {}
    }
}

pub fn export_words_json(words: &[String]) -> String {
    serde_json::to_string_pretty(&serde_json::json!({ "dictionary": words })).unwrap_or_default()
}

pub fn export_words_txt(words: &[String]) -> String {
    words.join("\n")
}

pub fn export_snippets_json(snippets: &[Snippet]) -> String {
    serde_json::to_string_pretty(&serde_json::json!({ "snippets": snippets })).unwrap_or_default()
}

// ---------------------------------------------------------------- UI

#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub enum Tab {
    #[default]
    Dictionary,
    Snippets,
}

#[derive(Default)]
pub struct DictState {
    pub tab: Tab,
    input: String,
    bulk_open: bool,
    bulk_text: String,
    editing: Option<(usize, String)>,
    confirm_clear: bool,
    // snippets
    trig: String,
    repl: String,
    edit_snip: Option<(usize, String, String)>,
    confirm_clear_snippets: bool,
}

/// States the headless renderer shows that are otherwise one click away.
#[cfg(test)]
impl DictState {
    pub(super) fn show_snippets(&mut self) {
        self.tab = Tab::Snippets;
    }
    pub(super) fn open_bulk(&mut self, text: &str) {
        self.bulk_open = true;
        self.bulk_text = text.to_string();
    }
    pub(super) fn edit_word(&mut self, i: usize, text: &str) {
        self.editing = Some((i, text.to_string()));
    }
    pub(super) fn edit_snippet(&mut self, i: usize, trigger: &str, replacement: &str) {
        self.edit_snip = Some((i, trigger.to_string(), replacement.to_string()));
    }
}

pub fn show(ui: &mut Ui, env: &mut Env, st: &mut DictState) -> bool {
    let p = *env.p;
    let mut changed = false;
    let avail = ui.available_width();
    let content_w = (avail - 48.0).clamp(300.0, 976.0);
    let side = ((avail - content_w) / 2.0).max(0.0);
    egui::ScrollArea::vertical().id_salt("dict-scroll").auto_shrink([false, false]).show(ui, |ui| {
        ui.horizontal_top(|ui| {
            ui.add_space(side);
            ui.vertical(|ui| {
                ui.set_width(content_w);
                ui.spacing_mut().item_spacing = vec2(0.0, 0.0);
                ui.add_space(24.0);
                let sel = if st.tab == Tab::Dictionary { 0 } else { 1 };
                if let Some(i) = w::pill_tabs(ui, &p, sel, &[("Dictionary", ""), ("Snippets", "")]) {
                    st.tab = if i == 0 { Tab::Dictionary } else { Tab::Snippets };
                }
                ui.add_space(20.0);
                match st.tab {
                    Tab::Dictionary => changed |= dictionary_tab(ui, env, st),
                    Tab::Snippets => changed |= snippets_tab(ui, env, st),
                }
                ui.add_space(32.0);
            });
        });
    });
    changed
}

fn add_input(ui: &mut Ui, env: &mut Env, st: &mut DictState) -> bool {
    let p = *env.p;
    let mut changed = false;
    let width = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(vec2(width, 40.0), Sense::hover());
    let id_hint = "Add words separated by commas...";
    ui.scope_builder(egui::UiBuilder::new().max_rect(rect), |ui| {
        let r = w::text_input_h(ui, &p, &mut st.input, id_hint, width, 40.0, false, true);
        if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
            changed |= commit_words(env, st);
            r.request_focus();
        }
    });
    // Right-hand controls: Add | import
    let add_active = !parse_words(&st.input).is_empty();
    let import_r = egui::Rect::from_center_size(egui::pos2(rect.max.x - 22.0, rect.center().y), vec2(24.0, 24.0));
    let imp = ui.interact(import_r, ui.id().with("dict-import-toggle"), Sense::click());
    w::pointer(&imp);
    w::paint_icon(ui, import_r, icons::UPLOAD, 13.0, if imp.hovered() || st.bulk_open { p.foreground } else { p.muted_a(65) });
    if imp.clicked() {
        st.bulk_open = !st.bulk_open;
    }
    imp.on_hover_text("Import words");
    let font = fonts::semibold(12.0);
    let add_w = w::measure(ui, "Add", font.clone()).x + 20.0;
    let add_r = egui::Rect::from_min_size(egui::pos2(import_r.min.x - 10.0 - add_w, rect.center().y - 12.0), vec2(add_w, 24.0));
    let add = ui.interact(add_r, ui.id().with("dict-add"), Sense::click());
    if add_active {
        w::pointer(&add);
    }
    let col = if add_active { mix(p.muted_a(75), p.primary, w::hover_anim(ui, add.id.with("h"), add.hovered())) } else { p.muted_a(50) };
    ui.painter().text(add_r.center(), egui::Align2::CENTER_CENTER, "Add", font, col);
    ui.painter().rect_filled(egui::Rect::from_min_size(egui::pos2(add_r.min.x - 5.0, rect.center().y - 8.0), vec2(1.0, 16.0)), 0.0, p.divider());
    if add.clicked() && add_active {
        changed |= commit_words(env, st);
    }
    changed
}

fn commit_words(env: &mut Env, st: &mut DictState) -> bool {
    let new = parse_words(&st.input);
    if new.is_empty() {
        return false;
    }
    let n = add_words(&mut env.cfg.dictionary, &new);
    st.input.clear();
    env.cmds.push(Cmd::Toast(match n {
        0 => "Already in your dictionary".into(),
        1 => "Added 1 word".into(),
        n => format!("Added {n} words"),
    }));
    n > 0
}

fn dictionary_tab(ui: &mut Ui, env: &mut Env, st: &mut DictState) -> bool {
    let p = *env.p;
    let mut changed = false;
    if env.cfg.dictionary.is_empty() && !st.bulk_open {
        w::panel(ui, &p, |ui| {
            w::panel_pad(ui, 24, 22, |ui| {
                w::text(ui, "Teach OpenTalk your words", 20.0, p.foreground);
                ui.add_space(10.0);
                w::text(ui, "Names, products and jargon that speech recognition tends to get wrong. They are sent to the speech provider as hints and used to fix capitalisation after transcription.", 14.0, p.muted_a(90));
                ui.add_space(20.0);
                ui.horizontal(|ui| {
                    ui.set_max_width(520.0);
                    ui.vertical(|ui| {
                        ui.set_width(520.0f32.min(ui.available_width()));
                        changed |= add_input(ui, env, st);
                    });
                });
            });
        });
        return changed;
    }
    changed |= add_input(ui, env, st);
    ui.add_space(12.0);
    if st.bulk_open {
        changed |= bulk_panel(ui, env, st);
        ui.add_space(12.0);
    }
    // list header
    let total = env.cfg.dictionary.len();
    let shown = filter_words(&env.cfg.dictionary, &st.input);
    ui.horizontal(|ui| {
        ui.set_min_height(28.0);
        w::text_semi(ui, format!("YOUR DICTIONARY ({total})"), 11.0, p.muted_a(85));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.spacing_mut().item_spacing.x = 12.0;
            let ex = w::icon_button(ui, &p, icons::DOWNLOAD, 24.0, 12.0, p.foreground, p.fg_a(6)).on_hover_text("Export dictionary");
            if ex.clicked() {
                export_dictionary(env);
            }
            if total > 0 && w::button(ui, &p, Btn::new("Clear all", Variant::Ghost).small()).clicked() {
                st.confirm_clear = true;
            }
        });
    });
    ui.add_space(8.0);
    if shown.is_empty() {
        w::panel(ui, &p, |ui| {
            w::panel_pad(ui, 16, 28, |ui| {
                ui.vertical_centered(|ui| {
                    w::text(ui, format!("No words match \"{}\". Press Enter to add it.", st.input.trim()), 12.5, p.muted_a(90));
                });
            });
        });
    } else {
        let mut remove: Option<usize> = None;
        let mut begin_edit: Option<usize> = None;
        w::panel(ui, &p, |ui| {
            for (k, &i) in shown.iter().enumerate() {
                if k > 0 {
                    w::divider(ui, &p);
                }
                let editing_this = matches!(&st.editing, Some((ei, _)) if *ei == i);
                let (full, _resp) = ui.allocate_exact_size(vec2(ui.available_width(), 38.0), Sense::hover());
                let rect = full.shrink2(vec2(16.0, 0.0));
                let hot = ui.rect_contains_pointer(full);
                if hot && !editing_this {
                    ui.painter().rect_filled(full, w::edge_rounding(k == 0, k + 1 == shown.len()), p.wash(0));
                }
                if editing_this {
                    let mut done = false;
                    let mut cancel = false;
                    if let Some((_, buf)) = st.editing.as_mut() {
                        ui.scope_builder(egui::UiBuilder::new().max_rect(egui::Rect::from_min_size(rect.min + vec2(0.0, 5.0), vec2(rect.width() - 70.0, 28.0))), |ui| {
                            let r = w::text_input_h(ui, &p, buf, "Word", rect.width() - 70.0, 28.0, false, false);
                            r.request_focus();
                            if r.lost_focus() {
                                if ui.input(|i| i.key_pressed(egui::Key::Escape)) { cancel = true } else { done = true }
                            }
                        });
                    }
                    let ok = egui::Rect::from_center_size(egui::pos2(rect.max.x - 46.0, rect.center().y), vec2(24.0, 24.0));
                    let okr = ui.interact(ok, ui.id().with(("dict-ok", i)), Sense::click());
                    w::pointer(&okr);
                    w::paint_icon(ui, ok, icons::CHECK, 12.0, p.success);
                    let cx = egui::Rect::from_center_size(egui::pos2(rect.max.x - 18.0, rect.center().y), vec2(24.0, 24.0));
                    let cxr = ui.interact(cx, ui.id().with(("dict-cx", i)), Sense::click());
                    w::pointer(&cxr);
                    w::paint_icon(ui, cx, icons::CLOSE, 11.0, p.muted_a(80));
                    if okr.clicked() { done = true }
                    if cxr.clicked() { cancel = true }
                    if done {
                        if let Some((ei, buf)) = st.editing.take() {
                            let t = buf.trim().to_string();
                            if !t.is_empty() && env.cfg.dictionary.get(ei).map(|o| o != &t).unwrap_or(false) {
                                env.cfg.dictionary[ei] = t;
                                changed = true;
                            }
                        }
                    } else if cancel {
                        st.editing = None;
                    }
                } else {
                    ui.painter().text(egui::pos2(rect.min.x, rect.center().y), egui::Align2::LEFT_CENTER, &env.cfg.dictionary[i], fonts::regular(13.0), p.foreground);
                    let a = ui.ctx().animate_bool_with_time(ui.id().with(("dict-row", i)), hot, 0.12);
                    if a > 0.0 {
                        let er = egui::Rect::from_center_size(egui::pos2(rect.max.x - 28.0, rect.center().y), vec2(24.0, 24.0));
                        let dr = egui::Rect::from_center_size(egui::pos2(rect.max.x - 2.0, rect.center().y), vec2(24.0, 24.0));
                        let e = ui.interact(er, ui.id().with(("dict-edit", i)), Sense::click());
                        let d = ui.interact(dr, ui.id().with(("dict-del", i)), Sense::click());
                        w::pointer(&e);
                        w::pointer(&d);
                        w::paint_icon(ui, er, icons::EDIT, 12.0, w::with_alpha_mul(if e.hovered() { p.foreground } else { p.muted_a(85) }, a));
                        w::paint_icon(ui, dr, icons::DELETE, 12.0, w::with_alpha_mul(if d.hovered() { p.destructive } else { p.muted_a(85) }, a));
                        if e.clicked() {
                            begin_edit = Some(i);
                        }
                        if d.clicked() {
                            remove = Some(i);
                        }
                    }
                }
            }
        });
        if let Some(i) = begin_edit {
            st.editing = Some((i, env.cfg.dictionary[i].clone()));
        }
        if let Some(i) = remove {
            env.cfg.dictionary.remove(i);
            changed = true;
        }
    }
    ui.add_space(14.0);
    let chars = prompt_chars(&hint_words(&env.cfg.dictionary, &env.cfg.snippets));
    w::text(ui, format!("{} characters in the speech prompt.", super::history::thousands(chars)), 12.0, p.muted_a(85));
    if let Some(msg) = budget_warning(env.cfg) {
        ui.add_space(8.0);
        w::notice(ui, &p, p.warning, |ui| {
            ui.horizontal_top(|ui| {
                let (ir, _) = ui.allocate_exact_size(vec2(14.0, 16.0), Sense::hover());
                w::paint_icon(ui, ir, icons::WARNING, 12.0, p.warning);
                w::text(ui, msg, 12.0, if p.dark { p.warning } else { mix(p.warning, Color32::BLACK, 0.3) });
            });
        });
    }

    if st.confirm_clear {
        match w::confirm_dialog(ui.ctx(), &p, "confirm-clear-dict", "Clear your dictionary?", "All custom words will be removed.", "Clear all", true) {
            w::DialogResult::Confirm => {
                env.cfg.dictionary.clear();
                st.confirm_clear = false;
                changed = true;
            }
            w::DialogResult::Cancel => st.confirm_clear = false,
            w::DialogResult::Open => {}
        }
    }
    changed
}

fn bulk_panel(ui: &mut Ui, env: &mut Env, st: &mut DictState) -> bool {
    let p = *env.p;
    let mut changed = false;
    w::panel(ui, &p, |ui| {
        w::panel_pad(ui, 12, 10, |ui| {
            w::text_area(ui, &p, &mut st.bulk_text, "Paste words separated by commas or new lines...", 3, false);
            ui.add_space(6.0);
            let pending = parse_words(&st.bulk_text).len();
            ui.horizontal(|ui| {
                ui.set_min_height(28.0);
                let ready = if pending > 0 { format!("{pending} words ready") } else { "Separate with commas or new lines".into() };
                w::text(ui, ready, 12.0, if pending > 0 { p.success } else { p.muted_a(85) });
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.spacing_mut().item_spacing.x = 8.0;
                    if w::button(ui, &p, Btn::new("Import", Variant::Primary).small().enabled(pending > 0)).clicked() {
                        let n = add_words(&mut env.cfg.dictionary, &parse_words(&st.bulk_text));
                        st.bulk_text.clear();
                        st.bulk_open = false;
                        env.cmds.push(Cmd::Toast(format!("Imported {n} words")));
                        changed = true;
                    }
                    if w::button(ui, &p, Btn::new("Cancel", Variant::Flat).small()).clicked() {
                        st.bulk_text.clear();
                        st.bulk_open = false;
                    }
                    if w::button(ui, &p, Btn::new("From file...", Variant::Outline).icon(icons::FOLDER).small()).clicked() {
                        if let Some(path) = super::filedialog::open_file() {
                            match read_import_file(&path) {
                                Ok(txt) => {
                                    let (words, snips) = parse_import(&txt);
                                    let n = add_words(&mut env.cfg.dictionary, &words);
                                    let s = merge_snippets(&mut env.cfg.snippets, snips);
                                    st.bulk_open = false;
                                    env.cmds.push(Cmd::Toast(if s > 0 { format!("Imported {n} words and {s} snippets") } else { format!("Imported {n} words") }));
                                    changed |= n + s > 0;
                                }
                                Err(e) => env.cmds.push(Cmd::Toast(format!("Could not read the file: {e}"))),
                            }
                        }
                    }
                });
            });
        });
    });
    changed
}

/// Largest dictionary/snippet file we import (a real word list is a few KB).
const MAX_IMPORT_BYTES: u64 = 4 * 1024 * 1024;

fn read_import_file(path: &str) -> std::io::Result<String> {
    let len = std::fs::metadata(path)?.len();
    if len > MAX_IMPORT_BYTES {
        return Err(std::io::Error::other("the file is larger than 4 MB"));
    }
    std::fs::read_to_string(path)
}

/// Append imported snippets, trimming triggers and skipping blank ones and triggers already in
/// use (case-insensitive, as the editor enforces). Returns how many were added.
pub fn merge_snippets(list: &mut Vec<Snippet>, new: Vec<Snippet>) -> usize {
    let mut added = 0;
    for sn in new {
        let trigger = sn.trigger.trim().to_string();
        if trigger.is_empty() || sn.replacement.trim().is_empty() || trigger_taken(list, usize::MAX, &trigger) {
            continue;
        }
        list.push(Snippet { trigger, replacement: sn.replacement });
        added += 1;
    }
    added
}

fn export_dictionary(env: &mut Env) {
    let Some(path) = super::filedialog::save_file("opentalk-dictionary.json") else { return };
    let body = if path.to_lowercase().ends_with(".txt") { export_words_txt(&env.cfg.dictionary) } else { export_words_json(&env.cfg.dictionary) };
    match std::fs::write(&path, body) {
        Ok(()) => env.cmds.push(Cmd::Toast("Dictionary exported".into())),
        Err(e) => env.cmds.push(Cmd::Toast(format!("Export failed: {e}"))),
    }
}

// ---------------------------------------------------------------- snippets

fn snippets_tab(ui: &mut Ui, env: &mut Env, st: &mut DictState) -> bool {
    let p = *env.p;
    let mut changed = false;
    w::text(ui, "Snippets expand a short spoken trigger into longer text, for example \"my email\" into your address.", 13.0, p.muted_a(80));
    ui.add_space(14.0);
    // create panel
    w::panel(ui, &p, |ui| {
        w::panel_pad(ui, 16, 14, |ui| {
            ui.spacing_mut().item_spacing = vec2(0.0, 8.0);
            w::text_semi(ui, "New snippet", 12.5, p.foreground);
            let wd = ui.available_width();
            w::text_input(ui, &p, &mut st.trig, "Trigger phrase, e.g. my email", wd, false);
            w::text_area(ui, &p, &mut st.repl, "Replacement text", 3, false);
            ui.horizontal(|ui| {
                let ok = !st.trig.trim().is_empty() && !st.repl.trim().is_empty();
                if w::button(ui, &p, Btn::new("Add snippet", Variant::Primary).icon(icons::ADD).small().enabled(ok)).clicked() {
                    let t = st.trig.trim().to_string();
                    if env.cfg.snippets.iter().any(|s| s.trigger.to_lowercase() == t.to_lowercase()) {
                        env.cmds.push(Cmd::Toast("A snippet with that trigger already exists".into()));
                    } else {
                        env.cfg.snippets.push(Snippet { trigger: t, replacement: st.repl.clone() });
                        st.trig.clear();
                        st.repl.clear();
                        changed = true;
                    }
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.spacing_mut().item_spacing.x = 8.0;
                    if w::button(ui, &p, Btn::new("Export", Variant::Flat).icon(icons::DOWNLOAD).small().enabled(!env.cfg.snippets.is_empty())).clicked() {
                        if let Some(path) = super::filedialog::save_file("opentalk-snippets.json") {
                            match std::fs::write(&path, export_snippets_json(&env.cfg.snippets)) {
                                Ok(()) => env.cmds.push(Cmd::Toast("Snippets exported".into())),
                                Err(e) => env.cmds.push(Cmd::Toast(format!("Export failed: {e}"))),
                            }
                        }
                    }
                });
            });
        });
    });
    ui.add_space(20.0);
    if env.cfg.snippets.is_empty() {
        ui.vertical_centered(|ui| {
            ui.add_space(24.0);
            w::text(ui, "No snippets yet", 13.5, p.muted_a(85));
        });
        return changed;
    }
    ui.horizontal(|ui| {
        ui.set_min_height(28.0);
        w::text_semi(ui, format!("YOUR SNIPPETS ({})", env.cfg.snippets.len()), 11.0, p.muted_a(85));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if w::button(ui, &p, Btn::new("Clear all", Variant::Ghost).small()).clicked() {
                st.confirm_clear_snippets = true;
            }
        });
    });
    ui.add_space(8.0);
    let mut remove: Option<usize> = None;
    let mut edit: Option<usize> = None;
    w::panel(ui, &p, |ui| {
        for i in 0..env.cfg.snippets.len() {
            if i > 0 {
                w::divider(ui, &p);
            }
            let sn = env.cfg.snippets[i].clone();
            let id = ui.id().with(("snip", i));
            let hot_prev: bool = ui.data(|d| d.get_temp(id)).unwrap_or(false);
            let a = ui.ctx().animate_bool_with_time(id.with("a"), hot_prev, 0.12);
            let r = egui::Frame::new().inner_margin(Margin::symmetric(16, 10)).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    let tw = (ui.available_width() * 0.28).clamp(100.0, 240.0);
                    ui.allocate_ui_with_layout(vec2(tw, 22.0), Layout::left_to_right(Align::Center), |ui| {
                        ui.set_min_size(vec2(tw, 22.0));
                        w::chip(ui, &p, &truncate(&sn.trigger, 28), ChipKind::Primary);
                    });
                    w::text(ui, "\u{2192}", 12.0, p.muted_a(55));
                    let rw = ui.available_width() - 64.0;
                    ui.allocate_ui_with_layout(vec2(rw.max(40.0), 22.0), Layout::left_to_right(Align::Center), |ui| {
                        w::text_nowrap(ui, one_line(&sn.replacement), fonts::regular(12.5), p.fg_a(70));
                    });
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.spacing_mut().item_spacing.x = 2.0;
                        ui.scope(|ui| {
                            ui.set_opacity(a);
                            if w::icon_button(ui, &p, icons::DELETE, 24.0, 12.0, p.destructive, p.destructive_a(8)).clicked() {
                                remove = Some(i);
                            }
                            if w::icon_button(ui, &p, icons::EDIT, 24.0, 12.0, p.foreground, p.fg_a(6)).clicked() {
                                edit = Some(i);
                            }
                        });
                    });
                });
            });
            let hot = ui.rect_contains_pointer(r.response.rect);
            ui.data_mut(|d| d.insert_temp(id, hot));
        }
    });
    if let Some(i) = remove {
        env.cfg.snippets.remove(i);
        changed = true;
    }
    if let Some(i) = edit {
        let s = &env.cfg.snippets[i];
        st.edit_snip = Some((i, s.trigger.clone(), s.replacement.clone()));
    }
    changed |= snippet_edit_dialog(ui.ctx(), env, st);
    if st.confirm_clear_snippets {
        match w::confirm_dialog(ui.ctx(), &p, "confirm-clear-snips", "Clear all snippets?", "Every snippet will be removed.", "Clear all", true) {
            w::DialogResult::Confirm => {
                env.cfg.snippets.clear();
                st.confirm_clear_snippets = false;
                changed = true;
            }
            w::DialogResult::Cancel => st.confirm_clear_snippets = false,
            w::DialogResult::Open => {}
        }
    }
    changed
}

fn snippet_edit_dialog(ctx: &egui::Context, env: &mut Env, st: &mut DictState) -> bool {
    let p = *env.p;
    let Some((idx, mut trig, mut repl)) = st.edit_snip.take() else { return false };
    let mut changed = false;
    let mut keep_open = true;
    let modal = w::glass_modal(ctx, &p, egui::Id::new("edit-snippet"), 24, |ui| {
        ui.set_width(440.0);
        ui.spacing_mut().item_spacing = vec2(0.0, 8.0);
        w::text_semi(ui, "Edit snippet", 15.0, p.foreground);
        let wd = ui.available_width();
        w::text_input(ui, &p, &mut trig, "Trigger phrase", wd, false);
        w::text_area(ui, &p, &mut repl, "Replacement text", 4, false);
        if trigger_taken(&env.cfg.snippets, idx, &trig) {
            w::text(ui, "Another snippet already uses that trigger.", 12.0, p.destructive);
        }
        ui.add_space(4.0);
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            let clash = trigger_taken(&env.cfg.snippets, idx, &trig);
            let ok = !trig.trim().is_empty() && !repl.trim().is_empty() && !clash;
            if w::button(ui, &p, Btn::new("Save", Variant::Primary).min_width(84.0).enabled(ok)).clicked() {
                if let Some(s) = env.cfg.snippets.get_mut(idx) {
                    s.trigger = trig.trim().to_string();
                    s.replacement = repl.clone();
                    changed = true;
                }
                keep_open = false;
            }
            if w::button(ui, &p, Btn::new("Cancel", Variant::Outline).min_width(84.0)).clicked() {
                keep_open = false;
            }
        });
    });
    if modal.should_close() {
        keep_open = false;
    }
    if keep_open {
        st.edit_snip = Some((idx, trig, repl));
    }
    changed
}

/// True when another snippet (not `skip`) already uses `trigger` (case-insensitive).
fn trigger_taken(snippets: &[Snippet], skip: usize, trigger: &str) -> bool {
    let t = trigger.trim().to_lowercase();
    !t.is_empty() && snippets.iter().enumerate().any(|(i, s)| i != skip && s.trigger.trim().to_lowercase() == t)
}

fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max { s.to_string() } else { format!("{}...", s.chars().take(max - 1).collect::<String>()) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sn(t: &str, r: &str) -> Snippet {
        Snippet { trigger: t.into(), replacement: r.into() }
    }

    #[test]
    fn parse_words_splits_trims_and_dedupes() {
        assert_eq!(parse_words(" OpenAI, Priya ;kubectl\n\nopenai , "), vec!["OpenAI", "Priya", "kubectl"]);
        assert!(parse_words(" , ;\n").is_empty());
    }

    #[test]
    fn merge_snippets_trims_and_skips_blank_or_taken_triggers() {
        let mut list = vec![Snippet { trigger: "My Email".into(), replacement: "a@b.c".into() }];
        let new = vec![
            Snippet { trigger: "  sig  ".into(), replacement: "Best, A".into() },
            Snippet { trigger: "   ".into(), replacement: "x".into() },
            Snippet { trigger: "blank".into(), replacement: "  ".into() },
            Snippet { trigger: "my email".into(), replacement: "dup".into() },
            Snippet { trigger: "SIG".into(), replacement: "dup within import".into() },
        ];
        assert_eq!(merge_snippets(&mut list, new), 1);
        assert_eq!(list.len(), 2);
        assert_eq!(list[1].trigger, "sig");
    }

    #[test]
    fn add_words_skips_duplicates_case_insensitively() {
        let mut w = vec!["OpenAI".to_string()];
        assert_eq!(add_words(&mut w, &["openai".into(), "Groq".into(), "groq".into()]), 1);
        assert_eq!(w, vec!["OpenAI", "Groq"]);
    }

    #[test]
    fn filter_is_case_insensitive_substring() {
        let w: Vec<String> = ["OpenWhispr", "Priya", "whisper"].iter().map(|s| s.to_string()).collect();
        assert_eq!(filter_words(&w, "WHISP"), vec![0, 2]);
        assert_eq!(filter_words(&w, ""), vec![0, 1, 2]);
        assert!(filter_words(&w, "zzz").is_empty());
    }

    #[test]
    fn prompt_chars_counts_separators() {
        assert_eq!(prompt_chars(&[]), 0);
        assert_eq!(prompt_chars(&["ab".into()]), 2);
        assert_eq!(prompt_chars(&["ab".into(), "cd".into(), "e".into()]), 2 + 2 + 2 + 2 + 1);
    }

    #[test]
    fn hints_merge_snippet_triggers() {
        let h = hint_words(&["Priya".into()], &[sn("my email", "x@y.z"), sn("priya", "dup"), sn("  ", "blank")]);
        assert_eq!(h, vec!["Priya", "my email"]);
    }

    #[test]
    fn budget_by_provider() {
        let mut cfg = Config::default();
        cfg.stt.provider = SttProviderId::Groq;
        assert_eq!(budget(&cfg).max_chars, Some(890));
        cfg.stt.provider = SttProviderId::Openai;
        cfg.stt.models.insert(SttProviderId::Openai, "whisper-1".into());
        assert_eq!(budget(&cfg).max_chars, Some(900));
        cfg.stt.models.insert(SttProviderId::Openai, "gpt-4o-transcribe".into());
        assert_eq!(budget(&cfg).max_chars, Some(8000));
        cfg.stt.provider = SttProviderId::Xai;
        assert_eq!(budget(&cfg).max_terms, Some(100));
        cfg.stt.provider = SttProviderId::Corti;
        assert!(budget(&cfg).unused);
    }

    #[test]
    fn fitting_respects_chars_and_terms() {
        let words: Vec<String> = (0..5).map(|_| "abcd".to_string()).collect(); // 4 each, 2 sep
        let b = Budget { provider: "t", max_chars: Some(14), max_terms: None, unused: false };
        assert_eq!(fitting_count(&words, &b), 2, "4 + 2 + 4 = 10, a third word would need 16");
        let b = Budget { provider: "t", max_chars: None, max_terms: Some(3), unused: false };
        assert_eq!(fitting_count(&words, &b), 3);
        let b = Budget { provider: "t", max_chars: None, max_terms: None, unused: true };
        assert_eq!(fitting_count(&words, &b), 0);
    }

    #[test]
    fn warning_only_when_words_are_dropped() {
        let mut cfg = Config::default();
        cfg.stt.provider = SttProviderId::Groq;
        cfg.dictionary = vec!["Priya".into()];
        assert!(budget_warning(&cfg).is_none());
        cfg.dictionary = (0..400).map(|i| format!("term{i:03}")).collect();
        let msg = budget_warning(&cfg).expect("over the 890 character budget");
        assert!(msg.contains("Groq") && msg.contains("890"), "{msg}");
        cfg.stt.provider = SttProviderId::Corti;
        assert!(budget_warning(&cfg).unwrap().contains("does not use"));
        cfg.dictionary.clear();
        assert!(budget_warning(&cfg).is_none());
    }

    #[test]
    fn import_formats() {
        assert_eq!(parse_import("a, b\nc"), (vec!["a".into(), "b".into(), "c".into()], vec![]));
        assert_eq!(parse_import(r#"["x","y, z"]"#).0, vec!["x", "y", "z"]);
        let (w, s) = parse_import(r#"{"dictionary":["Priya"],"snippets":[{"trigger":"sig","replacement":"Best, A"}]}"#);
        assert_eq!(w, vec!["Priya"]);
        assert_eq!(s, vec![sn("sig", "Best, A")]);
        let (w, s) = parse_import("\u{feff}[{\"trigger\":\"t\",\"replacement\":\"r\"}]");
        assert!(w.is_empty());
        assert_eq!(s, vec![sn("t", "r")]);
        // Broken JSON falls back to plain text.
        assert_eq!(parse_import("[oops").0, vec!["[oops"]);
    }

    #[test]
    fn export_roundtrips() {
        let words = vec!["A".to_string(), "B c".to_string()];
        assert_eq!(parse_import(&export_words_json(&words)).0, words);
        assert_eq!(parse_import(&export_words_txt(&words)).0, words);
        let snips = vec![sn("t", "multi\nline")];
        assert_eq!(parse_import(&export_snippets_json(&snips)).1, snips);
    }

    #[test]
    fn edited_trigger_may_not_clash_with_another_snippet() {
        let list = vec![sn("my email", "a"), sn("sig", "b")];
        assert!(trigger_taken(&list, 1, " My Email "));
        assert!(!trigger_taken(&list, 0, "my email"), "a snippet may keep its own trigger");
        assert!(!trigger_taken(&list, 0, "   "));
    }

    #[test]
    fn display_helpers() {
        assert_eq!(one_line("a\n  b\tc"), "a b c");
        assert_eq!(truncate("abcdef", 4), "abc...");
        assert_eq!(truncate("abc", 4), "abc");
    }
}
