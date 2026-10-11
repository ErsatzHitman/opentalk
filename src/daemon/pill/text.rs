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
}

/// RAII wrapper around a memory DC with the selected font.
struct Gdi {
    dc: windows::Win32::Graphics::Gdi::HDC,
    font: HGDIOBJ,
    old: HGDIOBJ,
}

impl Gdi {
    fn new(font: Font) -> Option<Gdi> {
        Gdi::with_face(font, FONT_FACE)
    }

    fn with_face(font: Font, face: &str) -> Option<Gdi> {
        // SAFETY: plain GDI object creation; every handle is released in Drop.
        unsafe {
            let dc = CreateCompatibleDC(None);
            if dc.is_invalid() {
                return None;
            }
            let face: Vec<u16> = face.encode_utf16().chain(std::iter::once(0)).collect();
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

// --- system icon font ---------------------------------------------------------------------

/// The Windows icon font: Segoe Fluent Icons (Windows 11), else Segoe MDL2 Assets (Windows 10, same
/// code points for the icons used here); `None` where neither is installed (Wine), and callers draw
/// their own vector shapes.
pub fn icon_face() -> Option<&'static str> {
    static F: std::sync::OnceLock<Option<&'static str>> = std::sync::OnceLock::new();
    *F.get_or_init(|| ["Segoe Fluent Icons", "Segoe MDL2 Assets"].into_iter().find(|f| face_is_real(f)))
}

/// GDI substitutes a missing face silently; the face it actually selected says whether it exists.
fn face_is_real(face: &str) -> bool {
    let Some(g) = Gdi::with_face(Font { px: 16, weight: 400 }, face) else { return false };
    let mut buf = [0u16; 64];
    // SAFETY: valid DC with the font selected; `buf` is large enough for any face name.
    let n = unsafe { windows::Win32::Graphics::Gdi::GetTextFaceW(g.dc, Some(&mut buf)) };
    n > 0 && String::from_utf16_lossy(&buf[..(n as usize).saturating_sub(1).min(buf.len())]).trim_end_matches('\0').eq_ignore_ascii_case(face)
}

type IconCache = Mutex<HashMap<(char, i32), Option<Arc<Mask>>>>;

/// Coverage mask of one icon-font glyph in a `px` x `px` device-pixel square, centred (cached).
pub fn icon_mask(glyph: char, px: i32) -> Option<Arc<Mask>> {
    static C: std::sync::OnceLock<IconCache> = std::sync::OnceLock::new();
    let face = icon_face()?;
    let c = C.get_or_init(|| Mutex::new(HashMap::new()));
    let key = (glyph, px);
    if let Some(m) = c.lock().unwrap_or_else(|e| e.into_inner()).get(&key) {
        return m.clone();
    }
    let m = rasterize_icon(face, glyph, px).map(Arc::new);
    c.lock().unwrap_or_else(|e| e.into_inner()).insert(key, m.clone());
    m
}

fn rasterize_icon(face: &str, glyph: char, px: i32) -> Option<Mask> {
    let px = px.max(4);
    let g = Gdi::with_face(Font { px: px * SS, weight: 400 }, face)?;
    let b = px * SS;
    let bmi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: b,
            biHeight: -b,
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
        std::ptr::write_bytes(bits as *mut u8, 0, (b * b * 4) as usize);
        SetBkMode(g.dc, TRANSPARENT);
        SetTextColor(g.dc, COLORREF(0x00ff_ffff));
        let mut buf = [0u16; 2];
        let mut wt = glyph.encode_utf16(&mut buf).to_vec();
        let mut rc = RECT { left: 0, top: 0, right: b, bottom: b };
        DrawTextW(
            g.dc,
            &mut wt,
            &mut rc,
            windows::Win32::Graphics::Gdi::DT_CENTER | windows::Win32::Graphics::Gdi::DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX | DT_NOCLIP,
        );
        let src = std::slice::from_raw_parts(bits as *const u8, (b * b * 4) as usize);
        let mut data = vec![0u8; (px * px) as usize];
        let n = (SS * SS) as u32;
        for y in 0..px {
            for x in 0..px {
                let mut sum = 0u32;
                for sy in 0..SS {
                    let row = ((y * SS + sy) * b + x * SS) as usize * 4;
                    for sx in 0..SS as usize {
                        let p = &src[row + sx * 4..row + sx * 4 + 3];
                        sum += p[0].max(p[1]).max(p[2]) as u32;
                    }
                }
                data[(y * px + x) as usize] = (sum / n) as u8;
            }
        }
        SelectObject(g.dc, old_bmp);
        let _ = DeleteObject(HGDIOBJ(bmp.0));
        Some(Mask { w: px, h: px, data })
    }
}

/// Blend an icon mask from [`icon_mask`] with its top-left at logical (x, y); no padding column.
pub fn draw_icon_mask(canvas: &mut Canvas, m: &Mask, x: f32, y: f32, color: Rgba, alpha: f32) {
    draw(canvas, m, x + 1.0 / canvas.scale, y, color, alpha);
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

/// Cumulative advance in device px before each character of `text` (`chars + 1` entries: 0 first,
/// the full width last), from ONE `GetTextExtentExPointW` call at 4x. This is what places a text
/// caret or a click without measuring O(n) prefixes (which would flood the `measure` cache).
pub fn prefix_widths(text: &str, font: Font) -> Vec<f32> {
    let units = wide(text);
    let mut out = vec![0.0f32];
    if units.is_empty() {
        return out;
    }
    let mut dx = vec![0i32; units.len()];
    let mut size = windows::Win32::Foundation::SIZE::default();
    let mut ok = false;
    if let Some(g) = Gdi::new(big(font)) {
        // SAFETY: `units` and `dx` outlive the call and are `units.len()` long; the DC is valid.
        ok = unsafe {
            let mut fit = 0i32;
            windows::Win32::Graphics::Gdi::GetTextExtentExPointW(
                g.dc,
                PCWSTR(units.as_ptr()),
                units.len() as i32,
                i32::MAX,
                Some(&mut fit),
                Some(dx.as_mut_ptr()),
                &mut size,
            )
        }
        .as_bool();
    }
    if !ok {
        dx.iter_mut().for_each(|v| *v = 0);
    }
    out.extend(char_prefixes(text, &dx, SS as f32));
    out
}

/// Per-char cumulative widths from per-UTF-16-unit cumulative extents `dx` (a surrogate pair takes
/// the extent of its second unit), divided by `div`. `text.chars().count()` entries.
pub fn char_prefixes(text: &str, dx: &[i32], div: f32) -> Vec<f32> {
    let mut out = Vec::with_capacity(text.chars().count());
    let mut unit = 0usize;
    for ch in text.chars() {
        unit += ch.len_utf16();
        let v = dx.get(unit.wrapping_sub(1)).copied().unwrap_or_else(|| dx.last().copied().unwrap_or(0));
        out.push(v as f32 / div);
    }
    out
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
        Some(Mask { w: w1, h: h1, data })
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
    fn char_prefixes_map_utf16_units_to_chars() {
        // "a" + U+1F600 (two units) + "b": extents per unit 10, 20, 30, 40.
        let dx = [10, 20, 30, 40];
        assert_eq!(char_prefixes("a\u{1F600}b", &dx, 1.0), vec![10.0, 30.0, 40.0]);
        assert_eq!(char_prefixes("ab", &[8, 16], 4.0), vec![2.0, 4.0]);
        assert!(char_prefixes("", &[], 4.0).is_empty());
        // A short extents array (GDI refused) never panics.
        assert_eq!(char_prefixes("ab", &[], 4.0), vec![0.0, 0.0]);
    }

    #[test]
    fn prefix_widths_are_monotonic_and_end_at_the_full_width() {
        let f = Font::new(13.0, 1.0, 400);
        let w = prefix_widths("Hello world", f);
        assert_eq!(w.len(), 12);
        assert_eq!(w[0], 0.0);
        assert!(w.windows(2).all(|p| p[1] >= p[0]));
        assert!(w[11] > 30.0);
        assert!((w[11] - measure("Hello world", f)).abs() < 1.5, "{} vs {}", w[11], measure("Hello world", f));
        assert_eq!(prefix_widths("", f), vec![0.0]);
        // One entry per character, not per UTF-16 unit.
        assert_eq!(prefix_widths("a\u{1F600}b", f).len(), 4);
    }

    #[test]
    fn mask_has_ink() {
        let m = mask("Recording...", Font::new(10.0, 1.0, 400)).expect("gdi mask");
        assert!(m.w > 30 && m.h >= 10);
        assert!(m.data.iter().filter(|&&v| v > 128).count() > 10);
        // supersampling yields intermediate coverage values, not just 0/255
        assert!(m.data.iter().any(|&v| v > 20 && v < 235));
    }
}
