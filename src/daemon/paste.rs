//! Text insertion + target capture. TASK win-output implements. Port of OpenWhispr
//! windows-fast-paste.c + clipboard.js Windows path, plus improvements (docs/PLAN.md §8):
//! full clipboard snapshot/restore (all HGLOBAL formats), clipboard-history exclusion formats,
//! wait (<= 1.5 s) for physically held modifiers instead of injecting key-ups, restore focus to the
//! captured HWND, Ctrl+Shift+V for terminals, UIPI (elevated target) detection, restore after 500 ms
//! only if GetClipboardSequenceNumber is unchanged.

use std::ffi::c_void;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{
    CloseHandle, E_ACCESSDENIED, ERROR_ACCESS_DENIED, GetLastError, GlobalFree, HANDLE, HGLOBAL, HWND, LPARAM,
};
use windows::Win32::Security::{
    GetSidSubAuthority, GetSidSubAuthorityCount, GetTokenInformation, TOKEN_MANDATORY_LABEL,
    TOKEN_QUERY, TokenIntegrityLevel,
};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, EnumClipboardFormats, GetClipboardData,
    GetClipboardFormatNameW, GetClipboardSequenceNumber, OpenClipboard, RegisterClipboardFormatW,
    SetClipboardData,
};
use windows::Win32::System::Memory::{
    GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock,
};
use windows::Win32::System::Threading::{
    AttachThreadInput, GetCurrentProcess, GetCurrentThreadId, OpenProcess, OpenProcessToken,
    PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBD_EVENT_FLAGS, KEYBDINPUT,
    KEYEVENTF_KEYUP, MAPVK_VK_TO_VSC, MapVirtualKeyW, SendInput, VIRTUAL_KEY, VK_CONTROL,
    VK_LCONTROL, VK_LMENU, VK_LSHIFT, VK_LWIN, VK_MENU, VK_RCONTROL, VK_RMENU, VK_RSHIFT, VK_RWIN,
    VK_SHIFT, VK_V, VK_F24,
};
use windows::Win32::UI::WindowsAndMessaging::{
    ASFW_ANY, AllowSetForegroundWindow, BringWindowToTop, CreateWindowExW, DestroyWindow, EnumChildWindows,
    GA_ROOTOWNER, GetAncestor, GetClassNameW, GetForegroundWindow, GetWindowThreadProcessId,
    HWND_MESSAGE, IsIconic, IsWindow, SW_RESTORE, SetForegroundWindow, ShowWindow, WINDOW_EX_STYLE,
    WS_POPUP,
};
use windows::core::{BOOL, PCWSTR, PWSTR, w};

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Target {
    pub hwnd: isize,
    pub pid: u32,
    /// Full exe path and file name (lowercase file name used for matching).
    pub exe_path: String,
    pub exe_name: String,
    pub class_name: String,
    pub is_terminal: bool,
    /// Target runs at higher integrity than us (paste will be blocked by UIPI).
    pub elevated: bool,
}

#[derive(Clone, Copy, Debug)]
pub struct PasteOptions {
    pub restore_clipboard: bool,
    pub smart_spacing: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PasteOutcome {
    Pasted,
    /// Text left on the clipboard; reason is user-facing.
    CopiedOnly { reason: String },
}

/// `dwExtraInfo` stamped on the keystrokes we inject ("OTPS"), so our own hook can recognise them.
pub const PASTE_EXTRA_INFO: usize = 0x4F54_5053;

const ELEVATED_REASON: &str = "Text copied \u{2014} the target app is running as administrator";
const HELD_REASON: &str = "Release the hotkey to paste";
const CLIPBOARD_BUSY_REASON: &str = "Text could not be placed on the clipboard";
const NO_FOCUS_REASON: &str = "Text copied \u{2014} could not return to the original window";
const SEND_FAILED_REASON: &str = "Text copied \u{2014} the paste keystroke was blocked";

/// How long we wait for the user to let go of Ctrl/Alt/Shift/Win before giving up.
const MODIFIER_WAIT: Duration = Duration::from_millis(1500);
/// Delay before the previous clipboard contents are put back.
const RESTORE_DELAY: Duration = Duration::from_millis(500);
/// Clipboard snapshots above this size are not taken (the paste proceeds without restoring).
const SNAPSHOT_CAP: usize = 64 << 20;

const TERMINAL_CLASSES: &[&str] = &[
    "ConsoleWindowClass",
    "CASCADIA_HOSTING_WINDOW_CLASS",
    "mintty",
    "VirtualConsoleClass",
    "PuTTY",
    "Alacritty",
    "org.wezfurlong.wezterm",
    "Hyper",
    "TMobaXterm",
    "kitty",
];

/// Electron-based terminals share `Chrome_WidgetWin_1`, so they are matched by exe name.
const TERMINAL_EXES: &[&str] = &[
    "termius.exe",
    "tabby.exe",
    "wave.exe",
    "rio.exe",
    "windowsterminal.exe",
    "wezterm-gui.exe",
    "alacritty.exe",
];

/// Terminal emulators paste with Ctrl+Shift+V. Matching is ASCII case-insensitive.
pub fn is_terminal(class_name: &str, exe_name: &str) -> bool {
    TERMINAL_CLASSES.iter().any(|c| c.eq_ignore_ascii_case(class_name))
        || TERMINAL_EXES.iter().any(|e| e.eq_ignore_ascii_case(exe_name))
}

// ---------------------------------------------------------------------------------------------
// Smart spacing
// ---------------------------------------------------------------------------------------------

/// Unspaced scripts (Han, kana) and CJK punctuation / fullwidth forms. Hangul is excluded on
/// purpose (Korean separates words with spaces). Mirrors OpenWhispr's `ENDS_WITH_CJK`, and also
/// counts the Common-script katakana marks U+30FB and U+30FC, which end many Japanese words.
fn is_unspaced_script(c: char) -> bool {
    matches!(c as u32,
        0x2E80..=0x2FDF            // CJK radicals, Kangxi radicals
        | 0x3000..=0x303F          // CJK symbols and punctuation
        | 0x3041..=0x3096 | 0x309D..=0x30FF // hiragana, katakana (incl. 30FB, 30FC)
        | 0x3400..=0x4DBF | 0x4E00..=0x9FFF // Han
        | 0x31F0..=0x31FF | 0x32D0..=0x32FE | 0x3300..=0x3357 // katakana ext, enclosed, compat
        | 0xF900..=0xFAFF          // CJK compatibility ideographs
        | 0xFE10..=0xFE1F | 0xFE30..=0xFE4F // vertical / compatibility forms
        | 0xFF00..=0xFF65          // fullwidth forms
        | 0xFF66..=0xFF9F          // halfwidth katakana
        | 0x1B000..=0x1B16F        // kana supplement / extended
        | 0x1F200
        | 0x20000..=0x323AF        // Han extensions B..H
    )
}

/// OpenWhispr smartSpacing: append one space unless text ends with whitespace or CJK.
pub fn apply_smart_spacing(text: &str) -> String {
    match text.chars().next_back() {
        None => String::new(),
        Some(c) if c.is_whitespace() || is_unspaced_script(c) => text.to_owned(),
        Some(_) => {
            let mut s = String::with_capacity(text.len() + 1);
            s.push_str(text);
            s.push(' ');
            s
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Target capture
// ---------------------------------------------------------------------------------------------

fn to_hwnd(h: isize) -> HWND {
    HWND(h as *mut c_void)
}

fn class_of(hwnd: HWND) -> String {
    let mut buf = [0u16; 256];
    // SAFETY: buf is a valid, writable slice for the duration of the call.
    let n = unsafe { GetClassNameW(hwnd, &mut buf) };
    String::from_utf16_lossy(&buf[..n.max(0) as usize])
}

fn pid_of(hwnd: HWND) -> u32 {
    let mut pid = 0u32;
    // SAFETY: `pid` is a valid out pointer.
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    pid
}

/// Full image path of `pid` (needs only PROCESS_QUERY_LIMITED_INFORMATION).
pub(crate) fn exe_path_of(pid: u32) -> Option<String> {
    // SAFETY: plain Win32 calls; the handle is closed before returning.
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = vec![0u16; 1024];
        let mut len = buf.len() as u32;
        let r = QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len);
        let _ = CloseHandle(h);
        r.ok()?;
        Some(String::from_utf16_lossy(&buf[..len as usize]))
    }
}

fn file_name(path: &str) -> &str {
    path.rsplit(['\\', '/']).next().unwrap_or(path)
}

struct ChildSearch {
    frame_pid: u32,
    /// Process of the first child window owned by another process.
    any_pid: u32,
    /// Process owning a `Windows.UI.Core.CoreWindow` child.
    core_pid: u32,
}

unsafe extern "system" fn enum_child(hwnd: HWND, lparam: LPARAM) -> BOOL {
    // SAFETY: lparam is the address of the ChildSearch owned by `resolve_uwp_pid`, alive for the
    // whole (synchronous) EnumChildWindows call.
    let s = unsafe { &mut *(lparam.0 as *mut ChildSearch) };
    let pid = pid_of(hwnd);
    if pid != 0 && pid != s.frame_pid {
        if s.any_pid == 0 {
            s.any_pid = pid;
        }
        if class_of(hwnd) == "Windows.UI.Core.CoreWindow" {
            s.core_pid = pid;
            return BOOL(0);
        }
    }
    BOOL(1)
}

/// UWP apps are hosted by `ApplicationFrameHost.exe`; the real app owns a CoreWindow child.
fn resolve_uwp_pid(frame: HWND, frame_pid: u32) -> u32 {
    let mut s = ChildSearch { frame_pid, any_pid: 0, core_pid: 0 };
    // SAFETY: the callback only dereferences `s`, which outlives the call.
    unsafe {
        let _ = EnumChildWindows(Some(frame), Some(enum_child), LPARAM(&mut s as *mut _ as isize));
    }
    if s.core_pid != 0 { s.core_pid } else if s.any_pid != 0 { s.any_pid } else { frame_pid }
}

const SECURITY_MANDATORY_MEDIUM_RID: u32 = 0x2000;
const SECURITY_MANDATORY_HIGH_RID: u32 = 0x3000;

/// Integrity RID of a process token (`token` must have TOKEN_QUERY).
fn token_integrity(token: HANDLE) -> Option<u32> {
    let mut len = 0u32;
    // SAFETY: first call only queries the required size; the second writes into an 8-byte aligned
    // buffer of exactly that size, which holds a TOKEN_MANDATORY_LABEL followed by its SID.
    unsafe {
        let _ = GetTokenInformation(token, TokenIntegrityLevel, None, 0, &mut len);
        if len == 0 {
            return None;
        }
        let mut buf = vec![0u64; (len as usize).div_ceil(8)];
        GetTokenInformation(
            token,
            TokenIntegrityLevel,
            Some(buf.as_mut_ptr() as *mut c_void),
            len,
            &mut len,
        )
        .ok()?;
        let label = &*(buf.as_ptr() as *const TOKEN_MANDATORY_LABEL);
        let sid = label.Label.Sid;
        if sid.is_invalid() {
            return None;
        }
        let count = *GetSidSubAuthorityCount(sid);
        if count == 0 {
            return None;
        }
        Some(*GetSidSubAuthority(sid, count as u32 - 1))
    }
}

fn our_integrity() -> u32 {
    static LEVEL: OnceLock<u32> = OnceLock::new();
    *LEVEL.get_or_init(|| {
        // SAFETY: querying our own pseudo-handle token.
        unsafe {
            let mut tok = HANDLE::default();
            if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut tok).is_err() {
                return SECURITY_MANDATORY_MEDIUM_RID;
            }
            let lvl = token_integrity(tok).unwrap_or(SECURITY_MANDATORY_MEDIUM_RID);
            let _ = CloseHandle(tok);
            lvl
        }
    })
}

/// True when `pid` runs at a higher integrity level than this process (UIPI would drop our
/// injected keystrokes). Access-denied while opening it counts as elevated.
fn is_elevated_pid(pid: u32) -> bool {
    let ours = our_integrity();
    let denied = |e: &windows::core::Error| e.code() == E_ACCESSDENIED;
    // SAFETY: handle is closed on every path.
    unsafe {
        let proc = match OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) {
            Ok(h) => h,
            Err(e) => return denied(&e) && ours < SECURITY_MANDATORY_HIGH_RID,
        };
        let mut tok = HANDLE::default();
        let r = OpenProcessToken(proc, TOKEN_QUERY, &mut tok);
        let _ = CloseHandle(proc);
        match r {
            Ok(()) => {
                let lvl = token_integrity(tok);
                let _ = CloseHandle(tok);
                lvl.is_some_and(|l| l > ours)
            }
            Err(e) => denied(&e) && ours < SECURITY_MANDATORY_HIGH_RID,
        }
    }
}

/// Describe an arbitrary top-level window as a paste target.
fn describe_window(hwnd: HWND) -> Option<Target> {
    let frame_pid = pid_of(hwnd);
    if frame_pid == 0 {
        return None;
    }
    let class_name = class_of(hwnd);
    let mut pid = frame_pid;
    let mut exe_path = exe_path_of(pid).unwrap_or_default();
    if file_name(&exe_path).eq_ignore_ascii_case("ApplicationFrameHost.exe") {
        let real = resolve_uwp_pid(hwnd, frame_pid);
        if real != frame_pid {
            if let Some(p) = exe_path_of(real) {
                pid = real;
                exe_path = p;
            }
        }
    }
    let exe_name = file_name(&exe_path).to_ascii_lowercase();
    let is_terminal = is_terminal(&class_name, &exe_name);
    Some(Target {
        hwnd: hwnd.0 as isize,
        pid,
        elevated: is_elevated_pid(pid),
        exe_path,
        exe_name,
        class_name,
        is_terminal,
    })
}

/// Foreground window at hotkey time (resolves UWP ApplicationFrameHost to the real app process).
pub fn capture_foreground() -> Option<Target> {
    // SAFETY: trivially safe query.
    let hwnd = unsafe { GetForegroundWindow() };
    if hwnd.is_invalid() {
        return None;
    }
    describe_window(hwnd)
}

// ---------------------------------------------------------------------------------------------
// Clipboard
// ---------------------------------------------------------------------------------------------

const CF_TEXT: u32 = 1;
const CF_BITMAP: u32 = 2;
const CF_METAFILEPICT: u32 = 3;
const CF_OEMTEXT: u32 = 7;
const CF_PALETTE: u32 = 9;
const CF_UNICODETEXT: u32 = 13;
const CF_ENHMETAFILE: u32 = 14;
const CF_OWNERDISPLAY: u32 = 0x80;
const CF_DSPTEXT: u32 = 0x81;
const CF_DSPBITMAP: u32 = 0x82;
const CF_DSPMETAFILEPICT: u32 = 0x83;
const CF_DSPENHMETAFILE: u32 = 0x8E;

/// Formats whose clipboard handle is not an HGLOBAL (GDI objects / owner-drawn), or that only make
/// sense for the process that put them there.
fn skip_format(fmt: u32) -> bool {
    matches!(
        fmt,
        CF_BITMAP
            | CF_METAFILEPICT
            | CF_PALETTE
            | CF_ENHMETAFILE
            | CF_OWNERDISPLAY
            | CF_DSPBITMAP
            | CF_DSPMETAFILEPICT
            | CF_DSPENHMETAFILE
    ) || (0x300..=0x3FF).contains(&fmt) // CF_GDIOBJFIRST..LAST
}

/// OLE bookkeeping formats that point into the previous owner's process.
const SKIPPED_REGISTERED: &[&str] =
    &["DataObject", "Ole Private Data", "OleClipboardPersistOnFlush", "Link Source"];

fn format_name(fmt: u32) -> String {
    let mut buf = [0u16; 128];
    // SAFETY: valid writable slice.
    let n = unsafe { GetClipboardFormatNameW(fmt, &mut buf) };
    String::from_utf16_lossy(&buf[..n.max(0) as usize])
}

fn register_format(name: PCWSTR) -> u32 {
    // SAFETY: `name` is a NUL-terminated wide literal.
    unsafe { RegisterClipboardFormatW(name) }
}

/// Hidden message-only window that owns the clipboard while we write to it. `SetClipboardData`
/// fails for a NULL owner, so every clipboard session creates one on its own thread.
struct Owner(HWND);

impl Owner {
    fn new() -> Option<Owner> {
        // SAFETY: STATIC is a predefined class; the window is destroyed in Drop on this thread.
        let hwnd = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("STATIC"),
                PCWSTR::null(),
                WS_POPUP,
                0,
                0,
                0,
                0,
                Some(HWND_MESSAGE),
                None,
                None,
                None,
            )
        }
        .ok()?;
        Some(Owner(hwnd))
    }
}

impl Drop for Owner {
    fn drop(&mut self) {
        // SAFETY: window was created on this thread.
        unsafe {
            let _ = DestroyWindow(self.0);
        }
    }
}

/// An open clipboard (closed on drop).
struct Clipboard;

impl Clipboard {
    /// OpenClipboard fails while another app holds it: up to 10 tries x 10 ms.
    fn open(owner: &Owner) -> Option<Clipboard> {
        for attempt in 0..10 {
            // SAFETY: owner window is valid for the lifetime of the guard's use.
            if unsafe { OpenClipboard(Some(owner.0)) }.is_ok() {
                return Some(Clipboard);
            }
            if attempt < 9 {
                thread::sleep(Duration::from_millis(10));
            }
        }
        None
    }

    /// Place `bytes` on the (already emptied) clipboard under `fmt`.
    fn set(&self, fmt: u32, bytes: &[u8]) -> bool {
        // SAFETY: standard HGLOBAL hand-off; on success the clipboard owns the memory, on failure
        // we free it. `h` is locked only while copying.
        unsafe {
            let Ok(h) = GlobalAlloc(GMEM_MOVEABLE, bytes.len().max(1)) else {
                return false;
            };
            let p = GlobalLock(h);
            if p.is_null() {
                let _ = GlobalFree(Some(h));
                return false;
            }
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), p as *mut u8, bytes.len());
            let _ = GlobalUnlock(h);
            if SetClipboardData(fmt, Some(HANDLE(h.0))).is_err() {
                let _ = GlobalFree(Some(h));
                return false;
            }
            true
        }
    }

    fn empty(&self) -> bool {
        // SAFETY: clipboard is open.
        unsafe { EmptyClipboard() }.is_ok()
    }
}

impl Drop for Clipboard {
    fn drop(&mut self) {
        // SAFETY: we hold the clipboard.
        unsafe {
            let _ = CloseClipboard();
        }
    }
}

/// Every HGLOBAL-backed clipboard format at one point in time.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Snapshot {
    formats: Vec<(u32, Vec<u8>)>,
}

impl Snapshot {
    pub(crate) fn format_count(&self) -> usize {
        self.formats.len()
    }
}

/// Read every restorable format. `None` when the clipboard can't be opened or holds too much data.
pub(crate) fn snapshot_clipboard() -> Option<Snapshot> {
    read_clipboard_snapshot()?.0
}

/// One pass over the clipboard. Outer `None`: it could not be opened. Inner `None`: it holds more
/// than [`SNAPSHOT_CAP`]. The `u32` is `GetClipboardSequenceNumber` read while the clipboard was
/// still open, so nobody can have changed it between the read and the number (rendering a
/// delay-rendered format does not count as a change).
fn read_clipboard_snapshot() -> Option<(Option<Snapshot>, u32)> {
    let owner = Owner::new()?;
    let cb = Clipboard::open(&owner)?;
    let mut formats: Vec<(u32, Vec<u8>)> = Vec::new();
    let mut total = 0usize;
    let mut has_unicode = false;
    let mut too_big = false;
    let mut fmt = 0u32;
    // SAFETY: the clipboard stays open for the whole loop; handles returned by GetClipboardData
    // are owned by the clipboard and only read here (locked, copied, unlocked).
    let seq = unsafe {
        loop {
            fmt = EnumClipboardFormats(fmt);
            if fmt == 0 {
                break;
            }
            if fmt == CF_UNICODETEXT {
                has_unicode = true;
            }
        }
        fmt = 0;
        loop {
            fmt = EnumClipboardFormats(fmt);
            if fmt == 0 {
                break;
            }
            if skip_format(fmt) {
                continue;
            }
            // Windows re-synthesizes these from CF_UNICODETEXT.
            if has_unicode && matches!(fmt, CF_TEXT | CF_OEMTEXT | CF_DSPTEXT) {
                continue;
            }
            if fmt >= 0xC000 && SKIPPED_REGISTERED.contains(&format_name(fmt).as_str()) {
                continue;
            }
            let Ok(h) = GetClipboardData(fmt) else { continue };
            let hg = HGLOBAL(h.0);
            let size = GlobalSize(hg);
            if size == 0 {
                continue;
            }
            total += size;
            if total > SNAPSHOT_CAP {
                too_big = true;
                break;
            }
            let p = GlobalLock(hg);
            if p.is_null() {
                continue;
            }
            formats.push((fmt, std::slice::from_raw_parts(p as *const u8, size).to_vec()));
            let _ = GlobalUnlock(hg);
        }
        GetClipboardSequenceNumber()
    };
    drop(cb);
    Some((if too_big { None } else { Some(Snapshot { formats }) }, seq))
}

/// Put a snapshot back (an empty snapshot clears the clipboard, as it was empty before).
pub(crate) fn restore_clipboard(snap: &Snapshot) -> bool {
    let Some(owner) = Owner::new() else { return false };
    let Some(cb) = Clipboard::open(&owner) else { return false };
    if !cb.empty() {
        return false;
    }
    let mut ok = true;
    for (fmt, bytes) in &snap.formats {
        ok &= cb.set(*fmt, bytes);
    }
    // The user's data is already known to their clipboard tools; putting it back must not make
    // Win+V history or sync pick up a second copy. A previous owner's own flags are preserved.
    if !snap.formats.is_empty() {
        let present: Vec<u32> = snap.formats.iter().map(|(f, _)| *f).collect();
        add_exclusion_markers(&cb, &present);
    }
    ok
}

fn utf16_bytes(text: &str) -> Vec<u8> {
    let mut v: Vec<u8> = Vec::with_capacity(text.len() * 2 + 2);
    for u in text.encode_utf16().chain(std::iter::once(0)) {
        v.extend_from_slice(&u.to_le_bytes());
    }
    v
}

/// Windows clipboard text uses CRLF line breaks; classic Edit controls draw a lone LF as a box and
/// keep everything on one line. A lone `\n` becomes `\r\n` (existing CRLF pairs are left alone).
fn to_crlf(text: &str) -> std::borrow::Cow<'_, str> {
    if !text.contains('\n') {
        return std::borrow::Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len() + 8);
    let mut prev = ' ';
    for c in text.chars() {
        if c == '\n' && prev != '\r' {
            out.push('\r');
        }
        out.push(c);
        prev = c;
    }
    std::borrow::Cow::Owned(out)
}

/// Replace the clipboard with `text`, flagged so Win+V history and cloud sync ignore it.
fn write_text(text: &str) -> bool {
    let Some(owner) = Owner::new() else { return false };
    let Some(cb) = Clipboard::open(&owner) else { return false };
    if !cb.empty() {
        return false;
    }
    let ok = cb.set(CF_UNICODETEXT, &utf16_bytes(&to_crlf(text)));
    add_exclusion_markers(&cb, &[]);
    ok
}

/// Flag the clipboard content so Win+V history, cloud sync and clipboard monitors ignore it
/// (best effort). Formats in `already` were put there by the caller and are left alone.
fn add_exclusion_markers(cb: &Clipboard, already: &[u32]) {
    // Presence (not value) matters for the monitor-exclusion format.
    let zero = 0u32.to_le_bytes();
    for name in [
        w!("ExcludeClipboardContentFromMonitorProcessing"),
        w!("CanIncludeInClipboardHistory"),
        w!("CanUploadToCloudClipboard"),
    ] {
        let f = register_format(name);
        if f != 0 && !already.contains(&f) {
            cb.set(f, &zero);
        }
    }
}

/// Read CF_UNICODETEXT (None if absent).
pub(crate) fn read_clipboard_text() -> Option<String> {
    let owner = Owner::new()?;
    let _cb = Clipboard::open(&owner)?;
    // SAFETY: clipboard open; handle read-only locked and copied.
    unsafe {
        let h = GetClipboardData(CF_UNICODETEXT).ok()?;
        let hg = HGLOBAL(h.0);
        let size = GlobalSize(hg);
        let p = GlobalLock(hg);
        if p.is_null() {
            return None;
        }
        let units = std::slice::from_raw_parts(p as *const u16, size / 2);
        let end = units.iter().position(|&u| u == 0).unwrap_or(units.len());
        let s = String::from_utf16_lossy(&units[..end]);
        let _ = GlobalUnlock(hg);
        Some(s)
    }
}

/// Put text on the clipboard (excluded from clipboard history). Returns success.
pub fn copy_to_clipboard(text: &str) -> bool {
    write_text(text)
}

// ---------------------------------------------------------------------------------------------
// Focus, modifiers, keystrokes
// ---------------------------------------------------------------------------------------------

/// True when the foreground window is `target` or a popup it owns (a dialog, a menu).
fn foreground_matches(target: &Target) -> bool {
    // SAFETY: trivially safe queries.
    unsafe {
        let fg = GetForegroundWindow();
        if fg.is_invalid() {
            return false;
        }
        let t = to_hwnd(target.hwnd);
        fg == t || GetAncestor(fg, GA_ROOTOWNER) == t
    }
}

/// Bring the captured window back to the foreground if focus moved. Returns whether it is there.
fn ensure_foreground(target: &Target) -> bool {
    if target.hwnd == 0 || foreground_matches(target) {
        return true;
    }
    let hwnd = to_hwnd(target.hwnd);
    // SAFETY: trivially safe query.
    if !unsafe { IsWindow(Some(hwnd)) }.as_bool() {
        return false;
    }
    for attempt in 0..2 {
        if attempt == 1 {
            // Windows refuses SetForegroundWindow to a process that did not receive the last input
            // event. A tap of an unassigned key (F24) counts as input and is ignored by apps.
            let _ = send_chord(&[key(VK_F24, false), key(VK_F24, true)]);
        }
        try_set_foreground(hwnd);
        let deadline = Instant::now() + Duration::from_millis(150);
        loop {
            if foreground_matches(target) {
                return true;
            }
            if Instant::now() >= deadline {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
    }
    false
}

/// One `SetForegroundWindow` attempt, with this thread's input attached to the current foreground
/// thread so Windows treats it as the active one.
fn try_set_foreground(hwnd: HWND) {
    // SAFETY: Win32 focus calls; thread input is detached on every path.
    unsafe {
        let fg = GetForegroundWindow();
        let fg_tid = if fg.is_invalid() { 0 } else { GetWindowThreadProcessId(fg, None) };
        let our_tid = GetCurrentThreadId();
        let attached = fg_tid != 0 && fg_tid != our_tid && AttachThreadInput(our_tid, fg_tid, true).as_bool();
        let _ = AllowSetForegroundWindow(ASFW_ANY);
        if IsIconic(hwnd).as_bool() {
            let _ = ShowWindow(hwnd, SW_RESTORE);
        }
        let _ = BringWindowToTop(hwnd);
        let _ = SetForegroundWindow(hwnd);
        if attached {
            let _ = AttachThreadInput(our_tid, fg_tid, false);
        }
    }
}

const MODIFIER_VKS: [VIRTUAL_KEY; 11] = [
    VK_CONTROL, VK_LCONTROL, VK_RCONTROL, VK_MENU, VK_LMENU, VK_RMENU, VK_SHIFT, VK_LSHIFT,
    VK_RSHIFT, VK_LWIN, VK_RWIN,
];

fn modifiers_held() -> bool {
    // SAFETY: GetAsyncKeyState has no preconditions. High bit = currently down.
    MODIFIER_VKS.iter().any(|vk| unsafe { GetAsyncKeyState(vk.0 as i32) } < 0)
}

/// Wait until no modifier is physically held. False if still held after `MODIFIER_WAIT`.
fn wait_modifiers_released() -> bool {
    if !modifiers_held() {
        return true;
    }
    let deadline = Instant::now() + MODIFIER_WAIT;
    while Instant::now() < deadline {
        thread::sleep(Duration::from_millis(5));
        if !modifiers_held() {
            return true;
        }
    }
    false
}

fn key(vk: VIRTUAL_KEY, up: bool) -> INPUT {
    // SAFETY: MapVirtualKeyW has no preconditions.
    let scan = unsafe { MapVirtualKeyW(vk.0 as u32, MAPVK_VK_TO_VSC) } as u16;
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: vk,
                wScan: scan,
                dwFlags: if up { KEYEVENTF_KEYUP } else { KEYBD_EVENT_FLAGS(0) },
                time: 0,
                dwExtraInfo: PASTE_EXTRA_INFO,
            },
        },
    }
}

/// The paste chord as one atomic `SendInput` batch.
fn paste_chord(terminal: bool) -> Vec<INPUT> {
    if terminal {
        vec![
            key(VK_LCONTROL, false),
            key(VK_LSHIFT, false),
            key(VK_V, false),
            key(VK_V, true),
            key(VK_LSHIFT, true),
            key(VK_LCONTROL, true),
        ]
    } else {
        vec![key(VK_LCONTROL, false), key(VK_V, false), key(VK_V, true), key(VK_LCONTROL, true)]
    }
}

/// Why the paste keystroke could not be injected.
enum SendError {
    /// UIPI refused it: the foreground window is more privileged than we are.
    Denied,
    Failed,
}

fn send_chord(inputs: &[INPUT]) -> Result<(), SendError> {
    // SAFETY: `inputs` is a valid slice of fully initialised INPUT structs.
    let sent = unsafe { SendInput(inputs, std::mem::size_of::<INPUT>() as i32) } as usize;
    if sent == inputs.len() {
        return Ok(());
    }
    // SAFETY: reads the calling thread's last error, right after the failing call.
    let denied = sent == 0 && unsafe { GetLastError() } == ERROR_ACCESS_DENIED;
    if sent > 0 {
        // A partial batch may have left Ctrl/Shift down; release them.
        let ups = [key(VK_LSHIFT, true), key(VK_LCONTROL, true)];
        // SAFETY: as above.
        unsafe { SendInput(&ups, std::mem::size_of::<INPUT>() as i32) };
    }
    Err(if denied { SendError::Denied } else { SendError::Failed })
}

// ---------------------------------------------------------------------------------------------
// Paste
// ---------------------------------------------------------------------------------------------

fn copied_only(reason: &str) -> PasteOutcome {
    PasteOutcome::CopiedOnly { reason: reason.to_owned() }
}

/// Core of [`paste_text`]; also returns the clipboard-restore thread so tests can join it.
pub(crate) fn paste_inner(
    text: &str,
    target: Option<&Target>,
    opts: PasteOptions,
) -> (PasteOutcome, Option<JoinHandle<()>>) {
    let text = if opts.smart_spacing { apply_smart_spacing(text) } else { text.to_owned() };
    if text.is_empty() {
        return (PasteOutcome::Pasted, None);
    }
    let owned;
    let target = match target {
        Some(t) => Some(t),
        None => {
            owned = capture_foreground();
            owned.as_ref()
        }
    };

    // UIPI would silently drop our keystrokes: leave the text for a manual paste.
    if target.is_some_and(|t| t.elevated) {
        discard_clipboard_snapshot();
        let reason = if write_text(&text) { ELEVATED_REASON } else { CLIPBOARD_BUSY_REASON };
        return (copied_only(reason), None);
    }

    let pending = if opts.restore_clipboard {
        acquire_pending()
    } else {
        discard_clipboard_snapshot();
        None
    };
    // Any outcome other than a completed paste leaves our text on the clipboard on purpose, so
    // the user's previous contents are not put back over it.
    let fail = |reason: &str| {
        if let Some((gen_id, _)) = &pending {
            abandon_pending(*gen_id);
        }
        (copied_only(reason), None)
    };
    if !write_text(&text) {
        return fail(CLIPBOARD_BUSY_REASON);
    }
    // SAFETY: trivially safe query.
    let seq = unsafe { GetClipboardSequenceNumber() };

    if !wait_modifiers_released() {
        return fail(HELD_REASON);
    }
    if let Some(t) = target {
        if !ensure_foreground(t) {
            return fail(NO_FOCUS_REASON);
        }
    }
    // Give the clipboard-owner change a moment to settle before the target reads it.
    thread::sleep(Duration::from_millis(10));

    let terminal = target.is_some_and(|t| t.is_terminal);
    match send_chord(&paste_chord(terminal)) {
        Ok(()) => {}
        Err(SendError::Denied) => return fail(ELEVATED_REASON),
        Err(SendError::Failed) => return fail(SEND_FAILED_REASON),
    }

    let restore = pending.and_then(|(gen_id, snap)| {
        let spawned = thread::Builder::new().name("clipboard-restore".into()).spawn(move || {
            thread::sleep(RESTORE_DELAY);
            // A newer paste took over this snapshot (or abandoned it): nothing to do here.
            if !take_pending(gen_id) {
                return;
            }
            // Only if nobody (user, other app) has touched the clipboard since our write.
            // SAFETY: trivially safe query.
            if unsafe { GetClipboardSequenceNumber() } == seq && !restore_clipboard(&snap) {
                crate::log_warn!("paste: could not restore the previous clipboard contents");
            }
        });
        match spawned {
            Ok(h) => Some(h),
            Err(_) => {
                // No thread for the delayed restore: leave the dictation text on the clipboard.
                abandon_pending(gen_id);
                crate::log_warn!("paste: could not start the clipboard restore thread");
                None
            }
        }
    });
    (PasteOutcome::Pasted, restore)
}

/// The user's clipboard as it was before the first of possibly several back-to-back pastes, with
/// the generation of the paste currently responsible for restoring it.
static PENDING: Mutex<Option<(u64, Arc<Snapshot>)>> = Mutex::new(None);
static GENERATION: AtomicU64 = AtomicU64::new(1);

/// Snapshot to restore after this paste. If an earlier paste has not restored yet, the clipboard
/// holds its dictation text rather than the user's data, so that earlier snapshot is reused and
/// the earlier restore is cancelled. Otherwise the snapshot taken at recording start is used when
/// the clipboard has not changed since, keeping `GetClipboardData` off the paste's critical path.
/// `None` when the clipboard could not be snapshotted.
fn acquire_pending() -> Option<(u64, Arc<Snapshot>)> {
    acquire_pending_with(&PENDING, &PREFETCH, &SystemClip)
}

fn acquire_pending_with(
    pending: &Mutex<Option<(u64, Arc<Snapshot>)>>,
    prefetch: &Prefetch,
    src: &dyn ClipSource,
) -> Option<(u64, Arc<Snapshot>)> {
    let mut slot = pending.lock().unwrap_or_else(|e| e.into_inner());
    let gen_id = GENERATION.fetch_add(1, Ordering::Relaxed);
    let snap = match slot.take() {
        Some((_, prev)) => {
            // The clipboard holds our own text: a prefetch is not the user's data.
            prefetch.clear();
            prev
        }
        None => match prefetch.take_fresh(src, PREFETCH_WAIT) {
            Prefetched::Usable(s) => s,
            Prefetched::TooBig => return None,
            Prefetched::Miss => Arc::new(src.snapshot()?.0?),
        },
    };
    *slot = Some((gen_id, snap.clone()));
    Some((gen_id, snap))
}

// ---------------------------------------------------------------------------------------------
// Clipboard prefetch (snapshot at recording start, off the paste's critical path)
// ---------------------------------------------------------------------------------------------

/// How long a paste waits for a prefetch that is still reading before it reads the clipboard
/// itself (reading twice at once would only make both slower).
const PREFETCH_WAIT: Duration = Duration::from_secs(5);

/// Where clipboard state comes from (injectable for tests).
trait ClipSource: Send + Sync {
    fn sequence(&self) -> u32;
    /// See [`read_clipboard_snapshot`].
    fn snapshot(&self) -> Option<(Option<Snapshot>, u32)>;
}

struct SystemClip;

impl ClipSource for SystemClip {
    fn sequence(&self) -> u32 {
        // SAFETY: trivially safe query.
        unsafe { GetClipboardSequenceNumber() }
    }
    fn snapshot(&self) -> Option<(Option<Snapshot>, u32)> {
        read_clipboard_snapshot()
    }
}

enum PrefetchState {
    Empty,
    /// A worker with this epoch is reading the clipboard.
    Running(u64),
    /// `snap` is `None` when the clipboard held more than [`SNAPSHOT_CAP`].
    Ready { seq: u32, snap: Option<Arc<Snapshot>> },
}

enum Prefetched {
    Usable(Arc<Snapshot>),
    /// The clipboard (unchanged) is too large to snapshot: paste without restoring.
    TooBig,
    /// Nothing usable: read the clipboard now.
    Miss,
}

struct PrefetchInner {
    /// Bumped by every clear so a worker that finishes late drops its result.
    epoch: u64,
    state: PrefetchState,
}

/// The clipboard snapshot taken when recording starts, kept until the paste uses it.
struct Prefetch {
    inner: Mutex<PrefetchInner>,
    done: Condvar,
}

static PREFETCH: Prefetch = Prefetch::new();

impl Prefetch {
    const fn new() -> Prefetch {
        Prefetch { inner: Mutex::new(PrefetchInner { epoch: 0, state: PrefetchState::Empty }), done: Condvar::new() }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, PrefetchInner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Claim the right to prefetch. `None` when a worker is already reading or the stored
    /// snapshot is still current.
    fn begin(&self, src: &dyn ClipSource) -> Option<u64> {
        let mut g = self.lock();
        match &g.state {
            PrefetchState::Running(_) => return None,
            PrefetchState::Ready { seq, .. } if *seq == src.sequence() => return None,
            _ => {}
        }
        g.epoch += 1;
        let epoch = g.epoch;
        g.state = PrefetchState::Running(epoch);
        Some(epoch)
    }

    fn finish(&self, epoch: u64, result: Option<(Option<Snapshot>, u32)>) {
        let mut g = self.lock();
        if !matches!(g.state, PrefetchState::Running(e) if e == epoch) {
            return; // cleared meanwhile: drop the data
        }
        g.state = match result {
            Some((snap, seq)) => PrefetchState::Ready { seq, snap: snap.map(Arc::new) },
            None => PrefetchState::Empty,
        };
        self.done.notify_all();
    }

    /// Drop the stored snapshot, or make a worker still reading drop its result.
    fn clear(&self) {
        let mut g = self.lock();
        g.epoch += 1;
        g.state = PrefetchState::Empty;
        self.done.notify_all();
    }

    /// Consume the prefetch if the clipboard is unchanged since it was read, waiting up to `wait`
    /// for a worker still reading.
    fn take_fresh(&self, src: &dyn ClipSource, wait: Duration) -> Prefetched {
        let deadline = Instant::now() + wait;
        let mut g = self.lock();
        while matches!(g.state, PrefetchState::Running(_)) {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                g.epoch += 1; // the late result is dropped
                g.state = PrefetchState::Empty;
                return Prefetched::Miss;
            }
            g = self.done.wait_timeout(g, left).unwrap_or_else(|e| e.into_inner()).0;
        }
        match std::mem::replace(&mut g.state, PrefetchState::Empty) {
            PrefetchState::Ready { seq, snap } if seq == src.sequence() => match snap {
                Some(s) => Prefetched::Usable(s),
                None => Prefetched::TooBig,
            },
            _ => Prefetched::Miss,
        }
    }
}

/// Start reading the clipboard on a worker thread, unless a current prefetch exists.
fn prepare_with(prefetch: &'static Prefetch, src: Arc<dyn ClipSource>) -> Option<JoinHandle<()>> {
    let epoch = prefetch.begin(&*src)?;
    let spawned = thread::Builder::new().name("clipboard-prefetch".into()).spawn(move || {
        let result = src.snapshot();
        prefetch.finish(epoch, result);
    });
    match spawned {
        Ok(h) => Some(h),
        Err(_) => {
            prefetch.clear();
            None
        }
    }
}

/// Snapshot the user's clipboard now, on a worker thread, so [`paste_text`] does not read it on
/// the critical path (a delay-rendering owner such as an Excel range renders every format
/// synchronously inside `GetClipboardData`: hundreds of ms to seconds). Call it when recording
/// starts, from a helper thread. The paste reuses the snapshot only while
/// `GetClipboardSequenceNumber` is unchanged and otherwise reads the clipboard itself. Returns
/// at once.
pub fn prepare_clipboard_snapshot() {
    // While an earlier paste has not restored yet, the clipboard holds its dictation text.
    if PENDING.lock().unwrap_or_else(|e| e.into_inner()).is_some() {
        return;
    }
    let _ = prepare_with(&PREFETCH, Arc::new(SystemClip));
}

/// Drop the snapshot taken by [`prepare_clipboard_snapshot`] (dictation cancelled or failed), so
/// large clipboard data is not kept in memory.
pub fn discard_clipboard_snapshot() {
    PREFETCH.clear();
}

/// Claim the restore for `gen_id`; false if a newer paste owns it or it was abandoned.
fn take_pending(gen_id: u64) -> bool {
    let mut slot = PENDING.lock().unwrap_or_else(|e| e.into_inner());
    match slot.as_ref() {
        Some((g, _)) if *g == gen_id => {
            *slot = None;
            true
        }
        _ => false,
    }
}

fn abandon_pending(gen_id: u64) {
    take_pending(gen_id);
}

/// Paste `text` into `target` (or the current foreground window).
pub fn paste_text(text: &str, target: Option<&Target>, opts: PasteOptions) -> PasteOutcome {
    paste_inner(text, target, opts).0
}

// ---------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Foundation::{LRESULT, WPARAM};
    use windows::Win32::UI::WindowsAndMessaging::{
        DefWindowProcW, DispatchMessageW, GetMessageW, MSG, PostMessageW, PostQuitMessage, RegisterClassW, WM_CLOSE,
        WM_DESTROY, WNDCLASSW,
    };

    #[test]
    fn spacing_appends_space() {
        assert_eq!(apply_smart_spacing("hello"), "hello ");
        assert_eq!(apply_smart_spacing("Hello, world."), "Hello, world. ");
        assert_eq!(apply_smart_spacing("caf\u{e9}"), "caf\u{e9} ");
        assert_eq!(apply_smart_spacing("\u{d55c}\u{ad6d}\u{c5b4}"), "\u{d55c}\u{ad6d}\u{c5b4} "); // Hangul keeps spacing
    }

    #[test]
    fn spacing_keeps_whitespace_and_empty() {
        assert_eq!(apply_smart_spacing(""), "");
        assert_eq!(apply_smart_spacing("hi "), "hi ");
        assert_eq!(apply_smart_spacing("hi\n"), "hi\n");
        assert_eq!(apply_smart_spacing("hi\t"), "hi\t");
        assert_eq!(apply_smart_spacing(" "), " ");
        assert_eq!(apply_smart_spacing("hi\u{a0}"), "hi\u{a0}");
    }

    #[test]
    fn spacing_skips_cjk() {
        for s in [
            "\u{4f60}\u{597d}",             // Han
            "\u{3067}\u{3059}\u{3002}",     // hiragana + ideographic full stop
            "\u{30b3}\u{30f3}\u{30d4}\u{30e5}\u{30fc}\u{30bf}\u{30fc}", // katakana ending in the long-vowel mark
            "\u{3053}\u{3093}\u{306b}\u{3061}\u{306f}",
            "\u{ff01}",                     // fullwidth !
            "\u{ff76}",                     // halfwidth katakana
            "\u{20000}",                    // Han extension B
            "\u{fe30}",                     // compatibility form
        ] {
            assert_eq!(apply_smart_spacing(s), s, "{s:?}");
        }
        // A latin tail after CJK still gets a space.
        assert_eq!(apply_smart_spacing("\u{4f60}\u{597d} ok"), "\u{4f60}\u{597d} ok ");
    }

    #[test]
    fn terminal_classes() {
        for c in [
            "ConsoleWindowClass",
            "CASCADIA_HOSTING_WINDOW_CLASS",
            "mintty",
            "VirtualConsoleClass",
            "PuTTY",
            "Alacritty",
            "org.wezfurlong.wezterm",
            "Hyper",
            "TMobaXterm",
            "kitty",
        ] {
            assert!(is_terminal(c, "x.exe"), "{c}");
        }
        assert!(is_terminal("consolewindowclass", ""));
        assert!(is_terminal("MINTTY", ""));
    }

    #[test]
    fn terminal_exes() {
        for e in [
            "termius.exe",
            "Tabby.exe",
            "WAVE.EXE",
            "rio.exe",
            "windowsterminal.exe",
            "wezterm-gui.exe",
            "alacritty.exe",
        ] {
            assert!(is_terminal("Chrome_WidgetWin_1", e), "{e}");
        }
    }

    #[test]
    fn non_terminals() {
        assert!(!is_terminal("Notepad", "notepad.exe"));
        assert!(!is_terminal("Chrome_WidgetWin_1", "code.exe"));
        assert!(!is_terminal("CabinetWClass", "explorer.exe"));
        assert!(!is_terminal("", ""));
        // Exact match only: a longer class name is not a terminal.
        assert!(!is_terminal("HyperText", "x.exe"));
    }

    #[test]
    fn chord_shapes() {
        let c = paste_chord(false);
        assert_eq!(c.len(), 4);
        let t = paste_chord(true);
        assert_eq!(t.len(), 6);
        // SAFETY: we built every INPUT as a keyboard input.
        unsafe {
            assert_eq!(t[1].Anonymous.ki.wVk, VK_LSHIFT);
            assert_eq!(t[2].Anonymous.ki.wVk, VK_V);
            assert_eq!(t[2].Anonymous.ki.dwFlags, KEYBD_EVENT_FLAGS(0));
            assert_eq!(t[3].Anonymous.ki.dwFlags, KEYEVENTF_KEYUP);
            assert_eq!(c[0].Anonymous.ki.dwExtraInfo, PASTE_EXTRA_INFO);
            assert_ne!(c[1].Anonymous.ki.wScan, 0);
        }
    }

    #[test]
    fn utf16_has_terminator() {
        assert_eq!(utf16_bytes("a"), vec![b'a', 0, 0, 0]);
        assert_eq!(utf16_bytes(""), vec![0, 0]);
    }

    #[test]
    fn crlf_normalisation() {
        assert_eq!(to_crlf("a"), "a");
        assert_eq!(to_crlf("a\nb"), "a\r\nb");
        assert_eq!(to_crlf("a\r\nb"), "a\r\nb");
        assert_eq!(to_crlf("a\n\nb\n"), "a\r\n\r\nb\r\n");
        assert_eq!(to_crlf(""), "");
    }

    #[test]
    fn file_name_split() {
        assert_eq!(file_name(r"C:\Windows\explorer.exe"), "explorer.exe");
        assert_eq!(file_name("a.exe"), "a.exe");
    }

    #[test]
    fn pending_restore_ownership() {
        *PENDING.lock().unwrap() = Some((100, Arc::new(Snapshot::default())));
        assert!(!take_pending(99), "wrong generation");
        assert!(take_pending(100));
        assert!(!take_pending(100), "already taken");
        *PENDING.lock().unwrap() = Some((7, Arc::new(Snapshot::default())));
        abandon_pending(7);
        assert!(PENDING.lock().unwrap().is_none());
    }

    /// Clipboard stand-in that counts how often it is read.
    struct FakeClip {
        seq: std::sync::atomic::AtomicU32,
        reads: std::sync::atomic::AtomicUsize,
        delay: Duration,
        too_big: bool,
    }

    impl FakeClip {
        fn new(seq: u32) -> Arc<FakeClip> {
            Arc::new(FakeClip { seq: seq.into(), reads: 0.into(), delay: Duration::ZERO, too_big: false })
        }
        fn reads(&self) -> usize {
            self.reads.load(Ordering::SeqCst)
        }
        fn set_seq(&self, seq: u32) {
            self.seq.store(seq, Ordering::SeqCst);
        }
    }

    impl ClipSource for FakeClip {
        fn sequence(&self) -> u32 {
            self.seq.load(Ordering::SeqCst)
        }
        fn snapshot(&self) -> Option<(Option<Snapshot>, u32)> {
            thread::sleep(self.delay);
            self.reads.fetch_add(1, Ordering::SeqCst);
            let snap = Snapshot { formats: vec![(CF_UNICODETEXT, vec![1, 2, 3])] };
            Some((if self.too_big { None } else { Some(snap) }, self.sequence()))
        }
    }

    fn leaked_prefetch() -> &'static Prefetch {
        Box::leak(Box::new(Prefetch::new()))
    }

    fn pending_slot() -> Mutex<Option<(u64, Arc<Snapshot>)>> {
        Mutex::new(None)
    }

    #[test]
    fn fresh_prefetch_means_no_snapshot_on_the_paste_path() {
        let (pre, pending, clip) = (leaked_prefetch(), pending_slot(), FakeClip::new(5));
        prepare_with(pre, clip.clone()).expect("started").join().unwrap();
        assert_eq!(clip.reads(), 1);
        let (_, snap) = acquire_pending_with(&pending, pre, &*clip).expect("snapshot");
        assert_eq!(clip.reads(), 1, "the paste must not read the clipboard again");
        assert_eq!(snap.format_count(), 1);
        // The prefetch was consumed: a second paste (nothing pending) reads again.
        pending.lock().unwrap().take();
        acquire_pending_with(&pending, pre, &*clip).expect("snapshot");
        assert_eq!(clip.reads(), 2);
    }

    #[test]
    fn changed_sequence_snapshots_again() {
        let (pre, pending, clip) = (leaked_prefetch(), pending_slot(), FakeClip::new(5));
        prepare_with(pre, clip.clone()).expect("started").join().unwrap();
        clip.set_seq(6); // the user copied something after recording started
        assert!(acquire_pending_with(&pending, pre, &*clip).is_some());
        assert_eq!(clip.reads(), 2, "stale prefetch must not be reused");
    }

    #[test]
    fn no_prefetch_snapshots_on_the_paste_path() {
        let (pre, pending, clip) = (leaked_prefetch(), pending_slot(), FakeClip::new(1));
        assert!(acquire_pending_with(&pending, pre, &*clip).is_some());
        assert_eq!(clip.reads(), 1);
    }

    #[test]
    fn discarded_prefetch_is_not_used() {
        let (pre, pending, clip) = (leaked_prefetch(), pending_slot(), FakeClip::new(1));
        prepare_with(pre, clip.clone()).unwrap().join().unwrap();
        pre.clear();
        assert!(matches!(pre.lock().state, PrefetchState::Empty));
        assert!(acquire_pending_with(&pending, pre, &*clip).is_some());
        assert_eq!(clip.reads(), 2);
    }

    #[test]
    fn current_prefetch_is_not_repeated_but_a_stale_one_is() {
        let (pre, clip) = (leaked_prefetch(), FakeClip::new(1));
        prepare_with(pre, clip.clone()).unwrap().join().unwrap();
        assert!(prepare_with(pre, clip.clone()).is_none(), "still current");
        clip.set_seq(2);
        prepare_with(pre, clip.clone()).expect("stale").join().unwrap();
        assert_eq!(clip.reads(), 2);
    }

    #[test]
    fn earlier_pending_snapshot_wins_and_drops_the_prefetch() {
        let (pre, pending, clip) = (leaked_prefetch(), pending_slot(), FakeClip::new(1));
        let user = Arc::new(Snapshot { formats: vec![(CF_UNICODETEXT, vec![9])] });
        *pending.lock().unwrap() = Some((1, user.clone()));
        prepare_with(pre, clip.clone()).unwrap().join().unwrap();
        let (_, snap) = acquire_pending_with(&pending, pre, &*clip).unwrap();
        assert!(Arc::ptr_eq(&snap, &user));
        assert!(matches!(pre.lock().state, PrefetchState::Empty));
        assert_eq!(clip.reads(), 1);
    }

    #[test]
    fn paste_waits_for_a_running_prefetch_instead_of_reading_twice() {
        let (pre, pending) = (leaked_prefetch(), pending_slot());
        let clip = Arc::new(FakeClip {
            seq: 3.into(),
            reads: 0.into(),
            delay: Duration::from_millis(150),
            too_big: false,
        });
        let worker = prepare_with(pre, clip.clone()).expect("started");
        assert!(acquire_pending_with(&pending, pre, &*clip).is_some());
        worker.join().unwrap();
        assert_eq!(clip.reads(), 1);
    }

    #[test]
    fn oversized_clipboard_is_remembered_and_not_read_again() {
        let (pre, pending) = (leaked_prefetch(), pending_slot());
        let clip = Arc::new(FakeClip { seq: 3.into(), reads: 0.into(), delay: Duration::ZERO, too_big: true });
        prepare_with(pre, clip.clone()).unwrap().join().unwrap();
        assert!(acquire_pending_with(&pending, pre, &*clip).is_none());
        assert_eq!(clip.reads(), 1);
        assert!(pending.lock().unwrap().is_none());
    }

    #[test]
    fn late_prefetch_result_after_clear_is_dropped() {
        let pre = leaked_prefetch();
        let clip = FakeClip::new(1);
        let epoch = pre.begin(&*clip).unwrap();
        pre.clear();
        pre.finish(epoch, clip.snapshot());
        assert!(matches!(pre.lock().state, PrefetchState::Empty));
    }

    #[test]
    fn describe_own_window() {
        let owner = Owner::new().expect("window");
        let t = describe_window(owner.0).expect("target");
        assert_eq!(t.pid, std::process::id());
        assert!(t.exe_name.starts_with("opentalk"), "{t:?}");
        assert_eq!(t.class_name, "Static");
        assert!(!t.is_terminal);
        assert!(!t.elevated);
        assert!(t.exe_path.to_ascii_lowercase().ends_with(&t.exe_name));
    }

    #[test]
    fn uwp_frame_windows_resolve_to_the_real_app() {
        use windows::Win32::UI::WindowsAndMessaging::{EnumWindows, IsWindowVisible};
        unsafe extern "system" fn collect(hwnd: HWND, lp: LPARAM) -> BOOL {
            // SAFETY: lp points at a Vec<isize> owned by the caller for the duration of the call.
            let v = unsafe { &mut *(lp.0 as *mut Vec<isize>) };
            if unsafe { IsWindowVisible(hwnd) }.as_bool() && class_of(hwnd) == "ApplicationFrameWindow" {
                v.push(hwnd.0 as isize);
            }
            BOOL(1)
        }
        let mut frames: Vec<isize> = Vec::new();
        // SAFETY: callback only touches `frames`.
        unsafe {
            let _ = EnumWindows(Some(collect), LPARAM(&mut frames as *mut _ as isize));
        }
        for h in frames {
            let t = describe_window(to_hwnd(h)).expect("target");
            eprintln!("uwp frame -> {} (pid {})", t.exe_name, t.pid);
            // Suspended apps keep a frame without a CoreWindow child; those stay as the host.
            assert!(!t.exe_name.is_empty());
        }
    }

    #[test]
    fn integrity_is_known() {
        assert!(our_integrity() >= 0x1000);
    }

    // ----- desktop tests: touch the real clipboard / windows. Run with `--ignored --test-threads=1`.

    /// Serializes desktop tests (the clipboard is process-global).
    static DESKTOP: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    #[ignore]
    fn desktop_clipboard_snapshot_roundtrip() {
        let _g = DESKTOP.lock().unwrap_or_else(|e| e.into_inner());
        let before = snapshot_clipboard().expect("snapshot");
        // Put known multi-format content on the clipboard.
        let fmt_custom = register_format(w!("OpenTalkTestFormat"));
        {
            let owner = Owner::new().unwrap();
            let cb = Clipboard::open(&owner).unwrap();
            assert!(cb.empty());
            assert!(cb.set(CF_UNICODETEXT, &utf16_bytes("snapshot test \u{1f600}")));
            assert!(cb.set(fmt_custom, &[1, 2, 3, 4, 5]));
        }
        let known = snapshot_clipboard().expect("snapshot known");
        assert!(known.format_count() >= 2);
        // Clobber with dictation text, then restore the known snapshot.
        assert!(copy_to_clipboard("clobbered"));
        assert_eq!(read_clipboard_text().as_deref(), Some("clobbered"));
        assert!(restore_clipboard(&known));
        assert_eq!(read_clipboard_text().as_deref(), Some("snapshot test \u{1f600}"));
        let again = snapshot_clipboard().expect("snapshot again");
        let get = |s: &Snapshot, f: u32| s.formats.iter().find(|(x, _)| *x == f).map(|(_, b)| b.clone());
        assert_eq!(get(&again, fmt_custom), Some(vec![1, 2, 3, 4, 5]));
        assert_eq!(get(&again, CF_UNICODETEXT), get(&known, CF_UNICODETEXT));
        // Put the user's original clipboard back.
        assert!(restore_clipboard(&before));
    }

    #[test]
    #[ignore]
    fn desktop_clipboard_history_flags() {
        let _g = DESKTOP.lock().unwrap_or_else(|e| e.into_inner());
        let before = snapshot_clipboard().expect("snapshot");
        assert!(copy_to_clipboard("flags"));
        let names: Vec<String> = {
            let owner = Owner::new().unwrap();
            let _cb = Clipboard::open(&owner).unwrap();
            let mut v = Vec::new();
            let mut f = 0;
            // SAFETY: clipboard open.
            unsafe {
                loop {
                    f = EnumClipboardFormats(f);
                    if f == 0 {
                        break;
                    }
                    v.push(format_name(f));
                }
            }
            v
        };
        assert!(names.iter().any(|n| n == "ExcludeClipboardContentFromMonitorProcessing"), "{names:?}");
        assert!(names.iter().any(|n| n == "CanIncludeInClipboardHistory"), "{names:?}");
        assert!(names.iter().any(|n| n == "CanUploadToCloudClipboard"), "{names:?}");
        assert!(restore_clipboard(&before));
    }

    #[test]
    #[ignore]
    fn desktop_paste_elevated_target_copies_only() {
        let _g = DESKTOP.lock().unwrap_or_else(|e| e.into_inner());
        let before = snapshot_clipboard().expect("snapshot");
        let target = Target { hwnd: 1, pid: 1, elevated: true, ..Default::default() };
        let out = paste_text(
            "elevated text",
            Some(&target),
            PasteOptions { restore_clipboard: true, smart_spacing: true },
        );
        assert_eq!(out, PasteOutcome::CopiedOnly { reason: ELEVATED_REASON.to_owned() });
        // Nothing was injected and nothing restores over the text the user must paste by hand.
        thread::sleep(RESTORE_DELAY + Duration::from_millis(200));
        assert_eq!(read_clipboard_text().as_deref(), Some("elevated text "));
        assert!(restore_clipboard(&before));
    }

    /// A clipboard owner that renders on demand, like an Excel range: each `GetClipboardData`
    /// on one of its formats takes `RENDER_MS`. Returns the owner's thread and window.
    const RENDER_MS: u64 = 150;
    const RENDER_FORMATS: usize = 4;

    unsafe extern "system" fn delay_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
        const WM_RENDERFORMAT: u32 = 0x0305;
        match msg {
            WM_RENDERFORMAT => {
                thread::sleep(Duration::from_millis(RENDER_MS));
                // SAFETY: the requesting thread holds the clipboard open; ownership of the 1 MiB
                // block passes to the clipboard on success.
                unsafe {
                    if let Ok(h) = GlobalAlloc(GMEM_MOVEABLE, 1 << 20) {
                        if SetClipboardData(wp.0 as u32, Some(HANDLE(h.0))).is_err() {
                            let _ = GlobalFree(Some(h));
                        }
                    }
                }
                LRESULT(0)
            }
            WM_DESTROY => {
                // SAFETY: plain message-loop call on the window's own thread.
                unsafe { PostQuitMessage(0) };
                LRESULT(0)
            }
            // SAFETY: forwarding the unhandled message with the original arguments.
            _ => unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
        }
    }

    /// Put `RENDER_FORMATS` delay-rendered formats on the clipboard, owned by a window that
    /// serves them from its own thread. Returns the window to close later.
    fn delay_rendered_owner() -> (JoinHandle<()>, isize) {
        let (tx, rx) = std::sync::mpsc::channel();
        let th = thread::spawn(move || {
            // SAFETY: standard window class + message loop on this thread; the window is
            // destroyed by WM_CLOSE (posted by the test) before the thread ends.
            unsafe {
                let class = w!("OpenTalkDelayRenderOwner");
                let wc = WNDCLASSW { lpfnWndProc: Some(delay_proc), lpszClassName: class, ..Default::default() };
                RegisterClassW(&wc);
                let hwnd = CreateWindowExW(
                    WINDOW_EX_STYLE(0), class, PCWSTR::null(), WS_POPUP, 0, 0, 0, 0, Some(HWND_MESSAGE), None, None, None,
                )
                .expect("owner window");
                assert!(OpenClipboard(Some(hwnd)).is_ok());
                let _ = EmptyClipboard();
                for i in 0..RENDER_FORMATS {
                    let name: Vec<u16> = format!("OpenTalkDelayed{i} ").encode_utf16().collect();
                    let fmt = RegisterClipboardFormatW(PCWSTR(name.as_ptr()));
                    // A NULL handle promises the data later (WM_RENDERFORMAT).
                    let _ = SetClipboardData(fmt, None);
                }
                let _ = CloseClipboard();
                tx.send(hwnd.0 as isize).unwrap();
                let mut msg = MSG::default();
                while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                    DispatchMessageW(&msg);
                }
            }
        });
        (th, rx.recv().expect("owner started"))
    }

    /// The fix for the 40 ms budget: with delay-rendered formats on the clipboard, reading them at
    /// paste time costs `RENDER_MS` per format, while the prefetch taken at recording start moves
    /// that off the critical path.
    #[test]
    #[ignore]
    fn desktop_prefetch_keeps_delay_rendered_clipboard_off_the_paste_path() {
        let _g = DESKTOP.lock().unwrap_or_else(|e| e.into_inner());
        let before = snapshot_clipboard().expect("snapshot");
        let (th, owner) = delay_rendered_owner();

        // Without a prefetch the paste pays for every render.
        discard_clipboard_snapshot();
        let t = Instant::now();
        let (gen_id, snap) = acquire_pending().expect("inline snapshot");
        let inline = t.elapsed();
        assert!(snap.format_count() >= RENDER_FORMATS);
        abandon_pending(gen_id);

        // With one, only a sequence-number comparison is left.
        let t = Instant::now();
        prepare_clipboard_snapshot();
        let prepare = t.elapsed();
        let deadline = Instant::now() + Duration::from_secs(10);
        while !matches!(PREFETCH.lock().state, PrefetchState::Ready { .. }) {
            assert!(Instant::now() < deadline, "prefetch never finished");
            thread::sleep(Duration::from_millis(20));
        }
        let t = Instant::now();
        let (gen_id, snap) = acquire_pending().expect("prefetched snapshot");
        let prefetched = t.elapsed();
        assert!(snap.format_count() >= RENDER_FORMATS);
        abandon_pending(gen_id);
        eprintln!("prepare {prepare:?}, paste-path snapshot: inline {inline:?} vs prefetched {prefetched:?}");

        // Put the user's clipboard back and close the owner window.
        assert!(restore_clipboard(&before));
        // SAFETY: posting WM_CLOSE to our own window.
        unsafe {
            let _ = PostMessageW(Some(to_hwnd(owner)), WM_CLOSE, WPARAM(0), LPARAM(0));
        }
        th.join().unwrap();
        assert!(prepare < Duration::from_millis(20), "prepare must not block: {prepare:?}");
        assert!(inline >= Duration::from_millis(RENDER_MS * RENDER_FORMATS as u64 / 2), "{inline:?}");
        assert!(prefetched < Duration::from_millis(5), "{prefetched:?}");
    }

    #[test]
    #[ignore]
    fn desktop_capture_foreground() {
        let t = capture_foreground().expect("a foreground window");
        assert!(t.hwnd != 0 && t.pid != 0);
        assert!(!t.class_name.is_empty());
        eprintln!("foreground: {t:?}");
    }

    #[test]
    #[ignore]
    fn desktop_paste_notepad() {
        paste_into_notepad(false);
    }

    /// Focus moves to another window between the hotkey press and the paste: the paste must still
    /// land in the captured window (and multi-line text arrives intact).
    #[test]
    #[ignore]
    fn desktop_paste_restores_focus() {
        paste_into_notepad(true);
    }

    fn paste_into_notepad(steal_focus: bool) {
        use std::collections::HashSet;
        use std::process::Command;
        use windows::Win32::UI::WindowsAndMessaging::{
            EnumWindows, GetWindowTextLengthW, IsWindowVisible, PostMessageW, SendMessageW,
            WM_CLOSE, WM_GETTEXT, WM_GETTEXTLENGTH,
        };

        let _g = DESKTOP.lock().unwrap_or_else(|e| e.into_inner());

        unsafe extern "system" fn collect(hwnd: HWND, lp: LPARAM) -> BOOL {
            // SAFETY: lp points at a Vec<isize> owned by the caller for the duration of the call.
            let v = unsafe { &mut *(lp.0 as *mut Vec<isize>) };
            if unsafe { IsWindowVisible(hwnd) }.as_bool() && class_of(hwnd) == "Notepad" {
                v.push(hwnd.0 as isize);
            }
            BOOL(1)
        }
        fn notepads() -> Vec<isize> {
            let mut v: Vec<isize> = Vec::new();
            // SAFETY: callback only touches `v`.
            unsafe {
                let _ = EnumWindows(Some(collect), LPARAM(&mut v as *mut _ as isize));
            }
            v
        }
        unsafe extern "system" fn find_edit(hwnd: HWND, lp: LPARAM) -> BOOL {
            // SAFETY: lp points at an isize owned by the caller.
            let out = unsafe { &mut *(lp.0 as *mut isize) };
            let c = class_of(hwnd);
            if c == "RichEditD2DPT" || c == "Edit" || c == "RICHEDIT50W" {
                *out = hwnd.0 as isize;
                return BOOL(0);
            }
            BOOL(1)
        }
        fn window_text(hwnd: HWND) -> String {
            // SAFETY: WM_GETTEXT into a buffer sized from WM_GETTEXTLENGTH + 1.
            unsafe {
                let len = SendMessageW(hwnd, WM_GETTEXTLENGTH, None, None).0.max(0) as usize;
                let mut buf = vec![0u16; len + 2];
                let n = SendMessageW(
                    hwnd,
                    WM_GETTEXT,
                    Some(windows::Win32::Foundation::WPARAM(buf.len())),
                    Some(LPARAM(buf.as_mut_ptr() as isize)),
                )
                .0
                .max(0) as usize;
                String::from_utf16_lossy(&buf[..n.min(buf.len())])
            }
        }

        let user_clip = snapshot_clipboard().expect("snapshot");
        let known: HashSet<isize> = notepads().into_iter().collect();
        let _child = Command::new("notepad.exe").spawn().expect("spawn notepad");
        let mut np = 0isize;
        for _ in 0..100 {
            thread::sleep(Duration::from_millis(100));
            if let Some(h) = notepads().into_iter().find(|h| !known.contains(h)) {
                np = h;
                break;
            }
        }
        assert!(np != 0, "notepad window did not appear");
        thread::sleep(Duration::from_millis(800)); // let the editor finish loading

        // Make notepad the foreground window the way a user would have, then capture it.
        let hwnd = to_hwnd(np);
        let t0 = describe_window(hwnd).expect("describe notepad");
        assert!(ensure_foreground(&t0), "could not foreground notepad");
        let target = capture_foreground().expect("capture");
        assert_eq!(target.exe_name, "notepad.exe", "{target:?}");
        assert!(!target.is_terminal);
        assert!(!target.elevated);

        let thief = if steal_focus {
            use windows::Win32::UI::WindowsAndMessaging::WS_VISIBLE;
            // SAFETY: a visible popup created and destroyed on this thread.
            let h = unsafe {
                CreateWindowExW(
                    WINDOW_EX_STYLE(0),
                    w!("STATIC"),
                    w!("focus thief"),
                    WS_POPUP | WS_VISIBLE,
                    100,
                    100,
                    200,
                    100,
                    None,
                    None,
                    None,
                    None,
                )
            }
            .expect("thief window");
            let t = Target { hwnd: h.0 as isize, ..Default::default() };
            assert!(ensure_foreground(&t), "thief could not take focus");
            assert!(!foreground_matches(&target));
            Some(h)
        } else {
            None
        };

        let text = if steal_focus {
            "OpenTalk focus check\r\nsecond line"
        } else {
            "OpenTalk paste check \u{2014} caf\u{e9} 123"
        };
        assert!(copy_to_clipboard("original user clipboard"));
        let (out, restore) = paste_inner(
            text,
            Some(&target),
            PasteOptions { restore_clipboard: true, smart_spacing: false },
        );
        assert_eq!(out, PasteOutcome::Pasted);
        if let Some(h) = restore {
            h.join().unwrap();
        }
        thread::sleep(Duration::from_millis(400));
        assert_eq!(read_clipboard_text().as_deref(), Some("original user clipboard"), "clipboard restored");

        // Read the text back from the edit control.
        let mut edit = 0isize;
        // SAFETY: callback writes into `edit`.
        unsafe {
            let _ = EnumChildWindows(Some(hwnd), Some(find_edit), LPARAM(&mut edit as *mut _ as isize));
        }
        let got = if edit != 0 { window_text(to_hwnd(edit)) } else { String::new() };
        let _ = unsafe { GetWindowTextLengthW(hwnd) };
        // Close without saving: WM_CLOSE, then kill the process if a prompt keeps it alive.
        let pid = pid_of(hwnd);
        // SAFETY: plain message post.
        unsafe {
            let _ = PostMessageW(Some(hwnd), WM_CLOSE, windows::Win32::Foundation::WPARAM(0), LPARAM(0));
        }
        thread::sleep(Duration::from_millis(700));
        if notepads().contains(&np) {
            let _ = Command::new("taskkill").args(["/F", "/PID", &pid.to_string()]).output();
        }
        assert!(restore_clipboard(&user_clip));
        if let Some(h) = thief {
            // SAFETY: created above on this thread.
            unsafe {
                let _ = DestroyWindow(h);
            }
        }
        assert!(edit != 0, "edit control not found");
        let norm = |s: &str| s.replace("\r\n", "\n").replace('\r', "\n");
        assert!(norm(&got).contains(&norm(text)), "notepad text was {got:?}");
    }
}
