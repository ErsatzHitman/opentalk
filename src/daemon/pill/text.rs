//! Text via GDI: each line is rasterised with `DrawTextW` (Segoe UI, `ANTIALIASED_QUALITY`) into a
//! 32-bit DIB at 4x the target size and box-filtered down, which removes GDI's small-size hinting and
//! gives browser-like grayscale text with fractional advances. The grey value becomes a coverage mask
//! that is cached and composited with the theme colour. Wrapping and line boxes follow the CSS the web
//! pill uses (`leading-snug` = 1.375).

use super::raster::Canvas;
use super::theme::Rgba;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use windows::Win32::Foundation::{COLORREF, RECT};
use windows::Win32::Graphics::Gdi::{
    ANTIALIASED_QUALITY, BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CLIP_DEFAULT_PRECIS, CreateCompatibleDC, CreateDIBSection,
    CreateFontW, DEFAULT_CHARSET, DEFAULT_PITCH, DIB_RGB_COLORS, DT_LEFT, DT_NOCLIP, DT_NOPREFIX, DT_SINGLELINE, DT_TOP,
    DeleteDC, DeleteObject, DrawTextW, FF_DONTCARE, GetTextExtentPoint32W, GetTextMetricsW, HGDIOBJ, OUT_TT_PRECIS,
    SelectObject, SetBkColor, SetBkMode, SetTextColor, TEXTMETRICW, TRANSPARENT,
};
use windows::core::PCWSTR;

pub const FONT_FACE: &str = "Segoe UI";
/// CSS `leading-snug`.
pub const LEADING_SNUG: f32 = 1.375;
/// Supersampling factor of the GDI rasterisation.
const SS: i32 = 4;

/// Device-pixel font request.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Font {
    /// Em height in device pixels.
    pub px: i32,
    pub weight: i32,
}

impl Font {
    pub fn new(logical_px: f32, scale: f32, weight: i32) -> Font {
        Font { px: (logical_px * scale).round().max(1.0) as i32, weight }
    }
}

/// Coverage bitmap of one line of text (1 px of padding on the left, 1 on the right).
#[derive(Debug)]
pub struct Mask {
    pub w: i32,
    pub h: i32,
    pub data: Vec<u8>,
    /// Advance width in device px.
    pub advance: f32,
}

/// RAII wrapper around a memory DC with the selected font.
struct Gdi {
    dc: windows::Win32::Graphics::Gdi::HDC,
    font: HGDIOBJ,
    old: HGDIOBJ,
}

impl Gdi {
    fn new(font: Font) -> Option<Gdi> {
        // SAFETY: plain GDI object creation; every handle is released in Drop.
        unsafe {
            let dc = CreateCompatibleDC(None);
            if dc.is_invalid() {
                return None;
            }
            let face: Vec<u16> = FONT_FACE.encode_utf16().chain(std::iter::once(0)).collect();
            let f = CreateFontW(
                -font.px,
                0,
                0,
                0,
                font.weight,
                0,
                0,
                0,
                DEFAULT_CHARSET,
                OUT_TT_PRECIS,
                CLIP_DEFAULT_PRECIS,
                ANTIALIASED_QUALITY,
                (DEFAULT_PITCH.0 | FF_DONTCARE.0) as u32,
                PCWSTR(face.as_ptr()),
            );
            if f.is_invalid() {
                let _ = DeleteDC(dc);
                return None;
            }
            let font = HGDIOBJ(f.0);
            let old = SelectObject(dc, font);
            Some(Gdi { dc, font, old })
        }
    }
}

impl Drop for Gdi {
    fn drop(&mut self) {
        // SAFETY: handles were created in `new` and are released exactly once.
        unsafe {
            SelectObject(self.dc, self.old);
            let _ = DeleteObject(self.font);
            let _ = DeleteDC(self.dc);
        }
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

fn big(font: Font) -> Font {
    Font { px: font.px * SS, weight: font.weight }
}

fn measure_raw(g: &Gdi, text: &str) -> i32 {
    let w = wide(text);
    let mut size = windows::Win32::Foundation::SIZE::default();
    // SAFETY: `w` outlives the call; `g.dc` is a valid DC with the font selected.
    unsafe {
        let _ = GetTextExtentPoint32W(g.dc, &w, &mut size);
    }
    size.cx
}

fn metric_cache() -> &'static Mutex<HashMap<(String, Font), f32>> {
    static C: std::sync::OnceLock<Mutex<HashMap<(String, Font), f32>>> = std::sync::OnceLock::new();
    C.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Advance width of `text` in device px (fractional, measured unhinted at 4x; cached).
pub fn measure(text: &str, font: Font) -> f32 {
    if text.is_empty() {
        return 0.0;
    }
    let key = (text.to_string(), font);
    if let Some(v) = metric_cache().lock().unwrap_or_else(|e| e.into_inner()).get(&key) {
        return *v;
    }
    let Some(g) = Gdi::new(big(font)) else { return 0.0 };
    let v = measure_raw(&g, text) as f32 / SS as f32;
    let mut c = metric_cache().lock().unwrap_or_else(|e| e.into_inner());
    if c.len() > 2048 {
        c.clear();
    }
    c.insert(key, v);
    v
}

/// Ascent + descent of the font in device px (cached).
pub fn content_height(font: Font) -> f32 {
    static H: std::sync::OnceLock<Mutex<HashMap<Font, f32>>> = std::sync::OnceLock::new();
    let m = H.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(v) = m.lock().unwrap_or_else(|e| e.into_inner()).get(&font) {
        return *v;
    }
    let Some(g) = Gdi::new(big(font)) else { return font.px as f32 };
    let mut tm = TEXTMETRICW::default();
    // SAFETY: valid DC, valid out-pointer.
    unsafe {
        let _ = GetTextMetricsW(g.dc, &mut tm);
    }
    let v = tm.tmHeight.max(1) as f32 / SS as f32;
    m.lock().unwrap_or_else(|e| e.into_inner()).insert(font, v);
    v
}

fn rasterize(text: &str, font: Font) -> Option<Mask> {
    let g = Gdi::new(big(font))?;
    let adv4 = measure_raw(&g, text);
    let mut tm = TEXTMETRICW::default();
    // SAFETY: valid DC, valid out-pointer.
    unsafe {
        let _ = GetTextMetricsW(g.dc, &mut tm);
    }
    let h1 = ((tm.tmHeight.max(1) as f32) / SS as f32).ceil() as i32;
    let w1 = (adv4 as f32 / SS as f32).ceil() as i32 + 2;
    let (bw, bh) = (w1 * SS, h1 * SS);
    if w1 <= 2 || h1 <= 0 {
        return None;
    }
    let bmi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: bw,
            biHeight: -bh,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
    // SAFETY: standard DIB section creation; `bits` points into the section for its lifetime.
    unsafe {
        let bmp = CreateDIBSection(Some(g.dc), &bmi, DIB_RGB_COLORS, &mut bits, None, 0).ok()?;
        if bits.is_null() {
            let _ = DeleteObject(HGDIOBJ(bmp.0));
            return None;
        }
        let old_bmp = SelectObject(g.dc, HGDIOBJ(bmp.0));
        std::ptr::write_bytes(bits as *mut u8, 0, (bw * bh * 4) as usize);
        SetBkMode(g.dc, TRANSPARENT);
        SetBkColor(g.dc, COLORREF(0));
        SetTextColor(g.dc, COLORREF(0x00ff_ffff));
        let mut wt = wide(text);
        let mut rc = RECT { left: SS, top: 0, right: bw, bottom: bh };
        DrawTextW(g.dc, &mut wt, &mut rc, DT_LEFT | DT_TOP | DT_SINGLELINE | DT_NOPREFIX | DT_NOCLIP);
        let px = std::slice::from_raw_parts(bits as *const u8, (bw * bh * 4) as usize);
        let mut data = vec![0u8; (w1 * h1) as usize];
        let n = (SS * SS) as u32;
        for y in 0..h1 {
            for x in 0..w1 {
                let mut sum = 0u32;
                for sy in 0..SS {
                    let row = ((y * SS + sy) * bw + x * SS) as usize * 4;
                    for sx in 0..SS as usize {
                        let p = &px[row + sx * 4..row + sx * 4 + 3];
                        sum += p[0].max(p[1]).max(p[2]) as u32;
                    }
                }
                data[(y * w1 + x) as usize] = (sum / n) as u8;
            }
        }
        SelectObject(g.dc, old_bmp);
        let _ = DeleteObject(HGDIOBJ(bmp.0));
        Some(Mask { w: w1, h: h1, data, advance: adv4 as f32 / SS as f32 })
    }
}

type Cache = Mutex<HashMap<(String, Font), Option<Arc<Mask>>>>;

fn cache() -> &'static Cache {
    static C: std::sync::OnceLock<Cache> = std::sync::OnceLock::new();
    C.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Cached coverage mask for one line.
pub fn mask(text: &str, font: Font) -> Option<Arc<Mask>> {
    let key = (text.to_string(), font);
    let mut c = cache().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(m) = c.get(&key) {
        return m.clone();
    }
    if c.len() > 256 {
        c.clear();
    }
    let m = rasterize(text, font).map(Arc::new);
    c.insert(key, m.clone());
    m
}

/// Blend a mask at logical position (x, y) = top-left of the glyph cell, rounded to whole device px.
pub fn draw(canvas: &mut Canvas, m: &Mask, x: f32, y: f32, color: Rgba, alpha: f32) {
    let ox = (x * canvas.scale).round() as i32 - 1; // mask has 1px left padding
    let oy = (y * canvas.scale).round() as i32;
    let a = color.a * alpha;
    if a <= 0.0 {
        return;
    }
    for j in 0..m.h {
        let row = &m.data[(j * m.w) as usize..((j + 1) * m.w) as usize];
        for (i, &c) in row.iter().enumerate() {
            if c == 0 {
                continue;
            }
            let k = c as f32 / 255.0 * a;
            canvas.blend(ox + i as i32, oy + j, [color.r * k, color.g * k, color.b * k, k]);
        }
    }
}

/// Greedy CSS-like wrap (`white-space: pre-wrap; overflow-wrap: break-word`) at `max_w` device px.
/// Explicit `\n` start a new line; over-long words are split by character.
pub fn wrap(text: &str, font: Font, max_w: f32) -> Vec<String> {
    let mut out = Vec::new();
    for para in text.split('\n') {
        if para.is_empty() {
            out.push(String::new());
            continue;
        }
        let mut line = String::new();
        let mut line_w = 0.0;
        for word in split_keep_spaces(para) {
            let ww = measure(&word, font);
            // trailing spaces may hang past the edge, so test the word without them
            let ww_trim = measure(word.trim_end(), font);
            if line_w + ww_trim <= max_w || (line.is_empty() && ww_trim <= max_w) {
                line.push_str(&word);
                line_w += ww;
                continue;
            }
            if !line.is_empty() {
                out.push(line.trim_end().to_string());
                line.clear();
                line_w = 0.0;
            }
            if word.trim().is_empty() {
                continue;
            }
            if ww_trim <= max_w {
                line.push_str(&word);
                line_w = ww;
            } else {
                // break the long word
                for ch in word.chars() {
                    let s = ch.to_string();
                    let cw = measure(&s, font);
                    if line_w + cw > max_w && !line.is_empty() {
                        out.push(std::mem::take(&mut line));
                        line_w = 0.0;
                    }
                    line.push(ch);
                    line_w += cw;
                }
            }
        }
        out.push(line.trim_end().to_string());
    }
    out
}

/// Split into runs of "word + following spaces".
fn split_keep_spaces(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_space = false;
    for ch in s.chars() {
        if ch == ' ' {
            in_space = true;
            cur.push(ch);
        } else {
            if in_space {
                out.push(std::mem::take(&mut cur));
                in_space = false;
            }
            cur.push(ch);
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn measure_grows_with_text_and_size() {
        let f10 = Font::new(10.0, 1.0, 400);
        let f20 = Font::new(10.0, 2.0, 400);
        let a = measure("Ctrl + Win", f10);
        let b = measure("Ctrl + Win + Shift", f10);
        assert!(a > 20.0 && b > a, "{a} {b}");
        let c = measure("Ctrl + Win", f20);
        assert!((c - a * 2.0).abs() < 1.0, "{a} {c}");
        assert_eq!(measure("", f10), 0.0);
    }

    #[test]
    fn mask_has_ink() {
        let m = mask("Recording...", Font::new(10.0, 1.0, 400)).expect("gdi mask");
        assert!(m.w > 30 && m.h >= 10);
        assert!(m.data.iter().filter(|&&v| v > 128).count() > 10);
        // supersampling yields intermediate coverage values, not just 0/255
        assert!(m.data.iter().any(|&v| v > 20 && v < 235));
    }

    #[test]
    fn wrap_respects_width_and_newlines() {
        let f = Font::new(14.0, 1.0, 400);
        let lines = wrap("one two three four five six seven eight nine ten", f, 120.0);
        assert!(lines.len() >= 3);
        for l in &lines {
            assert!(measure(l, f) <= 121.0, "{l}");
        }
        assert_eq!(wrap("a\nb", f, 200.0), vec!["a".to_string(), "b".to_string()]);
        // an unbreakable word is split by character
        let long = "x".repeat(200);
        let lines = wrap(&long, f, 100.0);
        assert!(lines.len() > 1);
        assert_eq!(lines.concat(), long);
    }
}
