//! Generates `assets/opentalk.ico` (and optionally a PNG preview) from the OpenWhispr-style logo:
//! a ring with three vertical bars, white on a charcoal rounded square.
//!
//! Run: `cargo run --example make_icon` (writes assets/opentalk.ico; add a path argument to
//! write a 256 px PNG preview as well). Pure tiny-skia, no other tooling.
use tiny_skia::{FillRule, LineCap, Paint, PathBuilder, Pixmap, Stroke, Transform};

const SIZES: [u32; 7] = [16, 24, 32, 48, 64, 128, 256];

fn render(size: u32) -> Pixmap {
    let mut pm = Pixmap::new(size, size).expect("pixmap");
    let s = size as f32;

    // Charcoal rounded square.
    let r = s * 0.22;
    let mut bg = PathBuilder::new();
    let (x0, y0, x1, y1) = (0.0, 0.0, s, s);
    bg.move_to(x0 + r, y0);
    bg.line_to(x1 - r, y0);
    bg.quad_to(x1, y0, x1, y0 + r);
    bg.line_to(x1, y1 - r);
    bg.quad_to(x1, y1, x1 - r, y1);
    bg.line_to(x0 + r, y1);
    bg.quad_to(x0, y1, x0, y1 - r);
    bg.line_to(x0, y0 + r);
    bg.quad_to(x0, y0, x0 + r, y0);
    bg.close();
    let mut fill = Paint::default();
    fill.set_color_rgba8(0x17, 0x17, 0x17, 255);
    fill.anti_alias = true;
    if let Some(path) = bg.finish() {
        pm.fill_path(&path, &fill, FillRule::Winding, Transform::identity(), None);
    }
    // Logo: 24-unit viewBox (ring r=10, three bars, stroke 2, round caps) at 70% of the icon.
    let scale = s * 0.70 / 24.0;
    let offset = s * 0.15;
    let ts = Transform::from_scale(scale, scale).post_translate(offset, offset);
    let mut ink = Paint::default();
    ink.set_color_rgba8(255, 255, 255, 255);
    ink.anti_alias = true;
    let stroke = Stroke { width: 2.0, line_cap: LineCap::Round, ..Stroke::default() };
    if let Some(ring) = PathBuilder::from_circle(12.0, 12.0, 10.0) {
        pm.stroke_path(&ring, &ink, &stroke, ts, None);
    }
    let mut bars = PathBuilder::new();
    for (x, ya, yb) in [(8.75, 10.0, 14.0), (12.0, 8.0, 16.0), (15.25, 10.0, 14.0)] {
        bars.move_to(x, ya);
        bars.line_to(x, yb);
    }
    if let Some(path) = bars.finish() {
        pm.stroke_path(&path, &ink, &stroke, ts, None);
    }
    pm
}

fn main() {
    let pngs: Vec<(u32, Vec<u8>)> = SIZES.iter().map(|&n| (n, render(n).encode_png().expect("png"))).collect();

    // ICONDIR + ICONDIRENTRY[] + PNG blobs (PNG-compressed frames are valid since Vista).
    let mut ico = Vec::new();
    ico.extend_from_slice(&[0, 0, 1, 0]);
    ico.extend_from_slice(&(pngs.len() as u16).to_le_bytes());
    let mut offset = 6 + 16 * pngs.len() as u32;
    for (n, png) in &pngs {
        let dim = if *n >= 256 { 0 } else { *n as u8 };
        ico.extend_from_slice(&[dim, dim, 0, 0]);
        ico.extend_from_slice(&1u16.to_le_bytes()); // planes
        ico.extend_from_slice(&32u16.to_le_bytes()); // bpp
        ico.extend_from_slice(&(png.len() as u32).to_le_bytes());
        ico.extend_from_slice(&offset.to_le_bytes());
        offset += png.len() as u32;
    }
    for (_, png) in &pngs {
        ico.extend_from_slice(png);
    }
    let out = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("assets").join("opentalk.ico");
    std::fs::create_dir_all(out.parent().expect("assets dir")).expect("create assets");
    std::fs::write(&out, &ico).expect("write ico");
    println!("wrote {} ({} bytes, {} sizes)", out.display(), ico.len(), pngs.len());
    if let Some(preview) = std::env::args().nth(1) {
        std::fs::write(&preview, &pngs.last().expect("sizes").1).expect("write preview");
        println!("preview {preview}");
    }
}
