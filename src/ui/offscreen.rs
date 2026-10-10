//! Headless frame renderer for tests: runs the real UI through egui, tessellates it and rasterises
//! the triangles on the CPU (Gouraud vertex colours, texture lookups, scissor, premultiplied
//! "over"), then writes a PNG. It exists so the glass look can be checked with no window, GPU or
//! unlocked desktop. It does not emulate DWM acrylic: the window is composited over a synthetic,
//! deliberately busy "wallpaper" blurred to stand in for the acrylic.
//!
//! Writes only when `OPENTALK_SHOT_DIR` is set (otherwise the test is a no-op), e.g.
//! `OPENTALK_SHOT_DIR=docs/screenshots/glass/offscreen cargo test --bin opentalk offscreen_shots`.
use super::app::App;
use super::{Launch, Page, Section};
use crate::core::config::{Config, Theme};
use eframe::egui::{self, Color32, Pos2, Rect, TextureId, pos2, vec2};
use std::collections::HashMap;

struct Tex {
    w: usize,
    h: usize,
    px: Vec<Color32>,
}

pub struct Frame {
    pub w: usize,
    pub h: usize,
    /// Premultiplied RGBA floats.
    pub buf: Vec<[f32; 4]>,
}

/// A bright, busy stand-in for "whatever is behind the window", already blurred (acrylic).
fn wallpaper(x: f32, y: f32, w: f32, h: f32, dark: bool) -> [f32; 4] {
    let (u, v) = (x / w, y / h);
    let r = 0.55 + 0.45 * (u * 9.0 + (v * 5.0).sin() * 2.0).sin();
    let g = 0.50 + 0.45 * (v * 7.0 - u * 3.0).cos();
    let b = 0.60 + 0.40 * ((u + v) * 6.0).sin();
    let k = if dark { 0.45 } else { 1.0 };
    [r.clamp(0.0, 1.0) * k, g.clamp(0.0, 1.0) * k, b.clamp(0.0, 1.0) * k, 1.0]
}

fn c32(c: Color32) -> [f32; 4] {
    [c.r() as f32 / 255.0, c.g() as f32 / 255.0, c.b() as f32 / 255.0, c.a() as f32 / 255.0]
}

fn mul(a: [f32; 4], b: [f32; 4]) -> [f32; 4] {
    [a[0] * b[0], a[1] * b[1], a[2] * b[2], a[3] * b[3]]
}

impl Frame {
    fn blend(&mut self, x: usize, y: usize, s: [f32; 4]) {
        let d = &mut self.buf[y * self.w + x];
        let k = 1.0 - s[3];
        *d = [s[0] + d[0] * k, s[1] + d[1] * k, s[2] + d[2] * k, s[3] + d[3] * k];
    }

    pub fn save(&self, path: &std::path::Path, backdrop_dark: bool) {
        let mut pm = tiny_skia::Pixmap::new(self.w as u32, self.h as u32).expect("pixmap");
        let data = pm.data_mut();
        for (i, p) in self.buf.iter().enumerate() {
            // Composite over the wallpaper (acrylic stand-in) where the window is translucent.
            let (x, y) = ((i % self.w) as f32, (i / self.w) as f32);
            let wp = wallpaper(x, y, self.w as f32, self.h as f32, backdrop_dark);
            let k = 1.0 - p[3];
            let o = [p[0] + wp[0] * k, p[1] + wp[1] * k, p[2] + wp[2] * k];
            let q = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
            data[i * 4..i * 4 + 4].copy_from_slice(&[q(o[0]), q(o[1]), q(o[2]), 255]);
        }
        pm.save_png(path).expect("png");
    }
}

fn raster_tri(f: &mut Frame, clip: (i32, i32, i32, i32), v: [(Pos2, [f32; 4], Pos2); 3], tex: Option<&Tex>) {
    let minx = v.iter().map(|a| a.0.x).fold(f32::MAX, f32::min).floor().max(clip.0 as f32) as i32;
    let maxx = v.iter().map(|a| a.0.x).fold(f32::MIN, f32::max).ceil().min(clip.2 as f32) as i32;
    let miny = v.iter().map(|a| a.0.y).fold(f32::MAX, f32::min).floor().max(clip.1 as f32) as i32;
    let maxy = v.iter().map(|a| a.0.y).fold(f32::MIN, f32::max).ceil().min(clip.3 as f32) as i32;
    let (a, b, c) = (v[0].0, v[1].0, v[2].0);
    // Barycentrics in f64: with f32, screen-sized triangles (backdrop, scrim) lose a pixel here
    // and there along their top edge, which shows as a dotted line.
    let (ax, ay, bx, by, cx, cy) = (a.x as f64, a.y as f64, b.x as f64, b.y as f64, c.x as f64, c.y as f64);
    let area = (bx - ax) * (cy - ay) - (by - ay) * (cx - ax);
    if area.abs() < 1e-9 {
        return;
    }
    for y in miny.max(0)..maxy.min(f.h as i32) {
        for x in minx.max(0)..maxx.min(f.w as i32) {
            let (px, py) = (x as f64 + 0.500_13, y as f64 + 0.500_07);
            let w0 = ((bx - px) * (cy - py) - (by - py) * (cx - px)) / area;
            let w1 = ((cx - px) * (ay - py) - (cy - py) * (ax - px)) / area;
            let w2 = 1.0 - w0 - w1;
            if w0 < 0.0 || w1 < 0.0 || w2 < 0.0 {
                continue;
            }
            let (w0, w1, w2) = (w0 as f32, w1 as f32, w2 as f32);
            let mut col: [f32; 4] = std::array::from_fn(|k| w0 * v[0].1[k] + w1 * v[1].1[k] + w2 * v[2].1[k]);
            if let Some(t) = tex {
                let uv = pos2(w0 * v[0].2.x + w1 * v[1].2.x + w2 * v[2].2.x, w0 * v[0].2.y + w1 * v[1].2.y + w2 * v[2].2.y);
                let tx = ((uv.x * t.w as f32) as isize).clamp(0, t.w as isize - 1) as usize;
                let ty = ((uv.y * t.h as f32) as isize).clamp(0, t.h as isize - 1) as usize;
                col = mul(col, c32(t.px[ty * t.w + tx]));
            }
            if col[3] > 0.0 {
                f.blend(x as usize, y as usize, col);
            }
        }
    }
}

/// Where to look for fonts: the system directory first, then the free faces of a Linux host (the
/// tests run under Wine, whose `C:\Windows\Fonts` has no Segoe UI). Icon glyphs have no free
/// stand-in, so under Wine they are simply absent from the shots.
fn font_dirs() -> Vec<std::path::PathBuf> {
    let mut dirs = vec![super::fonts::system_dir()];
    for d in [r"Z:\usr\share\fonts\truetype\liberation", r"Z:\usr\share\fonts\truetype\dejavu", "/usr/share/fonts/truetype/liberation", "/usr/share/fonts/truetype/dejavu"] {
        dirs.push(d.into());
    }
    dirs
}

/// Render `frames` passes of `draw` into a `size` (points) window at `ppp`. Pointer events let a
/// test hover or click; the last pass is rasterised.
pub fn render(size: egui::Vec2, ppp: f32, tail: Vec<Vec<egui::Event>>, passes: usize, draw: impl FnMut(&egui::Context, &mut egui::Ui)) -> Frame {
    let (ctx, shapes, ppp, textures) = run_passes(size, ppp, tail, passes, draw);
    rasterise(&ctx, shapes, ppp, size, &textures)
}

/// The shapes of the last of `passes` frames, without rasterising them.
pub fn shapes_of(size: egui::Vec2, ppp: f32, tail: Vec<Vec<egui::Event>>, passes: usize, draw: impl FnMut(&egui::Context, &mut egui::Ui)) -> Vec<egui::epaint::ClippedShape> {
    run_passes(size, ppp, tail, passes, draw).1
}

/// Context, last frame's shapes, pixels per point and the texture atlas.
type Passes = (egui::Context, Vec<egui::epaint::ClippedShape>, f32, HashMap<TextureId, Tex>);

fn run_passes(
    size: egui::Vec2,
    ppp: f32,
    tail: Vec<Vec<egui::Event>>,
    passes: usize,
    mut draw: impl FnMut(&egui::Context, &mut egui::Ui),
) -> Passes {
    let ctx = egui::Context::default();
    super::fonts::install_from(&ctx, &font_dirs());
    let mut textures: HashMap<TextureId, Tex> = HashMap::new();
    let mut last = None;
    for pass in 0..passes {
        let mut input = egui::RawInput { screen_rect: Some(Rect::from_min_size(Pos2::ZERO, size)), time: Some(pass as f64 * 0.5), ..Default::default() };
        input.viewports.entry(egui::ViewportId::ROOT).or_default().native_pixels_per_point = Some(ppp);
        if pass + tail.len() >= passes {
            input.events = tail[pass + tail.len() - passes].clone();
        }
        let mut out = ctx.run_ui(input, |ui| draw(&ctx, ui));
        for (id, deltas) in &out.textures_delta.set {
            for d in deltas {
                let egui::ImageData::Color(img) = &d.image;
                let [w, h] = img.size;
                match d.pos {
                    None => {
                        textures.insert(*id, Tex { w, h, px: img.pixels.clone() });
                    }
                    Some([px, py]) => {
                        if let Some(t) = textures.get_mut(id) {
                            for y in 0..h {
                                for x in 0..w {
                                    t.px[(py + y) * t.w + px + x] = img.pixels[y * w + x];
                                }
                            }
                        }
                    }
                }
            }
        }
        out.textures_delta.clear();
        last = Some((out.shapes, out.pixels_per_point));
    }
    let (shapes, ppp) = last.expect("at least one pass");
    (ctx, shapes, ppp, textures)
}

fn rasterise(ctx: &egui::Context, shapes: Vec<egui::epaint::ClippedShape>, ppp: f32, size: egui::Vec2, textures: &HashMap<TextureId, Tex>) -> Frame {
    let prims = ctx.tessellate(shapes, ppp);
    let (pw, ph) = ((size.x * ppp) as usize, (size.y * ppp) as usize);
    let mut f = Frame { w: pw, h: ph, buf: vec![[0.0; 4]; pw * ph] };
    for cp in prims {
        let egui::epaint::Primitive::Mesh(mesh) = cp.primitive else { continue };
        let clip = ((cp.clip_rect.min.x * ppp).floor() as i32, (cp.clip_rect.min.y * ppp).floor() as i32, (cp.clip_rect.max.x * ppp).ceil() as i32, (cp.clip_rect.max.y * ppp).ceil() as i32);
        let tex = textures.get(&mesh.texture_id);
        for t in mesh.indices.chunks_exact(3) {
            let vs = [0, 1, 2].map(|k| {
                let v = mesh.vertices[t[k] as usize];
                (pos2(v.pos.x * ppp, v.pos.y * ppp), c32(v.color), v.uv)
            });
            raster_tri(&mut f, clip, vs, tex);
        }
    }
    f
}

fn seed_home(dir: &std::path::Path) {
    let now = crate::core::timing::unix_ms();
    let items = [
        ("Send the revised budget to Priya before the Thursday review.", "Outlook", "OUTLOOK.EXE", 5_000i64),
        ("Remind me to renew the domain and check the DNS records.", "Chrome", "chrome.exe", 60_000),
        ("Let's push the standup to ten thirty, the team is in a workshop until then.", "Slack", "slack.exe", 3_600_000),
        ("The new onboarding flow cuts three screens and keeps the permissions step.", "Notepad", "notepad.exe", 7_200_000),
        ("Draft a short reply thanking them and asking for the invoice number.", "Gmail", "chrome.exe", 90_000_000),
        ("Quarterly numbers look good, revenue up eleven percent.", "Word", "WINWORD.EXE", 100_000_000),
    ];
    let mut lines = Vec::new();
    for (i, (t, a, e, m)) in items.iter().enumerate() {
        let ts = now - m;
        lines.push(format!(
            r#"{{"id":{},"ts":{},"text":"{}","raw":"{}","status":"ok","app_name":"{}","app_exe":"{}","provider":"groq","model":"whisper-large-v3-turbo","duration_ms":4200,"latency_ms":620}}"#,
            ts + i as i64, ts, t, t.to_lowercase(), a, e
        ));
    }
    lines.push(format!(
        r#"{{"id":{},"ts":{},"text":"","raw":"","status":"failed","error":"Network error: could not reach the provider.","app_name":"Teams","app_exe":"ms-teams.exe","provider":"groq","model":"whisper-large-v3-turbo"}}"#,
        now - 8_000_000,
        now - 8_000_000
    ));
    std::fs::write(dir.join("history.jsonl"), lines.join("\n") + "\n").expect("seed history");
}

/// What a scenario starts from.
#[derive(Clone, Copy)]
struct Sc {
    name: &'static str,
    launch: Launch,
    size: egui::Vec2,
    /// Pointer events for the last passes (hover, click, wheel).
    events: fn() -> Vec<Vec<egui::Event>>,
    /// Open the Ctrl+K palette.
    palette: bool,
    /// Start with an empty history file.
    empty_history: bool,
    /// Adjust the config (dictionary words, snippets, provider, ...).
    cfg: fn(&mut Config),
    /// Put the pages into a state that is otherwise a click away.
    setup: fn(&mut App),
}

fn no_events() -> Vec<Vec<egui::Event>> {
    vec![]
}
fn keep_cfg(_: &mut Config) {}
fn keep_app(_: &mut App) {}

fn click_at(x: f32, y: f32) -> Vec<Vec<egui::Event>> {
    let pos = pos2(x, y);
    let m = egui::Modifiers::NONE;
    vec![
        vec![egui::Event::PointerMoved(pos)],
        vec![egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed: true, modifiers: m }],
        vec![egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed: false, modifiers: m }, egui::Event::PointerGone],
        vec![egui::Event::PointerMoved(pos)],
    ]
}
fn hover_at(x: f32, y: f32) -> Vec<Vec<egui::Event>> {
    vec![vec![egui::Event::PointerMoved(pos2(x, y))]]
}
/// Wheel the content under (x, y) down by `dy` points.
fn scroll_at(x: f32, y: f32, dy: f32) -> Vec<Vec<egui::Event>> {
    let wheel = egui::Event::MouseWheel { unit: egui::MouseWheelUnit::Point, delta: vec2(0.0, -dy), phase: egui::TouchPhase::Move, modifiers: egui::Modifiers::NONE };
    // A few idle passes after the wheel let the scroll animation settle.
    vec![vec![egui::Event::PointerMoved(pos2(x, y))], vec![wheel], vec![], vec![], vec![]]
}

const WORDS: [&str; 12] = ["OpenTalk", "Priya", "kubectl", "Groq", "Whisper", "Tailscale", "Figma", "Anthropic", "PostgreSQL", "Ramanujan", "Zürich", "OAuth"];

fn with_words(c: &mut Config) {
    c.dictionary = WORDS.iter().map(|w| w.to_string()).collect();
}
fn with_words_corti(c: &mut Config) {
    with_words(c);
    c.stt.provider = crate::core::config::SttProviderId::Corti;
}
fn with_snippets(c: &mut Config) {
    with_words(c);
    c.snippets = vec![
        crate::core::config::Snippet { trigger: "my email".into(), replacement: "priya.raman@example.com".into() },
        crate::core::config::Snippet { trigger: "sign off".into(), replacement: "Best regards,\nPriya".into() },
        crate::core::config::Snippet { trigger: "standup link".into(), replacement: "https://meet.example.com/standup".into() },
    ];
}
fn history_off(c: &mut Config) {
    c.history_enabled = false;
}
fn ai_cleanup_on(c: &mut Config) {
    c.ai_cleanup.enabled = true;
}
fn stt_custom(c: &mut Config) {
    c.stt.provider = crate::core::config::SttProviderId::Custom;
}

#[test]
fn offscreen_shots() {
    let Some(out_dir) = std::env::var_os("OPENTALK_SHOT_DIR") else { return };
    let out_dir = std::path::PathBuf::from(out_dir);
    std::fs::create_dir_all(&out_dir).unwrap();
    let home = std::env::temp_dir().join(format!("ot-offscreen-{}", std::process::id()));
    let empty = std::env::temp_dir().join(format!("ot-offscreen-empty-{}", std::process::id()));
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&empty).unwrap();
    seed_home(&home);

    let ppp = 1.0;
    let big = vec2(1200.0, 800.0);
    let tall = vec2(1200.0, 1000.0);
    let hist = Launch { page: Page::History, settings: None };
    let dict = Launch { page: Page::Dictionary, settings: None };
    let sett = |s| Launch { page: Page::History, settings: Some(s) };
    let base = Sc { name: "", launch: hist, size: big, events: no_events, palette: false, empty_history: false, cfg: keep_cfg, setup: keep_app };
    let scenarios = [
        Sc { name: "history", ..base },
        Sc { name: "history-empty", empty_history: true, ..base },
        Sc { name: "history-disabled", cfg: history_off, ..base },
        Sc { name: "history-row-menu", events: || click_at(1136.0, 510.0), ..base },
        Sc { name: "history-hover-search", events: || hover_at(1000.0, 148.0), ..base },
        Sc { name: "dictionary-empty", launch: dict, ..base },
        Sc { name: "dictionary-words", launch: dict, cfg: with_words, ..base },
        Sc { name: "dictionary-words-edit", launch: dict, cfg: with_words, setup: |a| a.dict_mut().edit_word(2, "kubectl"), events: || hover_at(600.0, 250.0), ..base },
        Sc { name: "dictionary-bulk", launch: dict, cfg: with_words_corti, setup: |a| a.dict_mut().open_bulk("Ravi, Ananya, Kubernetes"), ..base },
        Sc { name: "dictionary-snippets", launch: dict, cfg: with_snippets, setup: |a| a.dict_mut().show_snippets(), events: || hover_at(600.0, 520.0), ..base },
        Sc { name: "dictionary-snippet-edit", launch: dict, cfg: with_snippets, setup: |a| { a.dict_mut().show_snippets(); a.dict_mut().edit_snippet(1, "sign off", "Best regards,\nPriya"); }, ..base },
        Sc { name: "settings-preferences", launch: sett(Section::Preferences), size: tall, ..base },
        Sc { name: "settings-preferences-scrolled", launch: sett(Section::Preferences), size: tall, events: || scroll_at(700.0, 500.0, 1400.0), ..base },
        Sc { name: "settings-hotkeys", launch: sett(Section::Hotkeys), size: tall, ..base },
        Sc { name: "settings-stt", launch: sett(Section::SpeechToText), size: tall, ..base },
        Sc { name: "settings-stt-custom", launch: sett(Section::SpeechToText), size: tall, cfg: stt_custom, ..base },
        Sc { name: "settings-stt-result", launch: sett(Section::SpeechToText), size: tall, setup: |a| a.settings_mut().stt.outcome = Some(super::settings::stt::TestOutcome::Ok { text: "Hey, can you send the quarterly report to Priya by end of day? Thanks.".into(), latency_ms: 640 }), ..base },
        Sc { name: "settings-stt-error", launch: sett(Section::SpeechToText), size: tall, setup: |a| a.settings_mut().stt.outcome = Some(super::settings::stt::TestOutcome::Err("401 Unauthorized: the API key was rejected.".into())), ..base },
        Sc { name: "settings-cleanup", launch: sett(Section::Cleanup), size: tall, ..base },
        Sc { name: "settings-cleanup-ai", launch: sett(Section::Cleanup), size: tall, cfg: ai_cleanup_on, ..base },
        Sc { name: "settings-privacy", launch: sett(Section::Privacy), size: tall, ..base },
        Sc { name: "settings-system", launch: sett(Section::System), size: tall, ..base },
        Sc { name: "palette", palette: true, ..base },
        Sc { name: "support-menu", events: || click_at(74.0, 717.0), ..base },
        Sc { name: "confirm-clear", events: || click_at(1122.0, 172.0), ..base },
    ];
    let only = std::env::var("OPENTALK_SHOT_ONLY").ok();
    for dark in [false, true] {
        for sc in &scenarios {
            if let Some(o) = &only
                && !sc.name.contains(o.as_str()) {
                    continue;
                }
            // SAFETY: this test is run alone (filtered by name) and no other thread reads the variable.
            unsafe { std::env::set_var("OPENTALK_HOME", if sc.empty_history { &empty } else { &home }) };
            let mut cfg = Config { theme: if dark { Theme::Dark } else { Theme::Light }, ..Config::default() };
            (sc.cfg)(&mut cfg);
            let mut app = App::headless(sc.launch, cfg);
            (sc.setup)(&mut app);
            if sc.palette {
                app.open_palette();
            }
            let frame = render(sc.size, ppp, (sc.events)(), 7, |_ctx, ui| app.draw(ui));
            let file = out_dir.join(format!("{}-{}.png", sc.name, if dark { "dark" } else { "light" }));
            frame.save(&file, dark);
        }
    }
    let _ = std::fs::remove_dir_all(&home);
    let _ = std::fs::remove_dir_all(&empty);
}

/// The shell, rendered through the real UI code: the backdrop is uneven, the window is translucent
/// (acrylic shows through) and the glass top bar paints over the page, in both themes.
#[test]
fn offscreen_shell_has_an_uneven_translucent_backdrop() {
    for dark in [false, true] {
        let cfg = Config { theme: if dark { Theme::Dark } else { Theme::Light }, ..Config::default() };
        let mut app = App::headless(Launch { page: Page::Dictionary, settings: None }, cfg);
        // The Dictionary page needs no history file; OPENTALK_HOME is never touched by it.
        let f = render(vec2(1000.0, 700.0), 1.0, vec![], 4, |_c, ui| app.draw(ui));
        // Gutter between the window edge and the glass surfaces (x < 8): pure backdrop.
        let px = |x: usize, y: usize| f.buf[y * f.w + x];
        let samples: Vec<[f32; 4]> = (0..5).map(|i| px(3, 120 + i * 120)).collect();
        assert!(samples.iter().all(|s| s[3] > 0.7 && s[3] < 1.0), "dark={dark}: window must be translucent over acrylic: {samples:?}");
        let spread = |c: usize| samples.iter().map(|s| s[c]).fold(f32::MIN, f32::max) - samples.iter().map(|s| s[c]).fold(f32::MAX, f32::min);
        assert!(spread(0).max(spread(1)).max(spread(2)).max(spread(3)) > 0.002, "dark={dark}: backdrop is flat");
        // Inside the top bar the glass adds light on top of the backdrop (rim + tint).
        let bar = px(500, 20);
        let gutter = px(500, 3);
        assert!(bar != gutter, "dark={dark}: the top bar is drawn");
    }
}

/// A Settings section to scan: name, section, config tweak.
type SectionCase = (&'static str, Section, fn(&mut Config));

/// A page state to scan: name, launch, config tweak, app setup.
type PageCase = (&'static str, Launch, fn(&mut Config), fn(&mut App));

/// Large opaque fills in `shapes`: rectangles, circles and meshes (every visible vertex at least
/// 94 % opaque) bigger than `min_area` points squared. Small opaque things (an avatar, a toggle
/// thumb, a status dot) are fine; a big one is a surface painted over the refracted backdrop.
fn large_opaque_fills(shapes: &[egui::epaint::ClippedShape], min_area: f32) -> Vec<Rect> {
    fn walk(s: &egui::Shape, min_area: f32, out: &mut Vec<Rect>) {
        let opaque = |c: Color32| c.a() >= 240;
        match s {
            egui::Shape::Vec(v) => v.iter().for_each(|s| walk(s, min_area, out)),
            egui::Shape::Rect(r) if opaque(r.fill) && r.rect.area() > min_area => out.push(r.rect),
            egui::Shape::Circle(c) if opaque(c.fill) && std::f32::consts::PI * c.radius * c.radius > min_area => out.push(Rect::from_center_size(c.center, vec2(2.0 * c.radius, 2.0 * c.radius))),
            egui::Shape::Mesh(m) => {
                let visible: Vec<_> = m.vertices.iter().filter(|v| v.color.a() > 0).collect();
                if visible.len() >= 8 && visible.iter().all(|v| opaque(v.color)) {
                    let bounds = Rect::from_points(&visible.iter().map(|v| v.pos).collect::<Vec<_>>());
                    if bounds.area() > min_area {
                        out.push(bounds);
                    }
                }
            }
            _ => {}
        }
    }
    let mut out = Vec::new();
    for cs in shapes {
        walk(&cs.shape, min_area, &mut out);
    }
    out
}

/// Rule 3, checked on the real pages: nothing inside glass paints an opaque surface of its own. The
/// pages on the bare backdrop (History, Dictionary in every state) paint none at all; with the
/// Settings modal open the only one is the modal's own stand-in for its blurred, scrimmed backdrop
/// (glass.rs `modal_shapes`), and a section's panels, rows, inputs, chips, notices and keycaps are
/// washes. Both themes.
#[test]
fn pages_inside_glass_paint_no_opaque_surfaces() {
    use crate::core::history::{Entry, Status};
    let now = crate::core::timing::unix_ms();
    let entries = vec![
        Entry { id: 3, ts: now - 5_000, text: "Send the revised budget to Priya.".into(), raw: "send the revised budget to priya".into(), app_name: Some("Outlook".into()), provider: "groq".into(), model: "whisper".into(), latency_ms: 620, ai_cleaned: true, ..Default::default() },
        Entry { id: 2, ts: now - 60_000, text: "Remind me to renew the domain.".into(), app_name: Some("Chrome".into()), ..Default::default() },
        Entry { id: 1, ts: now - 90_000, status: Status::Failed, error: Some("Network error".into()), app_name: Some("Teams".into()), ..Default::default() },
    ];
    let page = |dark: bool, launch: Launch, cfg: fn(&mut Config), setup: fn(&mut App)| {
        let mut c = Config { theme: if dark { Theme::Dark } else { Theme::Light }, ..Config::default() };
        cfg(&mut c);
        let mut app = App::headless(launch, c);
        app.hist_mut().entries = entries.clone();
        app.hist_mut().loaded = true;
        setup(&mut app);
        let shapes = shapes_of(vec2(1200.0, 900.0), 1.0, vec![], 4, |_c, ui| app.draw(ui));
        large_opaque_fills(&shapes, 1500.0)
    };
    let hist = Launch { page: Page::History, settings: None };
    let dict = Launch { page: Page::Dictionary, settings: None };
    for dark in [false, true] {
        let name = if dark { "dark" } else { "light" };
        // Bare-backdrop pages: no opaque fill anywhere.
        let bare: [PageCase; 6] = [
            ("history", hist, keep_cfg, keep_app),
            ("history banner", hist, history_off, keep_app),
            ("dictionary empty", dict, keep_cfg, keep_app),
            ("dictionary words", dict, with_words_corti, keep_app),
            ("dictionary bulk", dict, with_words, |a| a.dict_mut().open_bulk("a, b")),
            ("dictionary snippets", dict, with_snippets, |a| a.dict_mut().show_snippets()),
        ];
        for (what, launch, cfg, setup) in bare {
            let fills = page(dark, launch, cfg, setup);
            assert!(fills.is_empty(), "{what} ({name}) paints opaque surfaces: {fills:?}");
        }
        // The snippet edit dialog is a glass modal: its stand-in is the one opaque fill.
        let fills = page(dark, dict, with_snippets, |a| {
            a.dict_mut().show_snippets();
            a.dict_mut().edit_snippet(0, "my email", "a@b.c");
        });
        assert_eq!(fills.len(), 1, "edit dialog ({name}): only the modal stand-in is opaque: {fills:?}");
        // Settings sections inside the modal.
        let sections: [SectionCase; 7] = [
            ("preferences", Section::Preferences, keep_cfg),
            ("hotkeys", Section::Hotkeys, keep_cfg),
            ("stt", Section::SpeechToText, keep_cfg),
            ("stt custom", Section::SpeechToText, stt_custom),
            ("cleanup ai", Section::Cleanup, ai_cleanup_on),
            ("privacy", Section::Privacy, keep_cfg),
            ("system", Section::System, keep_cfg),
        ];
        for (what, sec, cfg) in sections {
            let fills = page(dark, Launch { page: Page::History, settings: Some(sec) }, cfg, keep_app);
            assert_eq!(fills.len(), 1, "settings {what} ({name}): only the modal stand-in is opaque: {fills:?}");
        }
    }
}

/// Negative control for the opaque-fill scan: it does see a big opaque rectangle, circle and mesh,
/// and ignores small ones and translucent washes.
#[test]
fn the_opaque_fill_scan_sees_what_it_should() {
    let clip = Rect::EVERYTHING;
    let wrap = |shape: egui::Shape| egui::epaint::ClippedShape { clip_rect: clip, shape };
    let big = Rect::from_min_size(pos2(10.0, 10.0), vec2(200.0, 100.0));
    let small = Rect::from_min_size(pos2(10.0, 10.0), vec2(16.0, 16.0));
    let paint = |s| large_opaque_fills(&[wrap(s)], 1500.0).len();
    assert_eq!(paint(egui::Shape::rect_filled(big, 4.0, Color32::from_rgb(20, 20, 20))), 1);
    assert_eq!(paint(egui::Shape::rect_filled(big, 4.0, Color32::from_black_alpha(20))), 0, "a wash is fine");
    assert_eq!(paint(egui::Shape::rect_filled(small, 4.0, Color32::WHITE)), 0, "an avatar-sized fill is fine");
    assert_eq!(paint(egui::Shape::circle_filled(pos2(100.0, 100.0), 60.0, Color32::WHITE)), 1);
    let mut mesh = egui::epaint::Mesh::default();
    for k in 0..8 {
        mesh.colored_vertex(pos2(10.0 + 30.0 * k as f32, 10.0 + 12.0 * (k % 2) as f32 * 8.0), Color32::from_rgb(9, 9, 9));
    }
    assert_eq!(paint(egui::Shape::mesh(mesh)), 1);
}
