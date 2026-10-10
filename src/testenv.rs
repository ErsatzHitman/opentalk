//! Test-environment probes (test builds only): recognise Wine precisely, so the few unit tests that
//! depend on Windows-only data (system fonts, the version resources of Windows' own programs) can
//! report `SKIPPED (Wine: <reason>)` there instead of failing. On Windows none of these probes ever
//! match, so those tests run and assert exactly as before.

use windows::Win32::Foundation::LPARAM;
use windows::Win32::Graphics::Gdi::{DEFAULT_CHARSET, EnumFontFamiliesExW, GetDC, LOGFONTW, ReleaseDC, TEXTMETRICW};
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
use windows::core::{s, w};

/// The Wine version when this process runs under Wine (`ntdll!wine_get_version` exists only there),
/// `None` on Windows.
pub fn wine_version() -> Option<String> {
    // SAFETY: ntdll is always loaded; `wine_get_version` (when present) takes no arguments and returns
    // a static NUL-terminated string.
    unsafe {
        let ntdll = GetModuleHandleW(w!("ntdll.dll")).ok()?;
        let f = GetProcAddress(ntdll, s!("wine_get_version"))?;
        let f: unsafe extern "C" fn() -> *const std::ffi::c_char = std::mem::transmute(f);
        let p = f();
        if p.is_null() {
            return Some(String::new());
        }
        Some(std::ffi::CStr::from_ptr(p).to_string_lossy().into_owned())
    }
}

unsafe extern "system" fn found_family(_lf: *const LOGFONTW, _tm: *const TEXTMETRICW, _kind: u32, found: LPARAM) -> i32 {
    // SAFETY: `found` is the `*mut bool` passed by `font_installed`, alive for the whole enumeration.
    unsafe {
        *(found.0 as *mut bool) = true;
    }
    0 // stop: one match is enough
}

/// Whether a font family with exactly this face name is installed (`EnumFontFamiliesExW` lists real
/// families only; GDI's silent substitution for `CreateFontW` does not count).
pub fn font_installed(face: &str) -> bool {
    let mut lf = LOGFONTW { lfCharSet: DEFAULT_CHARSET, ..Default::default() };
    for (d, c) in lf.lfFaceName.iter_mut().zip(face.encode_utf16().take(31)) {
        *d = c;
    }
    let mut found = false;
    // SAFETY: screen DC released below; the callback only writes through the pointer to `found`.
    unsafe {
        let dc = GetDC(None);
        if dc.is_invalid() {
            return false;
        }
        EnumFontFamiliesExW(dc, &lf, Some(found_family), LPARAM(&mut found as *mut bool as isize), 0);
        ReleaseDC(None, dc);
    }
    found
}

/// Why GDI text cannot match goldens rendered on Windows here: `Some(reason)` only when running under
/// Wine **and** the face the pill's text uses is not installed (so `DrawTextW` substitutes another
/// font). `None` on Windows, and on a Wine prefix that has the font.
pub fn wine_text_skip_reason(face: &str) -> Option<String> {
    let v = wine_version()?;
    if font_installed(face) {
        return None;
    }
    Some(format!("\"{face}\" is not installed in this Wine {v} prefix, DrawTextW substitutes another face"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probes_are_consistent() {
        // on Windows: not Wine, and the reason is always None; under Wine: a version string
        match wine_version() {
            None => assert!(wine_text_skip_reason("Segoe UI").is_none()),
            Some(v) => println!("running under Wine {v}"),
        }
        // a face that cannot exist is never reported as installed
        assert!(!font_installed("OpenTalk No Such Face 7f3a"));
    }
}
