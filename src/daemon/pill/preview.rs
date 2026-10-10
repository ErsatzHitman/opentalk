//! `--render-pill` CLI, the scripted scenes behind the golden images, and a small benchmark.
//!
//! Scenes are built exactly like the live daemon would drive the model (mode changes, 60 fps ticks
//! with a synthetic microphone level), so a golden at `t_ms` is what the window shows `t_ms` after
//! the state was entered.

use super::PillMode;
use super::model::{AppBubble, Pointer, RenderState};
use super::render::{pixel_size, render_frame, render_into};
use super::theme::Palette;
use crate::core::config::PanelPosition;
use tiny_skia::Pixmap;

/// Frame interval of the simulated animation clock.
const FRAME_MS: f64 = 1000.0 / 60.0;
/// How long the pre-roll recording runs before the state under test begins.
const PREROLL_MS: f64 = 5000.0;

pub const STATES: &[&str] = &[
    "idle",
    "hover",
    "tooltip",
    "preparing",
    "recording",
    "recording-tooltip",
    "thinking",
    "unavailable",
    "cancel",
    "cancel-hot",
    "error",
    "error-noretry",
    "error-long",
];

#[derive(Clone, Debug)]
pub struct Scene {
    pub state: String,
    pub t_ms: f64,
    pub dark: bool,
    pub scale: f32,
    /// Straight RGBA icon for the app bubble: (name, rgba, size).
    pub icon: Option<(String, Vec<u8>, u32)>,
    /// Constant microphone RMS, or a synthetic speech-like signal when `None`.
    pub level: Option<f32>,
    pub position: PanelPosition,
}

impl Scene {
    pub fn new(state: &str, t_ms: f64) -> Scene {
        Scene { state: state.to_string(), t_ms, dark: false, scale: 1.0, icon: None, level: None, position: PanelPosition::BottomRight }
    }
}

/// Deterministic speech-like RMS signal (syllable bursts with pauses).
pub fn synthetic_level(t_ms: f64) -> f32 {
    let slot = (t_ms / 80.0).floor() as i64;
    // cheap integer hash -> 0..1
    let mut h = (slot as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    h ^= h >> 29;
    h = h.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    h ^= h >> 32;
    let r = (h & 0xffff) as f32 / 65535.0;
    // phrase envelope: ~1.1 s phrases with short pauses
    let phase = t_ms.rem_euclid(1100.0) as f32 / 1100.0;
    let env = if phase < 0.82 { (std::f32::consts::PI * phase / 0.82).sin().powf(0.6) } else { 0.04 };
    (0.004 + r * 0.16 * env).max(0.0)
}

fn run(st: &mut RenderState, from: f64, to: f64, level: &dyn Fn(f64) -> f32) {
    let mut t = from;
    while t <= to + 1e-6 {
        st.tick(t, level(t));
        t += FRAME_MS;
    }
}

/// Build the model for a scene; returns it with the time at which to render.
pub fn build(scene: &Scene) -> Result<(RenderState, f64), String> {
    let pal = Palette::get(scene.dark);
    let mut st = RenderState::new(pal, scene.scale, scene.position);
    st.hotkey_label = "Ctrl + Win".into();
    let level_const = scene.level;
    let level = move |t: f64| level_const.unwrap_or_else(|| synthetic_level(t));
    let hover = Pointer { group: true, pill: false, bubble: false, cancel: false };
    let tip = Pointer { group: true, pill: true, bubble: false, cancel: false };
    let t = scene.t_ms;
    if let Some((name, rgba, size)) = &scene.icon {
        let b = AppBubble::new(name, rgba, *size, scene.scale).ok_or("bad icon")?;
        st.set_app(Some(b), -PREROLL_MS - 1.0);
    }
    match scene.state.as_str() {
        "idle" => {}
        "hover" => {
            st.set_pointer(hover, -1000.0);
            st.settle();
        }
        "tooltip" => {
            st.set_pointer(tip, -1000.0);
            st.settle();
        }
        "preparing" => st.set_mode(PillMode::Preparing, 0.0),
        "recording" => {
            st.set_mode(PillMode::Recording, 0.0);
            run(&mut st, 0.0, t, &level);
        }
        "recording-tooltip" => {
            st.set_mode(PillMode::Recording, -PREROLL_MS);
            run(&mut st, -PREROLL_MS, 0.0, &level);
            st.settle();
            st.set_pointer(tip, 0.0);
        }
        "thinking" | "unavailable" | "cancel" | "cancel-hot" => {
            st.set_mode(PillMode::Recording, -PREROLL_MS);
            run(&mut st, -PREROLL_MS, 0.0, &level);
            st.settle();
            match scene.state.as_str() {
                "thinking" => st.set_mode(PillMode::Thinking, 0.0),
                "unavailable" => st.set_mode(PillMode::Unavailable, 0.0),
                "cancel" => st.set_pointer(hover, 0.0),
                _ => st.set_pointer(Pointer { group: true, pill: false, bubble: false, cancel: true }, 0.0),
            }
        }
        "error" => st.error = Some(super::card::ErrorCard::new("Transcription failed", "The speech provider rejected your API key. Check it in Settings.", true, 0.0)),
        "error-noretry" => st.error = Some(super::card::ErrorCard::new("No microphone found", "Connect a microphone and try again.", false, 0.0)),
        "error-long" => {
            st.error = Some(super::card::ErrorCard::new(
                "Couldn't paste the transcript",
                "The target app is running as administrator, so Windows blocked the paste. The text is on your clipboard: press Ctrl+V to paste it manually. Run OpenTalk as administrator to paste into elevated apps.",
                true,
                0.0,
            ))
        }
        other => return Err(format!("unknown state '{other}' (expected one of: {})", STATES.join(", "))),
    }
    // keep simulating the mic for states that continue after the pre-roll
    if matches!(scene.state.as_str(), "recording-tooltip") {
        run(&mut st, 0.0, t, &level);
    }
    Ok((st, t))
}

pub fn render_scene(scene: &Scene) -> Result<Pixmap, String> {
    let (st, t) = build(scene)?;
    render_frame(&st, t).ok_or_else(|| "pill window size is not representable".to_string())
}

// --- icons ------------------------------------------------------------------------------------

/// Load an icon for the bubble: `.png` fixture, or an exe/ico path (via `appicon`, with a local
/// fallback so the CLI works before the shared loader lands).
pub fn load_icon(path: &str, px: u32) -> Option<(String, Vec<u8>, u32)> {
    let name = std::path::Path::new(path).file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
    if path.to_ascii_lowercase().ends_with(".png") {
        let pm = Pixmap::load_png(path).ok()?;
        if pm.width() != pm.height() {
            return None;
        }
        let size = pm.width();
        let rgba = pm.take_demultiplied();
        return Some((name, rgba, size));
    }
    let shared = std::panic::catch_unwind(|| super::super::appicon::load_path(path, px)).ok().flatten();
    if let Some(a) = shared {
        return Some((a.name, a.rgba, a.size));
    }
    let (rgba, size) = extract_icon_rgba(path, px as i32)?;
    Some((name, rgba, size))
}

/// Fallback: PrivateExtractIconsW + GetDIBits -> straight RGBA.
fn extract_icon_rgba(path: &str, px: i32) -> Option<(Vec<u8>, u32)> {
    use windows::Win32::Graphics::Gdi::{BITMAP, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, DeleteObject, GetDC, GetDIBits, GetObjectW, HGDIOBJ, ReleaseDC};
    use windows::Win32::UI::WindowsAndMessaging::{DestroyIcon, GetIconInfo, HICON, ICONINFO, PrivateExtractIconsW};
    let mut wpath = [0u16; 260];
    for (d, s) in wpath.iter_mut().zip(path.encode_utf16().take(259)) {
        *d = s;
    }
    let mut icons = [HICON::default(); 1];
    let mut ids = [0u32; 1];
    // SAFETY: buffers are sized for one icon; every GDI/user object is released before returning.
    unsafe {
        let n = PrivateExtractIconsW(&wpath, 0, px, px, Some(&mut icons), Some(ids.as_mut_ptr()), 0);
        if n == 0 || n == u32::MAX || icons[0].is_invalid() {
            return None;
        }
        let icon = icons[0];
        let mut info = ICONINFO::default();
        if GetIconInfo(icon, &mut info).is_err() {
            let _ = DestroyIcon(icon);
            return None;
        }
        let result = (|| {
            let mut bm = BITMAP::default();
            if GetObjectW(HGDIOBJ(info.hbmColor.0), std::mem::size_of::<BITMAP>() as i32, Some(&mut bm as *mut _ as *mut core::ffi::c_void)) == 0 {
                return None;
            }
            let (w, h) = (bm.bmWidth, bm.bmHeight);
            let mut bmi = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: w,
                    biHeight: -h,
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut buf = vec![0u8; (w * h * 4) as usize];
            let dc = GetDC(None);
            let got = GetDIBits(dc, info.hbmColor, 0, h as u32, Some(buf.as_mut_ptr() as *mut core::ffi::c_void), &mut bmi, DIB_RGB_COLORS);
            let mut mask = vec![0u8; (w * h * 4) as usize];
            let got_mask = GetDIBits(dc, info.hbmMask, 0, h as u32, Some(mask.as_mut_ptr() as *mut core::ffi::c_void), &mut bmi, DIB_RGB_COLORS);
            ReleaseDC(None, dc);
            if got == 0 || w != h {
                return None;
            }
            let has_alpha = buf.chunks_exact(4).any(|p| p[3] != 0);
            for (i, p) in buf.chunks_exact_mut(4).enumerate() {
                let (b, g, r) = (p[0], p[1], p[2]);
                let a = if has_alpha {
                    p[3]
                } else if got_mask != 0 && mask[i * 4] != 0 {
                    0
                } else {
                    255
                };
                p[0] = r;
                p[1] = g;
                p[2] = b;
                p[3] = a;
            }
            Some((buf, w as u32))
        })();
        let _ = DeleteObject(HGDIOBJ(info.hbmColor.0));
        let _ = DeleteObject(HGDIOBJ(info.hbmMask.0));
        let _ = DestroyIcon(icon);
        result
    }
}

// --- CLI --------------------------------------------------------------------------------------

fn parse_f32(args: &[String], flag: &str) -> Option<f32> {
    args.iter().position(|a| a == flag).and_then(|i| args.get(i + 1)).and_then(|v| v.parse().ok())
}

fn parse_str<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
    args.iter().position(|a| a == flag).and_then(|i| args.get(i + 1)).map(String::as_str)
}

fn composite_preview(pm: &Pixmap, up: u32, bg: Option<u32>) -> Pixmap {
    let (w, h) = (pm.width(), pm.height());
    let mut out = Pixmap::new(w * up, h * up).expect("preview size");
    let bg = bg.map(|v| [((v >> 16) & 255) as f32, ((v >> 8) & 255) as f32, (v & 255) as f32]);
    let src = pm.data();
    let dst = out.data_mut();
    for y in 0..h * up {
        for x in 0..w * up {
            let si = (((y / up) * w + (x / up)) * 4) as usize;
            let di = ((y * w * up + x) * 4) as usize;
            let a = src[si + 3] as f32 / 255.0;
            match bg {
                Some(b) => {
                    for c in 0..3 {
                        dst[di + c] = (src[si + c] as f32 + b[c] * (1.0 - a) + 0.5).min(255.0) as u8;
                    }
                    dst[di + 3] = 255;
                }
                None => dst[di..di + 4].copy_from_slice(&src[si..si + 4]),
            }
        }
    }
    out
}

/// `--render-pill <state> <t_ms> <out.png> [--dark] [--scale s] [--app-icon path] [--level 0..1]
///   [--position bottom-right|bottom-left|center] [--upscale N] [--bg RRGGBB] [--dump-icon out.png]`
/// `--render-pill bench 0 -` prints render timings.
pub fn cli_render(args: &[String]) -> i32 {
    if args.len() < 3 {
        eprintln!("usage: --render-pill <state> <t_ms> <out.png> [--dark] [--scale s] [--app-icon path] [--level 0..1]");
        eprintln!("states: {}, bench", STATES.join(", "));
        return 2;
    }
    let state = args[0].as_str();
    // scenes are simulated frame by frame, so bound the clock (10 min)
    let t_ms: f64 = match args[1].parse::<f64>() {
        Ok(v) if (0.0..=600_000.0).contains(&v) => v,
        _ => {
            eprintln!("invalid t_ms '{}' (expected 0..=600000)", args[1]);
            return 2;
        }
    };
    let out_path = &args[2];
    let rest = &args[3..];
    let mut scene = Scene::new(state, t_ms);
    scene.dark = rest.iter().any(|a| a == "--dark");
    scene.scale = parse_f32(rest, "--scale").unwrap_or(1.0).clamp(0.5, 8.0);
    scene.level = parse_f32(rest, "--level").map(|v| v.clamp(0.0, 1.0));
    scene.position = match parse_str(rest, "--position") {
        Some("bottom-left") => PanelPosition::BottomLeft,
        Some("center") | Some("bottom-center") => PanelPosition::BottomCenter,
        _ => PanelPosition::BottomRight,
    };
    if let Some(p) = parse_str(rest, "--app-icon") {
        match load_icon(p, 48) {
            Some(icon) => {
                if let Some(dump) = parse_str(rest, "--dump-icon") {
                    if let Some(mut pm) = Pixmap::new(icon.2, icon.2) {
                        for (d, s) in pm.data_mut().chunks_exact_mut(4).zip(icon.1.chunks_exact(4)) {
                            let a = s[3] as u32;
                            d[0] = ((s[0] as u32 * a + 127) / 255) as u8;
                            d[1] = ((s[1] as u32 * a + 127) / 255) as u8;
                            d[2] = ((s[2] as u32 * a + 127) / 255) as u8;
                            d[3] = s[3];
                        }
                        let _ = pm.save_png(dump);
                    }
                }
                scene.icon = Some(icon);
            }
            None => {
                eprintln!("could not load icon '{p}'");
                return 1;
            }
        }
    }
    if state == "bench" {
        return bench(&scene);
    }
    let pm = match render_scene(&scene) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("{e}");
            return 2;
        }
    };
    let pm = match parse_str(rest, "--crop") {
        Some(c) => {
            let v: Vec<f32> = c.split(',').filter_map(|p| p.trim().parse().ok()).collect();
            if v.len() == 4 {
                let s = scene.scale;
                let r = tiny_skia::IntRect::from_xywh((v[0] * s) as i32, (v[1] * s) as i32, (v[2] * s) as u32, (v[3] * s) as u32);
                r.and_then(|r| pm.clone_rect(r)).unwrap_or(pm)
            } else {
                pm
            }
        }
        None => pm,
    };
    let up = parse_str(rest, "--upscale").and_then(|v| v.parse::<u32>().ok()).unwrap_or(1).clamp(1, 16);
    let bg = parse_str(rest, "--bg").and_then(|v| u32::from_str_radix(v.trim_start_matches('#'), 16).ok());
    let img = if up > 1 || bg.is_some() { composite_preview(&pm, up, bg) } else { pm };
    match img.save_png(out_path) {
        Ok(()) => {
            println!("{}x{} -> {out_path}", img.width(), img.height());
            0
        }
        Err(e) => {
            eprintln!("cannot write {out_path}: {e}");
            1
        }
    }
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    sorted[((sorted.len() as f64 - 1.0) * p).round() as usize]
}

/// Render-time benchmark for the performance gates (use a release build).
fn bench(scene: &Scene) -> i32 {
    let mut results = Vec::new();
    for (name, dark) in [("recording light", false), ("recording dark", true)] {
        let mut sc = scene.clone();
        sc.state = "recording".into();
        sc.t_ms = 1500.0;
        sc.dark = dark;
        let (st, t) = match build(&sc) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("{e}");
                return 2;
            }
        };
        let (w, h) = pixel_size(&st);
        let mut pm = Pixmap::new(w, h).unwrap();
        for _ in 0..200 {
            render_into(&st, t, &mut pm);
        }
        let mut times = Vec::new();
        for _ in 0..2000 {
            let t0 = std::time::Instant::now();
            render_into(&st, t, &mut pm);
            times.push(t0.elapsed().as_secs_f64() * 1000.0);
        }
        times.sort_by(|a, b| a.total_cmp(b));
        results.push((name, percentile(&times, 0.5), percentile(&times, 0.99)));
    }
    for state in ["thinking", "cancel", "tooltip", "error"] {
        let mut sc = scene.clone();
        sc.state = state.into();
        sc.t_ms = 800.0;
        let Ok((st, t)) = build(&sc) else { continue };
        let (w, h) = pixel_size(&st);
        let mut pm = Pixmap::new(w, h).unwrap();
        for _ in 0..50 {
            render_into(&st, t, &mut pm);
        }
        let mut times = Vec::new();
        for _ in 0..500 {
            let t0 = std::time::Instant::now();
            render_into(&st, t, &mut pm);
            times.push(t0.elapsed().as_secs_f64() * 1000.0);
        }
        times.sort_by(|a, b| a.total_cmp(b));
        let label = format!("{state}");
        results.push((Box::leak(label.into_boxed_str()) as &str, percentile(&times, 0.5), percentile(&times, 0.99)));
    }
    for (n, p50, p99) in results {
        println!("{n:>18}: p50 {:.3} ms  p99 {:.3} ms", p50, p99);
    }
    0
}

#[cfg(test)]
mod golden {
    use super::*;
    use std::path::PathBuf;

    fn dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests").join("golden")
    }

    fn icon() -> Option<(String, Vec<u8>, u32)> {
        load_icon(dir().join("fixtures").join("explorer_icon.png").to_str()?, 48).map(|(_, r, s)| ("Windows Explorer".to_string(), r, s))
    }

    struct G {
        name: &'static str,
        state: &'static str,
        t: f64,
        dark: bool,
        scale: f32,
        icon: bool,
        /// Contains GDI text, so allow rasteriser differences.
        text: bool,
    }

    const fn g(name: &'static str, state: &'static str, t: f64, dark: bool, icon: bool, text: bool) -> G {
        G { name, state, t, dark, scale: 1.0, icon, text }
    }

    fn goldens() -> Vec<G> {
        let mut v = vec![
            g("idle_light", "idle", 0.0, false, false, false),
            g("idle_dark", "idle", 0.0, true, false, false),
            g("hover_light", "hover", 0.0, false, false, false),
            g("hover_dark", "hover", 0.0, true, false, false),
            g("tooltip_light", "tooltip", 0.0, false, false, true),
            g("tooltip_dark", "tooltip", 0.0, true, false, true),
            g("preparing_light_t500", "preparing", 500.0, false, false, false),
            g("preparing_dark_t1000", "preparing", 1000.0, true, false, false),
            g("recording_tooltip_light", "recording-tooltip", 0.0, false, false, true),
            g("unavailable_light_t600", "unavailable", 600.0, false, false, false),
            g("unavailable_dark_t1000", "unavailable", 1000.0, true, false, false),
            g("cancel_light_t140", "cancel", 140.0, false, false, false),
            g("cancel_light_t600", "cancel", 600.0, false, false, false),
            g("cancel_dark_t600", "cancel", 600.0, true, false, false),
            g("cancel_hot_light_t600", "cancel-hot", 600.0, false, false, false),
            g("error_light_t400", "error", 400.0, false, false, true),
            g("error_dark_t400", "error", 400.0, true, false, true),
            g("error_dark_t2000", "error", 2000.0, true, false, true),
            g("error_noretry_light", "error-noretry", 400.0, false, false, true),
            g("error_long_light", "error-long", 400.0, false, false, true),
            g("recording_icon_light_t1500", "recording", 1500.0, false, true, false),
            g("recording_icon_dark_t1500", "recording", 1500.0, true, true, false),
            g("recording_icon_light_t400", "recording", 400.0, false, true, false),
            g("thinking_icon_dark_t400", "thinking", 400.0, true, true, false),
            g("cancel_icon_light_t600", "cancel", 600.0, false, true, false),
        ];
        for (suffix, dark) in [("light", false), ("dark", true)] {
            for t in [0.0, 260.0, 560.0, 700.0, 1500.0] {
                let name: &'static str = Box::leak(format!("recording_{suffix}_t{}", t as i64).into_boxed_str());
                v.push(g(name, "recording", t, dark, false, false));
            }
            for t in [0.0, 400.0, 800.0] {
                let name: &'static str = Box::leak(format!("thinking_{suffix}_t{}", t as i64).into_boxed_str());
                v.push(g(name, "thinking", t, dark, false, false));
            }
        }
        v
    }

    fn diff_stats(a: &Pixmap, b: &Pixmap) -> (usize, u8) {
        let mut count = 0;
        let mut max = 0u8;
        for (x, y) in a.data().chunks_exact(4).zip(b.data().chunks_exact(4)) {
            let d = (0..4).map(|i| x[i].abs_diff(y[i])).max().unwrap_or(0);
            if d > 6 {
                count += 1;
            }
            max = max.max(d);
        }
        (count, max)
    }

    #[test]
    fn goldens_match() {
        let update = std::env::var("UPDATE_GOLDENS").is_ok();
        let icon = icon();
        let mut failures = Vec::new();
        for g in goldens() {
            let mut scene = Scene::new(g.state, g.t);
            scene.dark = g.dark;
            scene.scale = g.scale;
            if g.icon {
                let Some(ic) = icon.clone() else {
                    eprintln!("skipping {} (icon fixture missing)", g.name);
                    continue;
                };
                scene.icon = Some(ic);
            }
            let pm = render_scene(&scene).expect("scene");
            let path = dir().join(format!("{}.png", g.name));
            if update || !path.exists() {
                std::fs::create_dir_all(dir()).unwrap();
                pm.save_png(&path).unwrap();
                continue;
            }
            let want = Pixmap::load_png(&path).expect("golden png");
            if want.width() != pm.width() || want.height() != pm.height() {
                failures.push(format!("{}: size {}x{} != {}x{}", g.name, pm.width(), pm.height(), want.width(), want.height()));
                continue;
            }
            let (count, max) = diff_stats(&pm, &want);
            let total = (pm.width() * pm.height()) as usize;
            let allowed = if g.text { total * 3 / 100 } else { total / 500 };
            if count > allowed || (!g.text && max > 96) {
                failures.push(format!("{}: {count} px differ (max {max}, allowed {allowed})", g.name));
            }
        }
        assert!(failures.is_empty(), "golden mismatches:\n{}", failures.join("\n"));
    }

    #[test]
    fn synthetic_level_is_deterministic_and_speechlike() {
        assert_eq!(synthetic_level(1234.0), synthetic_level(1234.0));
        let vals: Vec<f32> = (0..100).map(|i| synthetic_level(i as f64 * 80.0)).collect();
        assert!(vals.iter().all(|v| (0.0..0.2).contains(v)));
        assert!(vals.iter().cloned().fold(0.0, f32::max) > 0.05);
        assert!(vals.iter().cloned().fold(1.0, f32::min) < 0.02);
    }

    #[test]
    fn unknown_state_is_an_error() {
        assert!(render_scene(&Scene::new("nope", 0.0)).is_err());
    }
}
