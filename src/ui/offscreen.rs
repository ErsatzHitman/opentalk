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
    let area = (b.x - a.x) * (c.y - a.y) - (b.y - a.y) * (c.x - a.x);
    if area.abs() < 1e-6 {
        return;
    }
    for y in miny.max(0)..maxy.min(f.h as i32) {
        for x in minx.max(0)..maxx.min(f.w as i32) {
            let p = pos2(x as f32 + 0.500_13, y as f32 + 0.500_07);
            let w0 = ((b.x - p.x) * (c.y - p.y) - (b.y - p.y) * (c.x - p.x)) / area;
            let w1 = ((c.x - p.x) * (a.y - p.y) - (c.y - p.y) * (a.x - p.x)) / area;
            let w2 = 1.0 - w0 - w1;
            if w0 < 0.0 || w1 < 0.0 || w2 < 0.0 {
                continue;
            }
            let mut col = [0.0f32; 4];
            for k in 0..4 {
                col[k] = w0 * v[0].1[k] + w1 * v[1].1[k] + w2 * v[2].1[k];
            }
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

/// Render `frames` passes of `draw` into a `size` (points) window at `ppp`. Pointer events let a
/// test hover or click; the last pass is rasterised.
pub fn render(size: egui::Vec2, ppp: f32, tail: Vec<Vec<egui::Event>>, passes: usize, mut draw: impl FnMut(&egui::Context, &mut egui::Ui)) -> Frame {
    let ctx = egui::Context::default();
    super::fonts::install(&ctx);
    let mut textures: HashMap<TextureId, Tex> = HashMap::new();
    let mut last = None;
    for pass in 0..passes {
        let mut input = egui::RawInput::default();
        input.screen_rect = Some(Rect::from_min_size(Pos2::ZERO, size));
        input.viewports.entry(egui::ViewportId::ROOT).or_default().native_pixels_per_point = Some(ppp);
        input.time = Some(pass as f64 * 0.5);
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

#[test]
fn offscreen_shots() {
    let Some(out_dir) = std::env::var_os("OPENTALK_SHOT_DIR") else { return };
    let out_dir = std::path::PathBuf::from(out_dir);
    std::fs::create_dir_all(&out_dir).unwrap();
    let home = std::env::temp_dir().join(format!("ot-offscreen-{}", std::process::id()));
    std::fs::create_dir_all(&home).unwrap();
    seed_home(&home);
    // SAFETY: this test is run alone (filtered by name) and sets the variable before any other thread reads it.
    unsafe { std::env::set_var("OPENTALK_HOME", &home) };

    let ppp = 1.25;
    let click = |x: f32, y: f32| {
        let pos = pos2(x, y);
        let m = egui::Modifiers::NONE;
        vec![
            vec![egui::Event::PointerMoved(pos)],
            vec![egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed: true, modifiers: m }],
            vec![egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed: false, modifiers: m }, egui::Event::PointerGone],
            vec![egui::Event::PointerMoved(pos)],
        ]
    };
    let hover = |x: f32, y: f32| vec![vec![egui::Event::PointerMoved(pos2(x, y))]];
    struct Sc {
        name: &'static str,
        launch: Launch,
        size: egui::Vec2,
        events: Vec<Vec<egui::Event>>,
        palette: bool,
    }
    let hist = Launch { page: Page::History, settings: None };
    let prefs = Launch { page: Page::History, settings: Some(Section::Preferences) };
    let big = vec2(1200.0, 800.0);
    let scenarios = vec![
        Sc { name: "shell-history", launch: hist, size: big, events: vec![], palette: false },
        Sc { name: "shell-dictionary", launch: Launch { page: Page::Dictionary, settings: None }, size: big, events: vec![], palette: false },
        Sc { name: "settings-preferences", launch: prefs, size: big, events: vec![], palette: false },
        Sc { name: "settings-preferences-tall", launch: prefs, size: vec2(1200.0, 1500.0), events: vec![], palette: false },
        Sc { name: "settings-stt", launch: Launch { page: Page::History, settings: Some(Section::SpeechToText) }, size: big, events: vec![], palette: false },
        Sc { name: "palette", launch: hist, size: big, events: vec![], palette: true },
        Sc { name: "support-menu", launch: hist, size: big, events: click(74.0, 717.0), palette: false },
        Sc { name: "confirm-clear", launch: hist, size: big, events: click(1122.0, 172.0), palette: false },
        Sc { name: "hover-search", launch: hist, size: big, events: hover(600.0, 33.0), palette: false },
    ];
    let only = std::env::var("OPENTALK_SHOT_ONLY").ok();
    for dark in [false, true] {
        for sc in &scenarios {
            if let Some(o) = &only {
                if !sc.name.contains(o.as_str()) {
                    continue;
                }
            }
            let cfg = Config { theme: if dark { Theme::Dark } else { Theme::Light }, ..Config::default() };
            let mut app = App::headless(sc.launch, cfg);
            if sc.palette {
                app.open_palette();
            }
            let frame = render(sc.size, ppp, sc.events.clone(), 7, |_ctx, ui| app.draw(ui));
            let file = out_dir.join(format!("{}-{}.png", sc.name, if dark { "dark" } else { "light" }));
            frame.save(&file, dark);
        }
    }
    let _ = std::fs::remove_dir_all(&home);
    let _ = vec2(0.0, 0.0);
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
