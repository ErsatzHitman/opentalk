//! A static page that resembles the real Settings window: 192 px sidebar, a card with a title bar,
//! switch rows and one real widget row. Drawn with `egui` only, so the three backends paint identical meshes.

use egui::{Align2, Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Rect, Stroke, StrokeKind, Ui, pos2, vec2};
use std::sync::Arc;

const SIDEBAR_W: f32 = 192.0;
const TOPBAR_H: f32 = 48.0;

/// Load Segoe UI from the Windows fonts directory (the real app does the same; egui's bundled fonts are off).
pub fn install_fonts(ctx: &egui::Context) {
    let dir = std::env::var_os("WINDIR").map(std::path::PathBuf::from).unwrap_or_else(|| r"C:\Windows".into()).join("Fonts");
    let regular = std::fs::read(dir.join("segoeui.ttf")).ok();
    let semibold = std::fs::read(dir.join("seguisb.ttf")).ok().or_else(|| regular.clone());
    let Some(regular) = regular else { return };
    let mut defs = FontDefinitions::empty();
    defs.font_data.insert("regular".into(), Arc::new(FontData::from_owned(regular)));
    defs.families.insert(FontFamily::Proportional, vec!["regular".into()]);
    defs.families.insert(FontFamily::Monospace, vec!["regular".into()]);
    let mut semi = vec![];
    if let Some(b) = semibold {
        defs.font_data.insert("semibold".into(), Arc::new(FontData::from_owned(b)));
        semi.push("semibold".to_string());
    }
    semi.push("regular".into());
    defs.families.insert(FontFamily::Name("semibold".into()), semi);
    ctx.set_fonts(defs);
}

fn semibold(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name("semibold".into()))
}

fn regular(size: f32) -> FontId {
    FontId::new(size, FontFamily::Proportional)
}

/// Draws the whole window into `ui` (which covers the viewport).
pub fn draw(ui: &mut Ui) {
    let (window, card, fg, dim, accent, line) = (
        Color32::from_rgb(24, 24, 27),
        Color32::from_rgb(32, 32, 36),
        Color32::from_gray(235),
        Color32::from_gray(150),
        Color32::from_rgb(99, 102, 241),
        Color32::from_white_alpha(18),
    );
    let screen = ui.max_rect();
    let p = ui.painter().clone();
    p.rect_filled(screen, 0.0, window);

    // Sidebar.
    let nav = ["History", "Dictionary", "Hotkeys", "Speech to text", "Cleanup", "System"];
    let mut y = screen.min.y + 48.0;
    for (i, label) in nav.iter().enumerate() {
        let r = Rect::from_min_size(pos2(screen.min.x + 8.0, y), vec2(SIDEBAR_W - 16.0, 32.0));
        if i == 3 {
            p.rect_filled(r, CornerRadius::same(8), Color32::from_white_alpha(14));
        }
        p.circle_stroke(pos2(r.min.x + 16.0, r.center().y), 6.0, Stroke::new(1.5, if i == 3 { fg } else { dim }));
        p.text(pos2(r.min.x + 34.0, r.center().y), Align2::LEFT_CENTER, label, regular(13.0), if i == 3 { fg } else { dim });
        y += 34.0;
    }
    p.text(pos2(screen.min.x + 20.0, screen.max.y - 24.0), Align2::LEFT_CENTER, "OpenTalk  v0.1.0", regular(11.0), dim);

    // Main card.
    let main = Rect::from_min_max(pos2(screen.min.x + SIDEBAR_W, screen.min.y), screen.max).shrink(8.0);
    p.rect_filled(main, CornerRadius::same(12), card);
    p.rect_stroke(main, CornerRadius::same(12), Stroke::new(1.0, line), StrokeKind::Inside);
    p.text(pos2(main.min.x + 24.0, main.min.y + TOPBAR_H / 2.0), Align2::LEFT_CENTER, "Speech to text", semibold(17.0), fg);
    p.line_segment([pos2(main.min.x, main.min.y + TOPBAR_H), pos2(main.max.x, main.min.y + TOPBAR_H)], Stroke::new(1.0, line));

    let rows = [
        ("Provider", "Groq whisper-large-v3-turbo"),
        ("Language", "English (auto-detect off)"),
        ("Streaming", "Send audio while you speak"),
        ("Dictionary hints", "42 terms passed to the model"),
        ("Cleanup model", "OpenRouter, free tier"),
        ("Paste method", "Clipboard, restore afterwards"),
        ("Sounds", "Start and stop chimes"),
        ("Launch at login", "Start in the tray"),
    ];
    let mut y = main.min.y + TOPBAR_H + 16.0;
    for (i, (title, sub)) in rows.iter().enumerate() {
        let r = Rect::from_min_size(pos2(main.min.x + 16.0, y), vec2(main.width() - 32.0, 56.0));
        p.rect_filled(r, CornerRadius::same(8), Color32::from_white_alpha(6));
        p.text(pos2(r.min.x + 16.0, r.min.y + 18.0), Align2::LEFT_CENTER, *title, semibold(13.0), fg);
        p.text(pos2(r.min.x + 16.0, r.min.y + 38.0), Align2::LEFT_CENTER, *sub, regular(12.0), dim);
        let sw = Rect::from_center_size(pos2(r.max.x - 40.0, r.center().y), vec2(36.0, 20.0));
        let on = i % 3 != 1;
        p.rect_filled(sw, CornerRadius::same(10), if on { accent } else { Color32::from_white_alpha(30) });
        let knob_x = if on { sw.max.x - 10.0 } else { sw.min.x + 10.0 };
        p.circle_filled(pos2(knob_x, sw.center().y), 7.0, Color32::WHITE);
        y += 62.0;
        if y > main.max.y - 60.0 {
            break;
        }
    }
    // One real widget row so the page has interactive egui content too.
    let r = Rect::from_min_size(pos2(main.min.x + 16.0, main.max.y - 52.0), vec2(260.0, 36.0));
    ui.scope_builder(egui::UiBuilder::new().max_rect(r), |ui| {
        ui.horizontal(|ui| {
            let _ = ui.button("Test connection");
            let mut v = true;
            ui.checkbox(&mut v, "Remember");
        });
    });
}
