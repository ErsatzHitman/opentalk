//! Foreground application icon for the Spokenly-style bubble. TASK win-output implements:
//! exe path -> high-quality icon (PrivateExtractIconsW / SHGetFileInfo at >= 48 px, the package
//! manifest's logo for packaged apps), friendly name from FileDescription; per-exe cache.
//!
//! Extraction order for classic Win32 executables: `PrivateExtractIconsW` (exact-size resource icon,
//! user32 only), then `SHGetFileInfoW` (system large icon), and `IShellItemImageFactory` only when both
//! return nothing. The factory loads ~35 shell DLLs (windows.storage, XAML, ...) into the process for
//! the rest of its life (about +17 MB working set), so packaged (UWP / MSIX) apps never use it. They
//! read the logo the package declares instead: the install folder (from the executable's path or the
//! process's package identity), `AppxManifest.xml`'s `Square44x44Logo`, the nearest `targetsize-N` /
//! `scale-N` PNG next to it, decoded with the `png` crate. No COM and no shell DLLs; callers run
//! this on a helper thread and the result stays in the per-exe cache.
//! Icons from `HICON`s are rasterised on black and white to recover their alpha channel.
use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::Mutex;

use windows::Win32::Foundation::{CloseHandle, HANDLE, SIZE};
use windows::Win32::Graphics::Gdi::{
    BITMAP, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, CreateCompatibleDC, CreateDIBSection, DIB_RGB_COLORS,
    DeleteDC, DeleteObject, GetDIBits, GetObjectW, HBITMAP, HGDIOBJ, SelectObject,
};
use windows::Win32::Storage::FileSystem::{
    FILE_FLAGS_AND_ATTRIBUTES, GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW,
};
use windows::Win32::Storage::Packaging::Appx::{GetApplicationUserModelId, GetPackageFullName, GetPackagePathByFullName};
use windows::Win32::System::Com::{
    COINIT_APARTMENTTHREADED, CoInitializeEx, CoUninitialize,
};
use windows::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
use windows::Win32::UI::Shell::{
    IShellItemImageFactory, SHCreateItemFromParsingName, SHGFI_ICON, SHGFI_LARGEICON,
    SHGetFileInfoW, SHFILEINFOW, SIIGBF_BIGGERSIZEOK, SIIGBF_ICONONLY,
};
use windows::Win32::UI::WindowsAndMessaging::{
    DI_NORMAL, DestroyIcon, DrawIconEx, HICON, PrivateExtractIconsW,
};
use windows::core::{PCWSTR, PWSTR};

use super::paste::Target;

#[derive(Clone, Debug, PartialEq)]
pub struct AppIcon {
    /// Friendly name ("Windows Explorer"), falls back to exe stem.
    pub name: String,
    pub exe_name: String,
    /// Straight (non-premultiplied) RGBA, `size` x `size`.
    pub rgba: Vec<u8>,
    pub size: u32,
}

type Cache = Mutex<HashMap<(String, u32), AppIcon>>;

fn cache() -> &'static Cache {
    static CACHE: std::sync::OnceLock<Cache> = std::sync::OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The cache survives a panic elsewhere while it was held (it only ever holds whole entries).
fn lock_cache() -> std::sync::MutexGuard<'static, HashMap<(String, u32), AppIcon>> {
    cache().lock().unwrap_or_else(|e| e.into_inner())
}

/// Largest icon we ever produce (a bigger request is clamped).
const MAX_PX: u32 = 256;

/// Icon for the target's process at roughly `px` pixels (cached).
pub fn load(target: &Target, px: u32) -> Option<AppIcon> {
    let px = px.clamp(16, MAX_PX);
    if target.exe_path.is_empty() {
        return None;
    }
    let key = (target.exe_path.to_ascii_lowercase(), px);
    if let Some(hit) = lock_cache().get(&key) {
        return Some(hit.clone());
    }
    let icon = extract(&target.exe_path, target.pid, px)?;
    lock_cache().insert(key, icon.clone());
    Some(icon)
}

/// Load an icon directly from an exe/ico path (used by `--render-pill --app-icon`).
pub fn load_path(path: &str, px: u32) -> Option<AppIcon> {
    let px = px.clamp(16, MAX_PX);
    if path.is_empty() {
        return None;
    }
    let key = (path.to_ascii_lowercase(), px);
    if let Some(hit) = lock_cache().get(&key) {
        return Some(hit.clone());
    }
    let icon = extract(path, 0, px)?;
    lock_cache().insert(key, icon.clone());
    Some(icon)
}

/// Which extraction strategy a target takes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Route {
    /// Classic executable: resource icon, then the system icon; the shell factory only as a last resort.
    Win32,
    /// UWP / MSIX app (or the frame host that fronts it): the logo the package declares, then the
    /// cheap extractors. Never the shell factory.
    Packaged,
}

/// Packaged apps live under `...\WindowsApps\` (or the inbox `...\SystemApps\`) or are shown through
/// `ApplicationFrameHost.exe`.
fn route_for(path: &str) -> Route {
    let lower = path.to_ascii_lowercase();
    if package_folder_end(path).is_some() || file_name(&lower) == "applicationframehost.exe" {
        Route::Packaged
    } else {
        Route::Win32
    }
}

/// `IShellItemImageFactory` backend (COM apartment scoped to the call).
fn shell_backend(item: &str, px: u32) -> Option<Vec<u8>> {
    let _com = Com::init();
    shell_image(item, px)
}

/// Cheap backends: exact-size resource icon (user32 only), then the system large icon.
fn cheap_backend(path: &str, px: u32) -> Option<Vec<u8>> {
    private_extract(path, px).or_else(|| shgetfileinfo(path, px))
}

/// What the package route yields: `px` x `px` straight RGBA plus the manifest's display name when
/// that is a literal string (resource references fall back to the exe's FileDescription).
struct PackagedIcon {
    rgba: Vec<u8>,
    name: Option<String>,
}

fn extract(path: &str, pid: u32, px: u32) -> Option<AppIcon> {
    extract_with(path, pid, px, &cheap_backend, &shell_backend, &packaged_backend)
}

/// [`extract`] with the backends injected so tests can prove which ones a route reaches. The shell
/// factory backs only the Win32 route's last resort; a packaged target never touches it.
fn extract_with(
    path: &str,
    pid: u32,
    px: u32,
    cheap: &dyn Fn(&str, u32) -> Option<Vec<u8>>,
    shell: &dyn Fn(&str, u32) -> Option<Vec<u8>>,
    packaged: &dyn Fn(&str, u32, u32) -> Option<PackagedIcon>,
) -> Option<AppIcon> {
    let exe_name = file_name(path).to_ascii_lowercase();
    let mut packaged_name = None;
    let rgba = match route_for(path) {
        Route::Win32 => cheap(path, px).or_else(|| shell(path, px)),
        Route::Packaged => match packaged(path, pid, px) {
            Some(p) => {
                packaged_name = p.name;
                Some(p.rgba)
            }
            // No readable manifest logo (assets only inside resources.pri, access denied, the frame
            // host itself): the exe's own icon, which is at worst generic.
            None => cheap(path, px),
        },
    }?;

    let name = packaged_name
        .or_else(|| friendly_name(path).map(|d| tidy_description(&d)))
        .unwrap_or_else(|| name_from_stem(&exe_name));
    Some(AppIcon { name, exe_name, rgba, size: px })
}

// ---------------------------------------------------------------------------------------------
// Packaged apps: manifest + logo files
// ---------------------------------------------------------------------------------------------

/// Largest manifest or logo file we read (real ones are a few KB to a few hundred KB).
const MAX_PACKAGE_FILE: u64 = 4 << 20;

fn packaged_backend(path: &str, pid: u32, px: u32) -> Option<PackagedIcon> {
    let root = package_root_from_path(path).or_else(|| package_root_of_pid(pid))?;
    let xml = read_capped(&root.join("AppxManifest.xml"))?;
    let xml = String::from_utf8_lossy(&xml);
    let apps = parse_manifest(xml.trim_start_matches('\u{feff}'));
    let app_id = if pid != 0 { aumid_of(pid).and_then(|a| a.split_once('!').map(|(_, id)| id.to_owned())) } else { None };
    let app = pick_app(&apps, app_id.as_deref(), file_name(path))?;
    let rgba = app.logos.iter().find_map(|(rel, base_edge)| load_logo(&root, rel, *base_edge, px))?;
    // A literal DisplayName only: `ms-resource:` strings need the MRT loader (SHLoadIndirectString),
    // which maps 11 more DLLs and ~6 MB of working set into the daemon. The caller falls back to the
    // exe's FileDescription.
    let name = app.display_name.as_deref().filter(|n| !n.starts_with("ms-resource:")).and_then(clean_description);
    Some(PackagedIcon { rgba, name })
}

/// Folders packaged apps are installed under: store apps and the inbox ones (LockApp, Search, Settings).
const PACKAGE_FOLDERS: [&str; 2] = [r"\windowsapps\", r"\systemapps\"];

/// Offset just past the package folder marker in `path` (case-insensitive), if there is one.
fn package_folder_end(path: &str) -> Option<usize> {
    let lower = path.to_ascii_lowercase();
    PACKAGE_FOLDERS.iter().find_map(|m| lower.find(m).map(|i| i + m.len()))
}

/// Install folder of a package whose executable sits under `...\WindowsApps\<PackageFullName>\`
/// (or `...\SystemApps\<PackageFullName>\`).
fn package_root_from_path(path: &str) -> Option<std::path::PathBuf> {
    let at = package_folder_end(path)?;
    let end = path[at..].find(['\\', '/'])?;
    if end == 0 {
        return None;
    }
    Some(std::path::PathBuf::from(&path[..at + end]))
}

/// Install folder of a running packaged process, from its package identity.
fn package_root_of_pid(pid: u32) -> Option<std::path::PathBuf> {
    if pid == 0 {
        return None;
    }
    // SAFETY: handle closed on every path; buffers sized by the APIs' own length reports.
    unsafe {
        let h: HANDLE = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut len = 0u32;
        let _ = GetPackageFullName(h, &mut len, None);
        let full = if len > 1 {
            let mut buf = vec![0u16; len as usize];
            let rc = GetPackageFullName(h, &mut len, Some(PWSTR(buf.as_mut_ptr())));
            if rc.0 == 0 { Some(buf) } else { None }
        } else {
            None
        };
        let _ = CloseHandle(h);
        let full = full?;
        let mut plen = 0u32;
        let _ = GetPackagePathByFullName(PCWSTR(full.as_ptr()), &mut plen, None);
        if plen <= 1 {
            return None;
        }
        let mut pbuf = vec![0u16; plen as usize];
        if GetPackagePathByFullName(PCWSTR(full.as_ptr()), &mut plen, Some(PWSTR(pbuf.as_mut_ptr()))).0 != 0 {
            return None;
        }
        let end = pbuf.iter().position(|&u| u == 0).unwrap_or(pbuf.len());
        Some(std::path::PathBuf::from(String::from_utf16_lossy(&pbuf[..end])))
    }
}

fn read_capped(path: &std::path::Path) -> Option<Vec<u8>> {
    use std::io::Read;
    let mut buf = Vec::new();
    std::fs::File::open(path).ok()?.take(MAX_PACKAGE_FILE + 1).read_to_end(&mut buf).ok()?;
    (buf.len() as u64 <= MAX_PACKAGE_FILE).then_some(buf)
}

/// One `<Application>` of a package manifest.
#[derive(Clone, Debug, Default, PartialEq)]
struct ManifestApp {
    id: String,
    /// `Executable` attribute (may carry a sub-folder: `Notepad\Notepad.exe`).
    exe: String,
    /// Logo paths in preference order with the edge (px) an unqualified file of that kind has:
    /// `Square44x44Logo` (44), `Square150x150Logo` (150), `Logo` (50).
    logos: Vec<(String, u32)>,
    display_name: Option<String>,
}

/// Value of attribute `name` in one tag (`name="v"` or `name='v'`, whole-word, entities decoded).
fn xml_attr(tag: &str, name: &str) -> Option<String> {
    let mut from = 0;
    while let Some(i) = tag[from..].find(name) {
        let at = from + i;
        from = at + name.len();
        if !tag[..at].ends_with(|c: char| c.is_ascii_whitespace()) {
            continue;
        }
        let rest = tag[from..].trim_start();
        let Some(rest) = rest.strip_prefix('=') else { continue };
        let rest = rest.trim_start();
        let quote = rest.chars().next().filter(|c| *c == '"' || *c == '\'')?;
        let body = &rest[1..];
        let end = body.find(quote)?;
        return Some(
            body[..end]
                .replace("&lt;", "<")
                .replace("&gt;", ">")
                .replace("&quot;", "\"")
                .replace("&apos;", "'")
                .replace("&amp;", "&"),
        );
    }
    None
}

/// The text of the tag that starts at `start` (`<` included) up to its closing `>`.
fn tag_at(xml: &str, start: usize) -> &str {
    let end = xml[start..].find('>').map_or(xml.len(), |e| start + e + 1);
    &xml[start..end]
}

/// Every `<Application ...>` with its `VisualElements` logos. A tolerant scanner, not an XML parser:
/// manifests are machine-written and only a handful of attributes matter.
fn parse_manifest(xml: &str) -> Vec<ManifestApp> {
    let starts: Vec<usize> = xml
        .match_indices("<Application")
        .filter(|(i, _)| xml[i + "<Application".len()..].starts_with(|c: char| c.is_ascii_whitespace()))
        .map(|(i, _)| i)
        .collect();
    let mut apps = Vec::new();
    for (n, &s) in starts.iter().enumerate() {
        let seg = &xml[s..starts.get(n + 1).copied().unwrap_or(xml.len())];
        let head = tag_at(seg, 0);
        let mut app = ManifestApp {
            id: xml_attr(head, "Id").unwrap_or_default(),
            exe: xml_attr(head, "Executable").unwrap_or_default(),
            ..Default::default()
        };
        let ve = seg
            .match_indices("VisualElements")
            .find(|(i, _)| seg[..*i].ends_with(['<', ':']))
            .and_then(|(i, _)| seg[..i].rfind('<'))
            .map(|lt| tag_at(seg, lt));
        if let Some(ve) = ve {
            for (attr, edge) in [("Square44x44Logo", 44), ("Square150x150Logo", 150), ("Logo", 50)] {
                if let Some(v) = xml_attr(ve, attr).filter(|v| !v.is_empty()) {
                    app.logos.push((v, edge));
                }
            }
            app.display_name = xml_attr(ve, "DisplayName");
        }
        apps.push(app);
    }
    apps
}

/// The app a process belongs to: its AUMID's app id, else the one whose `Executable` is this file,
/// else the first.
fn pick_app<'a>(apps: &'a [ManifestApp], app_id: Option<&str>, exe_file: &str) -> Option<&'a ManifestApp> {
    app_id
        .and_then(|id| apps.iter().find(|a| a.id.eq_ignore_ascii_case(id)))
        .or_else(|| apps.iter().find(|a| file_name(&a.exe).eq_ignore_ascii_case(exe_file)))
        .or_else(|| apps.first())
}

/// How a logo variant is drawn: the plated tile, or the glyph alone on transparency.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Form {
    Plated,
    Unplated,
    LightUnplated,
}

/// A candidate file next to the manifest's logo path.
#[derive(Clone, Debug, PartialEq)]
struct Variant {
    file: String,
    /// Edge in pixels: `targetsize-N`, or base edge x `scale-N` / 100.
    edge: u32,
    form: Form,
}

/// Classify `file` as a variant of `stem`.`ext` (`Square44x44Logo.targetsize-48_altform-unplated.png`).
/// High-contrast, language and theme variants are skipped: they are not the app's normal look.
fn parse_variant(file: &str, stem: &str, ext: &str, base_edge: u32) -> Option<Variant> {
    let lower = file.to_ascii_lowercase();
    let rest = lower.strip_prefix(&stem.to_ascii_lowercase())?;
    let rest = rest.strip_suffix(&format!(".{}", ext.to_ascii_lowercase()))?;
    let mut edge = None;
    let mut form = Form::Plated;
    if !rest.is_empty() {
        for tok in rest.strip_prefix('.')?.split('_') {
            if let Some(n) = tok.strip_prefix("targetsize-") {
                edge = Some(n.parse::<u32>().ok()?);
            } else if let Some(n) = tok.strip_prefix("scale-") {
                let scale = n.parse::<u32>().ok()?;
                edge.get_or_insert(base_edge * scale / 100);
            } else if tok == "altform-unplated" {
                form = Form::Unplated;
            } else if tok == "altform-lightunplated" {
                form = Form::LightUnplated;
            } else {
                return None;
            }
        }
    }
    let edge = edge.unwrap_or(base_edge);
    (edge > 0).then(|| Variant { file: file.to_owned(), edge, form })
}

/// Best variant for `px`: the preferred form first, then the smallest image at least `px` wide
/// (downscaling keeps it crisp), else the largest smaller one.
fn pick_variant(variants: &[Variant], px: u32) -> Option<&Variant> {
    variants.iter().min_by_key(|v| {
        let fits = v.edge >= px;
        (v.form, !fits, if fits { v.edge } else { u32::MAX - v.edge })
    })
}

/// Read the best logo file for `px` from the folder of manifest path `rel` and fit it to `px` x `px`.
fn load_logo(root: &std::path::Path, rel: &str, base_edge: u32, px: u32) -> Option<Vec<u8>> {
    let full = root.join(rel.replace('/', "\\"));
    let dir = full.parent()?;
    let file = full.file_name()?.to_str()?;
    let (stem, ext) = file.rsplit_once('.')?;
    let mut left: Vec<Variant> = std::fs::read_dir(dir)
        .ok()?
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .filter_map(|n| parse_variant(&n, stem, ext, base_edge))
        .collect();
    // A file that fails to decode must not hide the next best one.
    while let Some(best) = pick_variant(&left, px).cloned() {
        if let Some(img) = read_capped(&dir.join(&best.file)).and_then(|b| decode_png_rgba(&b, px)) {
            return Some(img);
        }
        left.retain(|v| v.file != best.file);
    }
    None
}

/// PNG bytes -> straight RGBA `px` x `px` (any colour type or depth; non-square art is letterboxed).
fn decode_png_rgba(bytes: &[u8], px: u32) -> Option<Vec<u8>> {
    let mut dec = png::Decoder::new(std::io::Cursor::new(bytes));
    dec.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = dec.read_info().ok()?;
    // Check the header's size before allocating the frame buffer for it.
    let (hw, hh) = (reader.info().width, reader.info().height);
    if hw == 0 || hh == 0 || hw > 4096 || hh > 4096 {
        return None;
    }
    let mut buf = vec![0u8; reader.output_buffer_size()?];
    let info = reader.next_frame(&mut buf).ok()?;
    let (w, h) = (info.width, info.height);
    if w == 0 || h == 0 || w > 4096 || h > 4096 {
        return None;
    }
    let data = &buf[..info.buffer_size()];
    let mut rgba = Vec::with_capacity(w as usize * h as usize * 4);
    match info.color_type {
        png::ColorType::Rgba => rgba.extend_from_slice(data),
        png::ColorType::Rgb => data.chunks_exact(3).for_each(|p| rgba.extend_from_slice(&[p[0], p[1], p[2], 255])),
        png::ColorType::Grayscale => data.iter().for_each(|&g| rgba.extend_from_slice(&[g, g, g, 255])),
        png::ColorType::GrayscaleAlpha => data.chunks_exact(2).for_each(|p| rgba.extend_from_slice(&[p[0], p[0], p[0], p[1]])),
        png::ColorType::Indexed => return None,
    }
    fit_square(rgba, w, h, px)
}

// ---------------------------------------------------------------------------------------------
// Names
// ---------------------------------------------------------------------------------------------

fn file_name(path: &str) -> &str {
    path.rsplit(['\\', '/']).next().unwrap_or(path)
}

/// "windows-terminal.exe" -> "Windows Terminal".
fn name_from_stem(exe_name: &str) -> String {
    let stem = match exe_name.rsplit_once('.') {
        Some((s, _)) if !s.is_empty() => s,
        _ => exe_name,
    };
    let words: Vec<String> = stem
        .split(['-', '_', ' ', '.'])
        .filter(|w| !w.is_empty())
        .map(|w| {
            let mut cs = w.chars();
            match cs.next() {
                Some(f) => f.to_uppercase().chain(cs).collect(),
                None => String::new(),
            }
        })
        .collect();
    if words.is_empty() { exe_name.to_owned() } else { words.join(" ") }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// `FileDescription` from the version resource, via the file's translation table.
fn friendly_name(path: &str) -> Option<String> {
    let wpath = wide(path);
    // SAFETY: buffers are sized from the API's own length report; pointers returned by
    // VerQueryValueW point into `data`, which outlives their use.
    unsafe {
        let size = GetFileVersionInfoSizeW(PCWSTR(wpath.as_ptr()), None);
        if size == 0 {
            return None;
        }
        let mut data = vec![0u8; size as usize];
        GetFileVersionInfoW(PCWSTR(wpath.as_ptr()), None, size, data.as_mut_ptr() as *mut c_void)
            .ok()?;

        let query = |sub: &str| -> Option<String> {
            let wsub = wide(sub);
            let mut p: *mut c_void = std::ptr::null_mut();
            let mut len = 0u32;
            if !VerQueryValueW(data.as_ptr() as *const c_void, PCWSTR(wsub.as_ptr()), &mut p, &mut len)
                .as_bool()
                || p.is_null()
                || len == 0
            {
                return None;
            }
            let units = std::slice::from_raw_parts(p as *const u16, len as usize);
            let end = units.iter().position(|&u| u == 0).unwrap_or(units.len());
            clean_description(&String::from_utf16_lossy(&units[..end]))
        };

        // (language, codepage) pairs the file declares, then the usual suspects.
        let mut langs: Vec<(u16, u16)> = Vec::new();
        let wsub = wide("\\VarFileInfo\\Translation");
        let mut p: *mut c_void = std::ptr::null_mut();
        let mut len = 0u32;
        if VerQueryValueW(data.as_ptr() as *const c_void, PCWSTR(wsub.as_ptr()), &mut p, &mut len)
            .as_bool()
            && !p.is_null()
        {
            let pairs = std::slice::from_raw_parts(p as *const u16, (len / 2) as usize);
            for c in pairs.chunks_exact(2) {
                langs.push((c[0], c[1]));
            }
        }
        for extra in [(0x0409, 0x04B0), (0x0409, 0x04E4), (0x0000, 0x04B0)] {
            if !langs.contains(&extra) {
                langs.push(extra);
            }
        }
        langs
            .into_iter()
            .find_map(|(l, cp)| query(&format!("\\StringFileInfo\\{l:04x}{cp:04x}\\FileDescription")))
    }
}

/// Some packaged apps leave their file name as the description ("Notepad.exe", "SnippingTool.exe"):
/// drop the extension and split CamelCase into words ("Snipping Tool").
fn tidy_description(s: &str) -> String {
    let Some(stem) = s.len().checked_sub(4).filter(|&i| s.is_char_boundary(i) && s[i..].eq_ignore_ascii_case(".exe")).map(|i| &s[..i])
    else {
        return s.to_owned();
    };
    if stem.is_empty() || stem.contains(' ') {
        return stem.to_owned();
    }
    let mut out = String::with_capacity(stem.len() + 2);
    let mut prev: Option<char> = None;
    for c in stem.chars() {
        if c.is_uppercase() && prev.is_some_and(|p| p.is_lowercase() || p.is_ascii_digit()) {
            out.push(' ');
        }
        out.push(c);
        prev = Some(c);
    }
    out
}

/// Trim and reject empty / control-laden descriptions.
fn clean_description(s: &str) -> Option<String> {
    let t: String = s.chars().filter(|c| !c.is_control()).collect();
    let t = t.trim();
    if t.is_empty() { None } else { Some(t.to_owned()) }
}

/// AppUserModelId of a packaged process.
fn aumid_of(pid: u32) -> Option<String> {
    // SAFETY: handle closed on every path; buffer sized by the API's length report.
    unsafe {
        let h: HANDLE = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut len = 0u32;
        let _ = GetApplicationUserModelId(h, &mut len, None);
        let r = if len > 1 {
            let mut buf = vec![0u16; len as usize];
            let rc = GetApplicationUserModelId(h, &mut len, Some(PWSTR(buf.as_mut_ptr())));
            if rc.0 == 0 {
                let end = buf.iter().position(|&u| u == 0).unwrap_or(buf.len());
                Some(String::from_utf16_lossy(&buf[..end]))
            } else {
                None
            }
        } else {
            None
        };
        let _ = CloseHandle(h);
        r
    }
}

// ---------------------------------------------------------------------------------------------
// Pixel extraction
// ---------------------------------------------------------------------------------------------

/// Apartment guard (STA); balanced uninit only when this call actually initialised COM.
struct Com(bool);

impl Com {
    fn init() -> Com {
        // SAFETY: standard COM init; RPC_E_CHANGED_MODE just means the thread already has COM.
        Com(unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }.is_ok())
    }
}

impl Drop for Com {
    fn drop(&mut self) {
        if self.0 {
            // SAFETY: paired with the successful CoInitializeEx above.
            unsafe { CoUninitialize() };
        }
    }
}

/// `IShellItemImageFactory::GetImage(ICONONLY)` -> straight RGBA at exactly `px` x `px`.
fn shell_image(path: &str, px: u32) -> Option<Vec<u8>> {
    let w = wide(path);
    // SAFETY: HBITMAP is deleted before returning.
    unsafe {
        let factory: IShellItemImageFactory =
            SHCreateItemFromParsingName(PCWSTR(w.as_ptr()), None).ok()?;
        let hbm = factory
            .GetImage(SIZE { cx: px as i32, cy: px as i32 }, SIIGBF_ICONONLY | SIIGBF_BIGGERSIZEOK)
            .ok()?;
        let r = hbitmap_to_rgba(hbm);
        let _ = DeleteObject(HGDIOBJ(hbm.0));
        let (mut bgra, bw, bh) = r?;
        bgra_to_straight_rgba(&mut bgra);
        fit_square(bgra, bw, bh, px)
    }
}

/// Read a 32bpp bitmap as top-down BGRA.
unsafe fn hbitmap_to_rgba(hbm: HBITMAP) -> Option<(Vec<u8>, u32, u32)> {
    // SAFETY: caller passes a valid bitmap; GetDIBits fills a buffer we sized for w*h*4 bytes.
    unsafe {
        let mut bm = BITMAP::default();
        if GetObjectW(HGDIOBJ(hbm.0), size_of::<BITMAP>() as i32, Some(&mut bm as *mut _ as *mut c_void))
            == 0
        {
            return None;
        }
        let (w, h) = (bm.bmWidth, bm.bmHeight);
        if w <= 0 || h <= 0 {
            return None;
        }
        let hdc = CreateCompatibleDC(None);
        if hdc.is_invalid() {
            return None;
        }
        let mut bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -h, // top-down
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut buf = vec![0u8; (w * h * 4) as usize];
        let lines = GetDIBits(
            hdc,
            hbm,
            0,
            h as u32,
            Some(buf.as_mut_ptr() as *mut c_void),
            &mut bmi,
            DIB_RGB_COLORS,
        );
        let _ = DeleteDC(hdc);
        if lines == 0 {
            return None;
        }
        Some((buf, w as u32, h as u32))
    }
}

/// In place: BGRA (premultiplied if every colour channel <= alpha) -> straight RGBA. A bitmap with
/// no alpha at all (all zero) is treated as opaque.
fn bgra_to_straight_rgba(px: &mut [u8]) {
    let has_alpha = px.chunks_exact(4).any(|p| p[3] != 0);
    let premultiplied = has_alpha && px.chunks_exact(4).all(|p| p[0] <= p[3] && p[1] <= p[3] && p[2] <= p[3]);
    for p in px.chunks_exact_mut(4) {
        let (b, g, r) = (p[0], p[1], p[2]);
        let a = if has_alpha { p[3] } else { 255 };
        let un = |c: u8| -> u8 {
            if premultiplied && a != 0 && a != 255 {
                ((c as u32 * 255 + a as u32 / 2) / a as u32).min(255) as u8
            } else {
                c
            }
        };
        p[0] = un(r);
        p[1] = un(g);
        p[2] = un(b);
        p[3] = a;
    }
}

/// Return `px` x `px` straight RGBA from a `w` x `h` source: untouched when it already fits,
/// otherwise bilinearly resampled (aspect kept, centred).
fn fit_square(src: Vec<u8>, w: u32, h: u32, px: u32) -> Option<Vec<u8>> {
    if w == 0 || h == 0 || src.len() < (w * h * 4) as usize {
        return None;
    }
    if w == px && h == px {
        return Some(src);
    }
    let scale = (px as f32 / w as f32).min(px as f32 / h as f32);
    let (dw, dh) = (((w as f32 * scale).round() as u32).max(1), ((h as f32 * scale).round() as u32).max(1));
    let (ox, oy) = ((px - dw.min(px)) / 2, (px - dh.min(px)) / 2);
    let mut out = vec![0u8; (px * px * 4) as usize];
    let at = |x: u32, y: u32| -> [f32; 4] {
        let i = ((y.min(h - 1) * w + x.min(w - 1)) * 4) as usize;
        let a = src[i + 3] as f32;
        // premultiplied for correct blending
        [src[i] as f32 * a / 255.0, src[i + 1] as f32 * a / 255.0, src[i + 2] as f32 * a / 255.0, a]
    };
    // Box-filter when shrinking by more than 2x would alias; average a kernel of the source footprint.
    let (fx, fy) = (w as f32 / dw as f32, h as f32 / dh as f32);
    for y in 0..dh.min(px) {
        for x in 0..dw.min(px) {
            let (sx, sy) = ((x as f32 + 0.5) * fx - 0.5, (y as f32 + 0.5) * fy - 0.5);
            let (kx, ky) = (fx.max(1.0), fy.max(1.0));
            let (x0, x1) = ((sx - kx / 2.0).floor().max(0.0) as u32, (sx + kx / 2.0).ceil().max(0.0) as u32);
            let (y0, y1) = ((sy - ky / 2.0).floor().max(0.0) as u32, (sy + ky / 2.0).ceil().max(0.0) as u32);
            let mut acc = [0f32; 4];
            let mut n = 0f32;
            for yy in y0..=y1.min(h - 1) {
                for xx in x0..=x1.min(w - 1) {
                    let v = at(xx, yy);
                    for k in 0..4 {
                        acc[k] += v[k];
                    }
                    n += 1.0;
                }
            }
            if n == 0.0 {
                continue;
            }
            let a = acc[3] / n;
            let o = (((y + oy) * px + (x + ox)) * 4) as usize;
            if a > 0.0 {
                for k in 0..3 {
                    out[o + k] = ((acc[k] / n) * 255.0 / a).round().clamp(0.0, 255.0) as u8;
                }
            }
            out[o + 3] = a.round().clamp(0.0, 255.0) as u8;
        }
    }
    Some(out)
}

/// Rasterise an HICON at `px` on black and on white and solve for colour + alpha per pixel.
fn hicon_to_rgba(hicon: HICON, px: u32) -> Option<Vec<u8>> {
    // SAFETY: a private memory DC + 32bpp DIB section are created, drawn into and released here.
    unsafe {
        let hdc = CreateCompatibleDC(None);
        if hdc.is_invalid() {
            return None;
        }
        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: px as i32,
                biHeight: -(px as i32),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits: *mut c_void = std::ptr::null_mut();
        let Ok(dib) = CreateDIBSection(Some(hdc), &bmi, DIB_RGB_COLORS, &mut bits, None, 0) else {
            let _ = DeleteDC(hdc);
            return None;
        };
        if bits.is_null() {
            let _ = DeleteObject(HGDIOBJ(dib.0));
            let _ = DeleteDC(hdc);
            return None;
        }
        let old = SelectObject(hdc, HGDIOBJ(dib.0));
        let n = (px * px * 4) as usize;
        let render = |fill: u8| -> Option<Vec<u8>> {
            let s = std::slice::from_raw_parts_mut(bits as *mut u8, n);
            s.fill(fill);
            DrawIconEx(hdc, 0, 0, hicon, px as i32, px as i32, 0, None, DI_NORMAL).ok()?;
            Some(s.to_vec())
        };
        let on_black = render(0x00);
        let on_white = render(0xFF);
        SelectObject(hdc, old);
        let _ = DeleteObject(HGDIOBJ(dib.0));
        let _ = DeleteDC(hdc);

        let (b, w) = (on_black?, on_white?);
        let mut out = vec![0u8; n];
        for i in (0..n).step_by(4) {
            // white - black = 255 * (1 - alpha) per channel
            let d = (0..3).map(|k| (w[i + k] as i32 - b[i + k] as i32).clamp(0, 255)).sum::<i32>() / 3;
            let a = (255 - d).clamp(0, 255) as u32;
            if a > 0 {
                for k in 0..3 {
                    // b = colour * alpha (premultiplied)
                    out[i + (2 - k)] = ((b[i + k] as u32 * 255 + a / 2) / a).min(255) as u8;
                }
            }
            out[i + 3] = a as u8;
        }
        Some(out)
    }
}

fn private_extract(path: &str, px: u32) -> Option<Vec<u8>> {
    let mut wpath = [0u16; 260];
    let units: Vec<u16> = path.encode_utf16().collect();
    if units.len() >= wpath.len() {
        return None;
    }
    wpath[..units.len()].copy_from_slice(&units);
    let mut icons = [HICON::default()];
    // SAFETY: icons[0] receives at most one HICON, which is destroyed after drawing.
    unsafe {
        let n = PrivateExtractIconsW(&wpath, 0, px as i32, px as i32, Some(&mut icons), None, 0);
        if n == 0 || n == u32::MAX || icons[0].is_invalid() {
            return None;
        }
        let r = hicon_to_rgba(icons[0], px);
        let _ = DestroyIcon(icons[0]);
        r
    }
}

/// `SHGetFileInfoW(SHGFI_ICON)`; its docs require COM on the calling thread, so it gets its own apartment.
fn shgetfileinfo(path: &str, px: u32) -> Option<Vec<u8>> {
    let _com = Com::init();
    let w = wide(path);
    let mut info = SHFILEINFOW::default();
    // SAFETY: `info` is a valid out struct; its HICON is destroyed after drawing.
    unsafe {
        let ok = SHGetFileInfoW(
            PCWSTR(w.as_ptr()),
            FILE_FLAGS_AND_ATTRIBUTES(0),
            Some(&mut info),
            size_of::<SHFILEINFOW>() as u32,
            SHGFI_ICON | SHGFI_LARGEICON,
        );
        if ok == 0 || info.hIcon.is_invalid() {
            return None;
        }
        let r = hicon_to_rgba(info.hIcon, px);
        let _ = DestroyIcon(info.hIcon);
        r
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn system_exe(name: &str) -> String {
        let root = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
        format!(r"{root}\{name}")
    }

    #[test]
    fn stem_names() {
        assert_eq!(name_from_stem("explorer.exe"), "Explorer");
        assert_eq!(name_from_stem("windows-terminal.exe"), "Windows Terminal");
        assert_eq!(name_from_stem("my_app.exe"), "My App");
        assert_eq!(name_from_stem("noext"), "Noext");
    }

    #[test]
    fn descriptions_that_are_file_names_are_tidied() {
        assert_eq!(tidy_description("Notepad.exe"), "Notepad");
        assert_eq!(tidy_description("SnippingTool.EXE"), "Snipping Tool");
        assert_eq!(tidy_description("Windows Terminal Host"), "Windows Terminal Host");
        assert_eq!(tidy_description("Paint 3D.exe"), "Paint 3D");
        assert_eq!(tidy_description(".exe"), "");
        assert_eq!(tidy_description("é.exe"), "é");
        assert_eq!(tidy_description("é"), "é");
    }

    #[test]
    fn description_cleaning() {
        assert_eq!(clean_description("  Notepad \r\n").as_deref(), Some("Notepad"));
        assert_eq!(clean_description("   "), None);
        assert_eq!(clean_description("\u{0}\u{1}"), None);
    }

    #[test]
    fn unpremultiply() {
        // premultiplied half-transparent red: B=0 G=0 R=128 A=128 -> straight R=255
        let mut px = vec![0, 0, 128, 128, 0, 0, 0, 0];
        bgra_to_straight_rgba(&mut px);
        assert_eq!(&px[..4], &[255, 0, 0, 128]);
        assert_eq!(&px[4..], &[0, 0, 0, 0]);
        // straight data (a channel exceeds alpha) is only reordered
        let mut px = vec![10, 20, 200, 100];
        bgra_to_straight_rgba(&mut px);
        assert_eq!(px, vec![200, 20, 10, 100]);
        // no alpha at all -> opaque
        let mut px = vec![1, 2, 3, 0];
        bgra_to_straight_rgba(&mut px);
        assert_eq!(px, vec![3, 2, 1, 255]);
    }

    #[test]
    fn resize_keeps_color_and_size() {
        // 2x2 solid opaque green scaled to 4x4
        let src = [0u8, 255, 0, 255].repeat(4);
        let out = fit_square(src, 2, 2, 4).unwrap();
        assert_eq!(out.len(), 4 * 4 * 4);
        assert!(out.chunks_exact(4).all(|p| p == [0, 255, 0, 255]));
        // non-square is letterboxed (transparent bars)
        let src = [255u8, 0, 0, 255].repeat(4 * 2);
        let out = fit_square(src, 4, 2, 4).unwrap();
        assert_eq!(&out[..4], &[0, 0, 0, 0]);
        let mid = ((4 + 1) * 4) as usize; // row 1, col 1
        assert_eq!(&out[mid..mid + 4], &[255, 0, 0, 255]);
    }

    #[test]
    fn explorer_icon_and_name() {
        let path = system_exe("explorer.exe");
        let icon = load_path(&path, 64).expect("explorer icon");
        assert_eq!(icon.size, 64);
        assert_eq!(icon.rgba.len(), 64 * 64 * 4);
        assert!(icon.rgba.chunks_exact(4).any(|p| p[3] > 200), "has opaque pixels");
        assert!(icon.rgba.chunks_exact(4).any(|p| p[3] == 0), "has transparent corners");
        assert_eq!(icon.exe_name, "explorer.exe");
        assert!(!icon.name.is_empty());
        eprintln!("explorer: name={:?}", icon.name);

        // Write a PNG for visual inspection.
        let mut pm = tiny_skia::Pixmap::new(icon.size, icon.size).unwrap();
        for (dst, src) in pm.pixels_mut().iter_mut().zip(icon.rgba.chunks_exact(4)) {
            *dst = tiny_skia::ColorU8::from_rgba(src[0], src[1], src[2], src[3]).premultiply();
        }
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target");
        let _ = std::fs::create_dir_all(&dir);
        pm.save_png(dir.join("appicon_explorer.png")).unwrap();
    }

    #[test]
    fn cache_hit_returns_same() {
        let t = Target { exe_path: system_exe("explorer.exe"), ..Default::default() };
        let a = load(&t, 48).expect("icon");
        let b = load(&t, 48).expect("cached icon");
        assert_eq!(a, b);
        assert_eq!(a.size, 48);
        // different size is a different cache entry
        assert_eq!(load(&t, 32).unwrap().size, 32);
    }

    #[test]
    fn routes() {
        assert_eq!(route_for(r"C:\Windows\System32\notepad.exe"), Route::Win32);
        assert_eq!(route_for(r"C:\Windows\explorer.exe"), Route::Win32);
        assert_eq!(route_for(r"C:\Windows\System32\ApplicationFrameHost.exe"), Route::Packaged);
        assert_eq!(
            route_for(r"C:\Program Files\WindowsApps\Microsoft.WindowsCalculator_1.0_x64__8wekyb3d8bbwe\Calculator.exe"),
            Route::Packaged
        );
        assert_eq!(route_for(r"C:\Tools\windowsapps-helper.exe"), Route::Win32);
        assert_eq!(route_for(r"C:\Windows\SystemApps\Microsoft.LockApp_cw5n1h2txyewy\LockApp.exe"), Route::Packaged);
    }

    fn no_packaged(_: &str, _: u32, _: u32) -> Option<PackagedIcon> {
        None
    }

    fn solid(px: u32, v: u8) -> Vec<u8> {
        vec![v; (px * px * 4) as usize]
    }

    /// A classic Win32 exe must be served by the cheap extractors; the shell factory (which pulls ~35
    /// DLLs into the daemon) is never reached, and neither is the package route.
    #[test]
    fn win32_exe_never_reaches_shell_factory() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        for name in ["notepad.exe", "explorer.exe", r"System32\cmd.exe"] {
            let path = system_exe(name);
            if !std::path::Path::new(&path).exists() {
                continue;
            }
            let calls = AtomicUsize::new(0);
            let shell = |_: &str, _: u32| {
                calls.fetch_add(1, Ordering::SeqCst);
                None
            };
            let icon = extract_with(&path, 0, 48, &cheap_backend, &shell, &no_packaged).expect("cheap path icon");
            assert_eq!(calls.load(Ordering::SeqCst), 0, "{name} reached the shell factory");
            assert_eq!(icon.rgba.len(), 48 * 48 * 4);
            assert!(icon.rgba.chunks_exact(4).any(|p| p[3] > 200));
        }
    }

    /// For a classic exe the factory stays reachable, but only as the very last resort.
    #[test]
    fn shell_factory_is_last_resort_for_win32() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let calls = AtomicUsize::new(0);
        let shell = |_: &str, px: u32| {
            calls.fetch_add(1, Ordering::SeqCst);
            Some(solid(px, 9))
        };
        // Cheap paths return nothing for a Win32 exe: the factory is asked exactly once, last.
        let none = |_: &str, _: u32| None;
        let got = extract_with(r"C:\Tools\plain.exe", 0, 32, &none, &shell, &no_packaged);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(got.unwrap().rgba.len(), 32 * 32 * 4);
        // Cheap paths succeed: the factory is never asked.
        calls.store(0, Ordering::SeqCst);
        let some = |_: &str, px: u32| Some(solid(px, 7));
        assert!(extract_with(r"C:\Tools\plain.exe", 0, 32, &some, &shell, &no_packaged).is_some());
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    /// The Packaged route never calls the in-process shell backend (it maps ~35 shell DLLs into the
    /// daemon for good): not when the manifest logo is found, not when it is missing, not for the
    /// frame host, whether or not the cheap extractors find anything.
    #[test]
    fn packaged_route_never_calls_shell_backend() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let shell_calls = AtomicUsize::new(0);
        let shell = |_: &str, px: u32| {
            shell_calls.fetch_add(1, Ordering::SeqCst);
            Some(solid(px, 9))
        };
        let cheap_calls = AtomicUsize::new(0);
        let cheap = |_: &str, px: u32| {
            cheap_calls.fetch_add(1, Ordering::SeqCst);
            Some(solid(px, 7))
        };
        let none = |_: &str, _: u32| None;
        let pkg = r"C:\Program Files\WindowsApps\Microsoft.WindowsTerminal_1.0_x64__abc\WindowsTerminal.exe";
        let frame_host = r"C:\Windows\System32\ApplicationFrameHost.exe";
        let found = |_: &str, _: u32, px: u32| Some(PackagedIcon { rgba: solid(px, 5), name: Some("Terminal".into()) });

        for path in [pkg, frame_host] {
            for pid in [0, 4242] {
                // Manifest logo found: used as is, no other backend asked.
                cheap_calls.store(0, Ordering::SeqCst);
                let icon = extract_with(path, pid, 32, &cheap, &shell, &found).expect("packaged icon");
                assert!(icon.rgba.iter().all(|&b| b == 5));
                assert_eq!(icon.name, "Terminal");
                assert_eq!(cheap_calls.load(Ordering::SeqCst), 0);
                // No manifest logo: the cheap extractors, still not the factory.
                let icon = extract_with(path, pid, 32, &cheap, &shell, &no_packaged).expect("cheap icon");
                assert!(icon.rgba.iter().all(|&b| b == 7));
                // Nothing at all: no icon, rather than the factory.
                assert!(extract_with(path, pid, 32, &none, &shell, &no_packaged).is_none());
            }
        }
        assert_eq!(shell_calls.load(Ordering::SeqCst), 0, "the shell factory must not be asked");
    }

    const MANIFEST: &str = r##"<?xml version="1.0" encoding="utf-8"?>
<Package xmlns="http://schemas.microsoft.com/appx/manifest/foundation/windows10" xmlns:uap="x">
  <Identity Name="Microsoft.Test" Version="1.0.0.0" />
  <Applications>
    <Application Id="App" Executable="Test\Test.exe" EntryPoint="Windows.FullTrustApplication">
      <uap:VisualElements DisplayName="ms-resource:AppName" BackgroundColor="#0078D4"
          Square150x150Logo="Assets\Med.png" Square44x44Logo="Assets\List.png" Description="d">
        <uap:DefaultTile Wide310x150Logo="Assets\Wide.png" />
      </uap:VisualElements>
    </Application>
    <Application Id="Second" Executable="other.exe">
      <uap:VisualElements DisplayName="Other &amp; Co" Logo="Assets\Store.png" />
    </Application>
  </Applications>
</Package>"##;

    #[test]
    fn manifest_parsing() {
        let apps = parse_manifest(MANIFEST);
        assert_eq!(apps.len(), 2);
        assert_eq!(apps[0].id, "App");
        assert_eq!(apps[0].exe, r"Test\Test.exe");
        assert_eq!(
            apps[0].logos,
            vec![(r"Assets\List.png".to_owned(), 44), (r"Assets\Med.png".to_owned(), 150)]
        );
        assert_eq!(apps[0].display_name.as_deref(), Some("ms-resource:AppName"));
        // `Logo` is matched whole-word: it must not be read out of `Square44x44Logo`.
        assert_eq!(apps[1].logos, vec![(r"Assets\Store.png".to_owned(), 50)]);
        assert_eq!(apps[1].display_name.as_deref(), Some("Other & Co"));
        // A manifest without applications, or garbage, yields nothing rather than panicking.
        assert!(parse_manifest("<Package/>").is_empty());
        assert!(parse_manifest("<Application ").is_empty() || parse_manifest("<Application ")[0].logos.is_empty());
    }

    #[test]
    fn app_selection() {
        let apps = parse_manifest(MANIFEST);
        assert_eq!(pick_app(&apps, Some("second"), "test.exe").unwrap().id, "Second");
        assert_eq!(pick_app(&apps, None, "OTHER.EXE").unwrap().id, "Second");
        assert_eq!(pick_app(&apps, None, "Test.exe").unwrap().id, "App");
        assert_eq!(pick_app(&apps, Some("Nope"), "zzz.exe").unwrap().id, "App");
        assert!(pick_app(&[], None, "a.exe").is_none());
    }

    #[test]
    fn package_roots() {
        let root = package_root_from_path(r"C:\Program Files\WindowsApps\Pkg_1.0_x64__abc\Sub\app.exe").unwrap();
        assert_eq!(root, std::path::PathBuf::from(r"C:\Program Files\WindowsApps\Pkg_1.0_x64__abc"));
        let sys = package_root_from_path(r"C:\windows\systemapps\Microsoft.LockApp_cw5n1h2txyewy\LockApp.exe").unwrap();
        assert_eq!(sys, std::path::PathBuf::from(r"C:\windows\systemapps\Microsoft.LockApp_cw5n1h2txyewy"));
        assert!(package_root_from_path(r"C:\Windows\System32\ApplicationFrameHost.exe").is_none());
        assert!(package_root_from_path(r"C:\Program Files\WindowsApps\").is_none());
        assert!(package_root_from_path(r"C:\Program Files\WindowsApps\Pkg_only").is_none());
        // Not a running packaged process.
        assert!(package_root_of_pid(0).is_none());
        assert!(package_root_of_pid(std::process::id()).is_none());
    }

    fn variants(names: &[&str], stem: &str, ext: &str, base: u32) -> Vec<Variant> {
        names.iter().filter_map(|n| parse_variant(n, stem, ext, base)).collect()
    }

    #[test]
    fn variant_parsing_skips_contrast_and_foreign_files() {
        let v = variants(
            &[
                "Square44x44Logo.scale-200.png",
                "Square44x44Logo.scale-200_contrast-black.png",
                "Square44x44Logo.targetsize-48.png",
                "Square44x44Logo.targetsize-48_altform-unplated.png",
                "Square44x44Logo.targetsize-48_altform-lightunplated.png",
                "Square44x44Logo.targetsize-48_altform-unplated_contrast-white.png",
                "square44x44logo.png",
                "Square44x44Logo2.png",
                "Square44x44Logo.language-de.png",
                "Other.png",
                "Square44x44Logo.targetsize-48.ico",
            ],
            "Square44x44Logo",
            "png",
            44,
        );
        let got: Vec<(&str, u32, Form)> = v.iter().map(|v| (v.file.as_str(), v.edge, v.form)).collect();
        assert_eq!(
            got,
            vec![
                ("Square44x44Logo.scale-200.png", 88, Form::Plated),
                ("Square44x44Logo.targetsize-48.png", 48, Form::Plated),
                ("Square44x44Logo.targetsize-48_altform-unplated.png", 48, Form::Unplated),
                ("Square44x44Logo.targetsize-48_altform-lightunplated.png", 48, Form::LightUnplated),
                ("square44x44logo.png", 44, Form::Plated),
            ]
        );
    }

    #[test]
    fn variant_choice_prefers_smallest_that_fits() {
        let names = [
            "L.targetsize-16.png",
            "L.targetsize-32.png",
            "L.targetsize-48.png",
            "L.targetsize-256.png",
            "L.scale-100.png",
            "L.scale-200.png",
        ];
        let v = variants(&names, "L", "png", 44);
        assert_eq!(pick_variant(&v, 64).unwrap().file, "L.scale-200.png"); // 88 is the smallest >= 64
        assert_eq!(pick_variant(&v, 48).unwrap().file, "L.targetsize-48.png");
        assert_eq!(pick_variant(&v, 40).unwrap().edge, 44); // scale-100
        assert_eq!(pick_variant(&v, 200).unwrap().file, "L.targetsize-256.png");
        // Nothing big enough: the largest available.
        let small = variants(&names[..2], "L", "png", 44);
        assert_eq!(pick_variant(&small, 64).unwrap().file, "L.targetsize-32.png");
        assert!(pick_variant(&[], 64).is_none());
    }

    fn encode_png(w: u32, h: u32, color: png::ColorType, data: &[u8], palette: Option<&[u8]>, trns: Option<&[u8]>) -> Vec<u8> {
        let mut out = Vec::new();
        let mut enc = png::Encoder::new(&mut out, w, h);
        enc.set_color(color);
        enc.set_depth(png::BitDepth::Eight);
        if let Some(p) = palette {
            enc.set_palette(p.to_vec());
        }
        if let Some(t) = trns {
            enc.set_trns(t.to_vec());
        }
        let mut wr = enc.write_header().unwrap();
        wr.write_image_data(data).unwrap();
        wr.finish().unwrap();
        out
    }

    #[test]
    fn png_decoding_all_colour_types() {
        let rgba = encode_png(2, 2, png::ColorType::Rgba, &[10, 20, 30, 40].repeat(4), None, None);
        assert_eq!(decode_png_rgba(&rgba, 2).unwrap(), [10, 20, 30, 40].repeat(4));
        let rgb = encode_png(2, 2, png::ColorType::Rgb, &[1, 2, 3].repeat(4), None, None);
        assert_eq!(decode_png_rgba(&rgb, 2).unwrap(), [1, 2, 3, 255].repeat(4));
        let gray = encode_png(2, 2, png::ColorType::Grayscale, &[90; 4], None, None);
        assert_eq!(decode_png_rgba(&gray, 2).unwrap(), [90, 90, 90, 255].repeat(4));
        let ga = encode_png(2, 2, png::ColorType::GrayscaleAlpha, &[90, 128].repeat(4), None, None);
        assert_eq!(decode_png_rgba(&ga, 2).unwrap(), [90, 90, 90, 128].repeat(4));
        // Palette with a transparent entry: expanded, alpha kept.
        let idx = encode_png(2, 2, png::ColorType::Indexed, &[0, 1, 1, 0], Some(&[255, 0, 0, 0, 255, 0]), Some(&[255, 0]));
        let (red, clear) = ([255, 0, 0, 255], [0, 255, 0, 0]);
        assert_eq!(decode_png_rgba(&idx, 2).unwrap(), [red, clear, clear, red].concat());
        // Resampled to the requested size, garbage rejected.
        assert_eq!(decode_png_rgba(&rgba, 8).unwrap().len(), 8 * 8 * 4);
        assert!(decode_png_rgba(b"not a png", 8).is_none());
        assert!(decode_png_rgba(&rgba[..rgba.len() / 2], 8).is_none());
    }

    /// A fake installed package on disk (`...\windowsapps\<full name>\`), end to end through
    /// [`extract`]: manifest, nearest variant, display name, and a missing-asset fallback.
    #[test]
    fn packaged_icon_from_a_package_folder() {
        let base = std::env::temp_dir().join(format!("opentalk-appicon-{}", std::process::id()));
        let root = base.join("windowsapps").join("Fake.App_1.0_x64__abc");
        let assets = root.join("Assets");
        std::fs::create_dir_all(&assets).unwrap();
        let manifest = MANIFEST.replace("ms-resource:AppName", "Fake App");
        std::fs::write(root.join("AppxManifest.xml"), manifest).unwrap();
        let red = |n: u32| encode_png(n, n, png::ColorType::Rgba, &[200, 0, 0, 255].repeat((n * n) as usize), None, None);
        let blue = |n: u32| encode_png(n, n, png::ColorType::Rgba, &[0, 0, 200, 255].repeat((n * n) as usize), None, None);
        std::fs::write(assets.join("List.targetsize-48.png"), red(48)).unwrap();
        std::fs::write(assets.join("List.targetsize-256.png"), red(256)).unwrap();
        std::fs::write(assets.join("List.targetsize-48_contrast-black.png"), blue(48)).unwrap();
        std::fs::write(assets.join("List.targetsize-64_altform-unplated.png"), blue(64)).unwrap();
        // A corrupt best candidate must not hide the next one.
        std::fs::write(assets.join("List.targetsize-64.png"), b"broken").unwrap();

        let exe = root.join("Test").join("Test.exe");
        let icon = extract(exe.to_str().unwrap(), 0, 64).expect("package icon");
        assert_eq!(icon.size, 64);
        assert_eq!(icon.name, "Fake App");
        assert_eq!(icon.exe_name, "test.exe");
        assert_eq!(icon.rgba.len(), 64 * 64 * 4);
        // Plated 48 px art scaled up to 64 beats the unplated and the contrast files.
        let centre = ((32 * 64 + 32) * 4) as usize;
        assert_eq!(&icon.rgba[centre..centre + 4], &[200, 0, 0, 255]);

        // Same package, cached by the public loader as well.
        let t = Target { exe_path: exe.to_str().unwrap().to_owned(), ..Default::default() };
        assert_eq!(load(&t, 64).unwrap().name, "Fake App");

        // Manifest gone: falls back to the cheap extractors (the file is not an exe, so no icon).
        std::fs::remove_file(root.join("AppxManifest.xml")).unwrap();
        assert!(packaged_backend(exe.to_str().unwrap(), 0, 64).is_none());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn missing_file_is_none() {
        assert!(load_path(r"C:\definitely\not\here.exe", 48).is_none());
        let t = Target::default();
        assert!(load(&t, 48).is_none());
    }
    #[test]
    fn fallback_extractors_work() {
        let path = system_exe("notepad.exe");
        if !std::path::Path::new(&path).exists() {
            return;
        }
        let a = private_extract(&path, 48);
        let b = shgetfileinfo(&path, 48);
        assert!(a.is_some() || b.is_some());
        for img in [a, b].into_iter().flatten() {
            assert_eq!(img.len(), 48 * 48 * 4);
            assert!(img.chunks_exact(4).any(|p| p[3] > 0));
        }
    }

    #[test]
    fn friendly_name_from_version_info() {
        let path = system_exe("notepad.exe");
        let n = friendly_name(&path);
        if n.is_none() {
            // Wine's builtin notepad.exe carries a version resource without a FileDescription; Windows'
            // own always has one, so this skip can never trigger on Windows
            if let Some(v) = crate::testenv::wine_version() {
                if !version_info_has(&path, "FileDescription") {
                    println!("SKIPPED (Wine: the notepad.exe of Wine {v} has no FileDescription in its version resource)");
                    return;
                }
            }
        }
        assert!(n.is_some_and(|s| !s.is_empty()));
    }

    /// Whether the file's version resource contains the string key `key` at all (raw UTF-16 search).
    fn version_info_has(path: &str, key: &str) -> bool {
        let wpath = wide(path);
        // SAFETY: the buffer is sized from the API's own length report.
        let data = unsafe {
            let size = GetFileVersionInfoSizeW(PCWSTR(wpath.as_ptr()), None);
            if size == 0 {
                return false;
            }
            let mut data = vec![0u8; size as usize];
            if GetFileVersionInfoW(PCWSTR(wpath.as_ptr()), None, size, data.as_mut_ptr() as *mut c_void).is_err() {
                return false;
            }
            data
        };
        let needle: Vec<u8> = key.encode_utf16().flat_map(u16::to_le_bytes).collect();
        data.windows(needle.len()).any(|w| w == needle.as_slice())
    }

    /// Run alone (`--ignored`): loading a Win32 exe's icon must not drag the shell DLLs (the source of
    /// the ~17 MB working-set growth) into the process.
    #[test]
    #[ignore]
    fn desktop_win32_icon_loads_no_shell_dlls() {
        use windows::Win32::System::LibraryLoader::GetModuleHandleW;
        let loaded = |name: &str| {
            let w = wide(name);
            // SAFETY: `w` is NUL-terminated and outlives the call.
            unsafe { GetModuleHandleW(PCWSTR(w.as_ptr())) }.is_ok()
        };
        let ws_mb = || {
            use windows::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
            use windows::Win32::System::Threading::GetCurrentProcess;
            let mut c = PROCESS_MEMORY_COUNTERS::default();
            // SAFETY: `c` is a valid out struct sized as reported.
            let ok = unsafe {
                GetProcessMemoryInfo(GetCurrentProcess(), &mut c, size_of::<PROCESS_MEMORY_COUNTERS>() as u32)
            };
            if ok.is_ok() { c.WorkingSetSize as f64 / 1048576.0 } else { 0.0 }
        };
        let ws0 = ws_mb();
        let probes = ["windows.storage.dll", "StateRepository.Core.dll", "thumbcache.dll"];
        let before: Vec<bool> = probes.iter().map(|p| loaded(p)).collect();
        for name in ["notepad.exe", "explorer.exe"] {
            let icon = load_path(&system_exe(name), 48).expect("icon");
            assert!(!icon.name.is_empty());
        }
        eprintln!("working set: {ws0:.1} MB -> {:.1} MB after Win32 icon loads", ws_mb());
        for (p, was) in probes.iter().zip(before) {
            if !was {
                assert!(!loaded(p), "{p} was loaded by the Win32 icon path");
            }
        }
    }

    /// Install folders of common packaged apps on this machine (PowerShell; `WindowsApps` cannot be listed).
    fn installed_packages() -> Vec<(String, String)> {
        let script = "Get-AppxPackage | Where-Object { $_.Name -match '^Microsoft\\.(WindowsTerminal|WindowsNotepad|WindowsCalculator|Paint|Windows\\.Photos|ScreenSketch)$' } | ForEach-Object { $_.Name + '|' + $_.InstallLocation }";
        let out = std::process::Command::new("powershell").args(["-NoProfile", "-Command", script]).output();
        let Ok(out) = out else { return Vec::new() };
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(|l| l.trim().split_once('|').map(|(n, p)| (n.to_owned(), p.to_owned())))
            .collect()
    }

    /// Run alone (`--ignored`): packaged apps get their manifest logo (PNGs under `target/`) and the
    /// shell DLLs the image factory maps never enter the process.
    #[test]
    #[ignore]
    fn desktop_packaged_icons_load_no_shell_dlls() {
        use windows::Win32::System::LibraryLoader::GetModuleHandleW;
        let loaded = |name: &str| {
            let w = wide(name);
            // SAFETY: `w` is NUL-terminated and outlives the call.
            unsafe { GetModuleHandleW(PCWSTR(w.as_ptr())) }.is_ok()
        };
        let modules = || -> Vec<String> {
            use windows::Win32::Foundation::HMODULE;
            use windows::Win32::System::ProcessStatus::{EnumProcessModules, GetModuleBaseNameW};
            use windows::Win32::System::Threading::GetCurrentProcess;
            let mut mods = vec![HMODULE::default(); 2048];
            let mut needed = 0u32;
            // SAFETY: the buffers are sized as passed; the pseudo handle needs no close.
            unsafe {
                let me = GetCurrentProcess();
                if EnumProcessModules(me, mods.as_mut_ptr(), (mods.len() * size_of::<HMODULE>()) as u32, &mut needed).is_err() {
                    return Vec::new();
                }
                mods.truncate(needed as usize / size_of::<HMODULE>());
                mods.iter().map(|m| { let mut b = [0u16; 260]; let n = GetModuleBaseNameW(me, Some(*m), &mut b) as usize; String::from_utf16_lossy(&b[..n]).to_ascii_lowercase() }).collect()
            }
        };
        let ws_now = || {
            use windows::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
            use windows::Win32::System::Threading::GetCurrentProcess;
            let mut c = PROCESS_MEMORY_COUNTERS::default();
            // SAFETY: `c` is a valid out struct sized as reported.
            let ok = unsafe { GetProcessMemoryInfo(GetCurrentProcess(), &mut c, size_of::<PROCESS_MEMORY_COUNTERS>() as u32) };
            if ok.is_ok() { c.WorkingSetSize as f64 / 1048576.0 } else { 0.0 }
        };
        let ws0 = ws_now();
        let mods0 = modules();
        let pkgs = installed_packages();
        if pkgs.is_empty() {
            eprintln!("no packaged apps found; nothing to check");
            return;
        }
        let probes = ["windows.storage.dll", "StateRepository.Core.dll", "thumbcache.dll", "Windows.UI.dll"];
        let before: Vec<bool> = probes.iter().map(|p| loaded(p)).collect();
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target");
        for (name, root) in &pkgs {
            // The manifest's first app, as the foreground capture would see its exe.
            let manifest = read_capped(&std::path::Path::new(root).join("AppxManifest.xml")).unwrap_or_default();
            let apps = parse_manifest(&String::from_utf8_lossy(&manifest));
            let exe = format!(r"{root}\{}", apps.first().map_or("app.exe", |a| a.exe.as_str()));
            let icon = load_path(&exe, 64).unwrap_or_else(|| panic!("{name}: no manifest icon"));
            eprintln!("{name}: {:?}", icon.name);
            assert!(icon.rgba.chunks_exact(4).any(|p| p[3] > 200), "{name}: icon is empty");
            let mut pm = tiny_skia::Pixmap::new(64, 64).unwrap();
            for (dst, src) in pm.pixels_mut().iter_mut().zip(icon.rgba.chunks_exact(4)) {
                *dst = tiny_skia::ColorU8::from_rgba(src[0], src[1], src[2], src[3]).premultiply();
            }
            pm.save_png(dir.join(format!("appicon_pkg_{name}.png"))).unwrap();
        }
        eprintln!("working set {:.1} MB -> {:.1} MB", ws0, ws_now());
        let added: Vec<String> = modules().into_iter().filter(|m| !mods0.contains(m)).collect();
        eprintln!("{} modules added by the packaged icon path: {}", added.len(), added.join(", "));
        for (p, was) in probes.iter().zip(before) {
            if !was {
                assert!(!loaded(p), "{p} was loaded by the packaged icon path");
            }
        }
    }

    /// Icon + name for whatever window is in front right now (packaged or not).
    #[test]
    #[ignore]
    fn desktop_foreground_app_icon() {
        if !super::super::paste::require_input_desktop("desktop_foreground_app_icon") {
            return;
        }
        let t = super::super::paste::capture_foreground().expect("foreground window");
        let icon = load(&t, 48).expect("icon for the foreground app");
        eprintln!("foreground app: {} ({})", icon.name, icon.exe_name);
        assert_eq!(icon.rgba.len(), 48 * 48 * 4);
        assert!(icon.rgba.chunks_exact(4).any(|p| p[3] > 200));
        assert!(!icon.name.is_empty());
    }
}
