//! Daemon <-> settings-process signalling via window messages to the daemon's hidden
//! message window (class `DAEMON_WINDOW_CLASS`). Config/history live in files; messages only notify.
//! TASK core-data implements `notify_daemon` (FindWindowW + PostMessageW) and `daemon_running`.
use std::sync::OnceLock;
use windows::Win32::Foundation::{CloseHandle, ERROR_ACCESS_DENIED, ERROR_ALREADY_EXISTS, GetLastError, HANDLE, HWND, LPARAM, WPARAM};
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::WindowsAndMessaging::{
    FindWindowExW, FindWindowW, HWND_MESSAGE, PostMessageW, RegisterWindowMessageW, SMTO_ABORTIFHUNG, SendMessageTimeoutW,
};
use windows::core::{HSTRING, PCWSTR};

use crate::ui::{Launch, Page, Section};

pub const DAEMON_WINDOW_CLASS: &str = "OpenTalkDaemon";
pub const SINGLE_INSTANCE_MUTEX: &str = "Local\\OpenTalk.Daemon";

/// WM_APP-based message ids handled by the daemon window.
pub const WM_APP: u32 = 0x8000;
pub const WM_OT_RELOAD: u32 = WM_APP + 1; // config.json changed
pub const WM_OT_SUSPEND_HOTKEYS: u32 = WM_APP + 2; // wparam: 1 suspend, 0 resume (hotkey capture)
pub const WM_OT_OPEN_SETTINGS: u32 = WM_APP + 3; // second instance launched
pub const WM_OT_TOGGLE: u32 = WM_APP + 4; // toggle dictation (bench/automation)
pub const WM_OT_CANCEL: u32 = WM_APP + 5;
pub const WM_OT_RETRY_LAST: u32 = WM_APP + 6;
pub const WM_OT_QUIT: u32 = WM_APP + 7;
/// Start dictating; does nothing unless the daemon is idle (idempotent, unlike [`WM_OT_TOGGLE`]).
pub const WM_OT_START: u32 = WM_APP + 8;
/// Stop the running recording; does nothing unless one is running (idempotent).
pub const WM_OT_STOP: u32 = WM_APP + 9;
/// Synchronous query (`SendMessage`, not posted): the reply is [`DaemonPhase::code`], 0 if the daemon was busy.
pub const WM_OT_QUERY_PHASE: u32 = WM_APP + 10;
/// Last id of the `WM_OT_*` range (the daemon lets lower-integrity senders use all of them).
pub const WM_OT_LAST: u32 = WM_OT_QUERY_PHASE;
/// First id the daemon may use for its own internal messages.
pub const WM_OT_INTERNAL_BASE: u32 = WM_APP + 0x100;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IpcMsg { Reload, SuspendHotkeys(bool), OpenSettings, Toggle, Cancel, RetryLast, Quit, Start, Stop }

impl IpcMsg {
    pub fn encode(self) -> (u32, usize) {
        match self {
            IpcMsg::Reload => (WM_OT_RELOAD, 0),
            IpcMsg::SuspendHotkeys(b) => (WM_OT_SUSPEND_HOTKEYS, b as usize),
            IpcMsg::OpenSettings => (WM_OT_OPEN_SETTINGS, 0),
            IpcMsg::Toggle => (WM_OT_TOGGLE, 0),
            IpcMsg::Cancel => (WM_OT_CANCEL, 0),
            IpcMsg::RetryLast => (WM_OT_RETRY_LAST, 0),
            IpcMsg::Quit => (WM_OT_QUIT, 0),
            IpcMsg::Start => (WM_OT_START, 0),
            IpcMsg::Stop => (WM_OT_STOP, 0),
        }
    }
}

/// What the daemon is doing, as answered to [`WM_OT_QUERY_PHASE`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DaemonPhase { Idle, Arming, Recording, Thinking }

impl DaemonPhase {
    /// Reply code; never 0 (0 means "no answer").
    pub fn code(self) -> usize {
        match self {
            DaemonPhase::Idle => 1,
            DaemonPhase::Arming => 2,
            DaemonPhase::Recording => 3,
            DaemonPhase::Thinking => 4,
        }
    }

    pub fn from_code(code: usize) -> Option<DaemonPhase> {
        Some(match code {
            1 => DaemonPhase::Idle,
            2 => DaemonPhase::Arming,
            3 => DaemonPhase::Recording,
            4 => DaemonPhase::Thinking,
            _ => return None,
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            DaemonPhase::Idle => "idle",
            DaemonPhase::Arming => "arming",
            DaemonPhase::Recording => "recording",
            DaemonPhase::Thinking => "thinking",
        }
    }
}

/// Ask the running daemon what it is doing. `None`: no daemon, no answer within 2 s, or it was
/// momentarily busy (retry). Bench/automation only; the daemon never calls this.
pub fn query_daemon_phase() -> Option<DaemonPhase> {
    query_phase_of(DAEMON_WINDOW_CLASS)
}

fn query_phase_of(class: &str) -> Option<DaemonPhase> {
    let hwnd = find_window(class)?;
    let mut reply = 0usize;
    // SAFETY: SendMessageTimeoutW to a window handle; a stale handle or a hung receiver just
    // makes it fail, and `reply` outlives the call.
    let ok = unsafe { SendMessageTimeoutW(hwnd, WM_OT_QUERY_PHASE, WPARAM(0), LPARAM(0), SMTO_ABORTIFHUNG, 2000, Some(&mut reply)) };
    if ok.0 == 0 { None } else { DaemonPhase::from_code(reply) }
}

/// Name of the registered window message that asks a running Settings window to show a page.
pub const SETTINGS_PAGE_MESSAGE: &str = "OpenTalk.SettingsPage";

/// Message id of [`SETTINGS_PAGE_MESSAGE`] (0 if Windows refuses to register it). Both sides
/// get the same id from the system, so it needs no coordination.
pub fn settings_page_message() -> u32 {
    static ID: OnceLock<u32> = OnceLock::new();
    *ID.get_or_init(|| {
        // SAFETY: the HSTRING outlives the call.
        unsafe { RegisterWindowMessageW(&HSTRING::from(SETTINGS_PAGE_MESSAGE)) }
    })
}

/// Marks a wparam as a page code, so a stray message with wparam 0 is not read as "History".
const PAGE_CODE_VALID: usize = 1 << 16;

/// Compact wparam for a Settings page request: bit 16 = valid, bits 8..16 = section (0 = none,
/// else index in `Section::ALL` + 1), bits 0..8 = page (0 History, 1 Dictionary). Covers
/// everything `ui::parse_page` can return.
pub fn encode_settings_page(launch: Launch) -> usize {
    let page = match launch.page {
        Page::History => 0,
        Page::Dictionary => 1,
    };
    let section = launch.settings.and_then(|s| Section::ALL.iter().position(|x| *x == s)).map_or(0, |i| i + 1);
    PAGE_CODE_VALID | section << 8 | page
}

/// Inverse of [`encode_settings_page`]; `None` for anything that is not a valid code.
pub fn decode_settings_page(wparam: usize) -> Option<Launch> {
    if wparam & !(PAGE_CODE_VALID | 0xFFFF) != 0 || wparam & PAGE_CODE_VALID == 0 {
        return None;
    }
    let page = match wparam & 0xFF {
        0 => Page::History,
        1 => Page::Dictionary,
        _ => return None,
    };
    let settings = match (wparam >> 8) & 0xFF {
        0 => None,
        n => Some(*Section::ALL.get(n - 1)?),
    };
    Some(Launch { page, settings })
}

/// Ask the Settings window `hwnd` to show `launch`. False if the post failed.
pub fn post_settings_page(hwnd: HWND, launch: Launch) -> bool {
    let id = settings_page_message();
    if id == 0 {
        return false;
    }
    // SAFETY: plain PostMessageW; a stale handle just makes the call fail.
    unsafe { PostMessageW(Some(hwnd), id, WPARAM(encode_settings_page(launch)), LPARAM(0)).is_ok() }
}

/// Post `msg` to the running daemon. Returns false if no daemon window exists.
pub fn notify_daemon(msg: IpcMsg) -> bool {
    post_to(DAEMON_WINDOW_CLASS, msg)
}

/// True if a daemon window is currently registered.
pub fn daemon_running() -> bool {
    find_window(DAEMON_WINDOW_CLASS).is_some()
}

fn post_to(class: &str, msg: IpcMsg) -> bool {
    let Some(hwnd) = find_window(class) else { return false };
    let (id, wparam) = msg.encode();
    // SAFETY: plain PostMessageW to a window handle; a stale handle just makes the call fail.
    unsafe { PostMessageW(Some(hwnd), id, WPARAM(wparam), LPARAM(0)).is_ok() }
}

/// Find a top-level window of `class`, falling back to message-only windows
/// (`FindWindowW` never returns those).
fn find_window(class: &str) -> Option<HWND> {
    let class = HSTRING::from(class);
    // SAFETY: the HSTRING outlives both calls; null window title matches any.
    unsafe {
        if let Ok(h) = FindWindowW(&class, PCWSTR::null())
            && !h.is_invalid()
        {
            return Some(h);
        }
        match FindWindowExW(Some(HWND_MESSAGE), None, &class, PCWSTR::null()) {
            Ok(h) if !h.is_invalid() => Some(h),
            _ => None,
        }
    }
}

/// Holds the single-instance mutex; the lock is released when dropped (or the process exits).
pub struct SingleInstanceGuard(HANDLE);

// SAFETY: a mutex kernel handle is valid on any thread; we only ever close it.
unsafe impl Send for SingleInstanceGuard {}
unsafe impl Sync for SingleInstanceGuard {}

impl Drop for SingleInstanceGuard {
    fn drop(&mut self) {
        // SAFETY: the handle was returned by CreateMutexW and is closed exactly once.
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

/// Take the process-wide single-instance lock. `None` means another daemon already holds it.
/// Access denied means a daemon at a higher integrity level (elevated) holds it, so that also
/// counts as taken. Any other failure to create it fails open with a guard that owns nothing,
/// so a broken environment never locks the user out of the app.
pub fn acquire_single_instance() -> Option<SingleInstanceGuard> {
    acquire_named(SINGLE_INSTANCE_MUTEX)
}

fn acquire_named(name: &str) -> Option<SingleInstanceGuard> {
    // SAFETY: the HSTRING outlives the call; GetLastError is read immediately after, before
    // any other Win32 call can overwrite it.
    unsafe {
        match CreateMutexW(None, false, &HSTRING::from(name)) {
            Ok(h) => {
                if GetLastError() == ERROR_ALREADY_EXISTS {
                    let _ = CloseHandle(h);
                    None
                } else {
                    Some(SingleInstanceGuard(h))
                }
            }
            Err(e) if e.code() == ERROR_ACCESS_DENIED.to_hresult() => None,
            Err(_) => Some(SingleInstanceGuard(HANDLE::default())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Foundation::LRESULT;
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DestroyWindow, MSG, PM_REMOVE, PeekMessageW, RegisterClassW,
        UnregisterClassW, WINDOW_EX_STYLE, WNDCLASSW, WS_POPUP,
    };

    #[test]
    fn encode_ids_and_wparam() {
        assert_eq!(IpcMsg::Reload.encode(), (WM_OT_RELOAD, 0));
        assert_eq!(IpcMsg::SuspendHotkeys(true).encode(), (WM_OT_SUSPEND_HOTKEYS, 1));
        assert_eq!(IpcMsg::SuspendHotkeys(false).encode(), (WM_OT_SUSPEND_HOTKEYS, 0));
        assert_eq!(IpcMsg::OpenSettings.encode().0, WM_APP + 3);
        assert_eq!(IpcMsg::Toggle.encode().0, WM_APP + 4);
        assert_eq!(IpcMsg::Cancel.encode().0, WM_APP + 5);
        assert_eq!(IpcMsg::RetryLast.encode().0, WM_APP + 6);
        assert_eq!(IpcMsg::Quit.encode().0, WM_APP + 7);
        assert_eq!(IpcMsg::Start.encode(), (WM_OT_START, 0));
        assert_eq!(IpcMsg::Stop.encode(), (WM_OT_STOP, 0));
        assert_eq!((WM_OT_START, WM_OT_STOP, WM_OT_QUERY_PHASE), (WM_APP + 8, WM_APP + 9, WM_APP + 10));
        assert!(WM_OT_LAST < WM_OT_INTERNAL_BASE);
    }

    #[test]
    fn phase_codes_round_trip_and_zero_is_no_answer() {
        for p in [DaemonPhase::Idle, DaemonPhase::Arming, DaemonPhase::Recording, DaemonPhase::Thinking] {
            assert_ne!(p.code(), 0);
            assert_eq!(DaemonPhase::from_code(p.code()), Some(p));
        }
        assert_eq!(DaemonPhase::from_code(0), None);
        assert_eq!(DaemonPhase::from_code(5), None);
        assert_eq!(query_phase_of("OpenTalkTestNoSuchClass"), None);
    }

    #[test]
    fn settings_page_code_round_trips_every_page() {
        let mut all = vec![];
        for page in [Page::History, Page::Dictionary] {
            all.push(Launch { page, settings: None });
            for s in Section::ALL {
                all.push(Launch { page, settings: Some(s) });
            }
        }
        let mut codes = std::collections::HashSet::new();
        for l in all {
            let c = encode_settings_page(l);
            assert_eq!(decode_settings_page(c), Some(l), "{l:?}");
            assert!(codes.insert(c), "duplicate code for {l:?}");
        }
    }

    #[test]
    fn settings_page_code_covers_everything_parse_page_accepts() {
        use crate::ui::parse_page;
        let mut inputs = vec!["", "history", "home", "dictionary", "snippets", "dict", "settings", "garbage", "--page=dictionary", "settings:bogus"];
        let sections = ["preferences", "prefs", "general", "hotkeys", "hotkey", "speech-to-text", "stt", "speechtotext", "transcription", "cleanup", "intelligence", "llm", "privacy", "privacy-data", "privacydata", "system"];
        let owned: Vec<String> = sections.iter().flat_map(|s| [format!("settings:{s}"), format!("settings/{s}"), s.to_string()]).collect();
        inputs.extend(owned.iter().map(String::as_str));
        for i in inputs {
            let l = parse_page(Some(i));
            assert_eq!(decode_settings_page(encode_settings_page(l)), Some(l), "{i}");
        }
    }

    #[test]
    fn settings_page_code_rejects_junk() {
        assert_eq!(decode_settings_page(0), None, "no valid bit");
        assert_eq!(decode_settings_page(1), None);
        assert_eq!(decode_settings_page(PAGE_CODE_VALID | 2), None, "unknown page");
        assert_eq!(decode_settings_page(PAGE_CODE_VALID | (Section::ALL.len() + 1) << 8), None, "unknown section");
        assert_eq!(decode_settings_page(PAGE_CODE_VALID | 1 << 24), None, "stray high bits");
        assert_eq!(decode_settings_page(usize::MAX), None);
    }

    #[test]
    fn settings_page_message_is_a_registered_id() {
        let id = settings_page_message();
        assert!((0xC000..=0xFFFF).contains(&id), "{id:#x}");
        assert_eq!(id, settings_page_message());
    }

    #[test]
    #[ignore = "desktop: creates a hidden window"]
    fn desktop_settings_page_post_reaches_window() {
        // SAFETY: STATIC is a system class; the window is destroyed below on this thread.
        let hwnd = unsafe {
            CreateWindowExW(WINDOW_EX_STYLE(0), windows::core::w!("STATIC"), None, WS_POPUP, 0, 0, 0, 0, Some(HWND_MESSAGE), None, None, None)
        }
        .expect("message window");
        let want = Launch { page: Page::Dictionary, settings: Some(Section::Hotkeys) };
        assert!(post_settings_page(hwnd, want));
        let mut m = MSG::default();
        // SAFETY: plain message peek on this thread's queue.
        let got = unsafe { PeekMessageW(&mut m, Some(hwnd), settings_page_message(), settings_page_message(), PM_REMOVE) }.as_bool();
        // SAFETY: our own window.
        unsafe {
            let _ = DestroyWindow(hwnd);
        }
        assert!(got);
        assert_eq!(decode_settings_page(m.wParam.0), Some(want));
    }

    #[test]
    fn no_window_means_not_running() {
        assert!(find_window("OpenTalkTestNoSuchClass").is_none());
        assert!(!post_to("OpenTalkTestNoSuchClass", IpcMsg::Reload));
    }

    #[test]
    fn single_instance_mutex_excludes_second_holder() {
        let name = format!(r"Local\OpenTalk.Test.{}", std::process::id());
        let first = acquire_named(&name).expect("first acquire");
        assert!(acquire_named(&name).is_none(), "second acquire must fail while held");
        drop(first);
        let again = acquire_named(&name);
        assert!(again.is_some(), "released on drop");
    }

    unsafe extern "system" fn proc(h: HWND, m: u32, w: WPARAM, l: LPARAM) -> LRESULT {
        // SAFETY: forwarding to the default procedure with the same arguments.
        unsafe { DefWindowProcW(h, m, w, l) }
    }

    /// Create a window of a unique class, post to it by class name, receive it.
    fn roundtrip(message_only: bool) {
        let class = format!("OpenTalkTest{}{}", if message_only { "Msg" } else { "Top" }, std::process::id());
        let class_w = HSTRING::from(class.as_str());
        // SAFETY: standard window class registration / creation on this thread; the window is
        // destroyed and the class unregistered before returning.
        unsafe {
            let inst = GetModuleHandleW(PCWSTR::null()).unwrap();
            let wc = WNDCLASSW {
                lpfnWndProc: Some(proc),
                hInstance: inst.into(),
                lpszClassName: PCWSTR(class_w.as_ptr()),
                ..Default::default()
            };
            assert_ne!(RegisterClassW(&wc), 0);
            let parent = message_only.then_some(HWND_MESSAGE);
            let hwnd = CreateWindowExW(
                WINDOW_EX_STYLE(0), &class_w, PCWSTR::null(), WS_POPUP, 0, 0, 0, 0, parent, None, Some(inst.into()), None,
            )
            .unwrap();

            assert!(find_window(&class).is_some());
            assert!(post_to(&class, IpcMsg::SuspendHotkeys(true)));
            assert!(post_to(&class, IpcMsg::Quit));

            let mut m = MSG::default();
            assert!(PeekMessageW(&mut m, Some(hwnd), WM_APP, WM_APP + 0x100, PM_REMOVE).as_bool());
            assert_eq!((m.message, m.wParam.0), (WM_OT_SUSPEND_HOTKEYS, 1));
            assert!(PeekMessageW(&mut m, Some(hwnd), WM_APP, WM_APP + 0x100, PM_REMOVE).as_bool());
            assert_eq!(m.message, WM_OT_QUIT);

            let _ = DestroyWindow(hwnd);
            let _ = UnregisterClassW(&class_w, Some(inst.into()));
        }
        assert!(find_window(&class).is_none());
    }

    #[test]
    #[ignore = "desktop: creates a hidden window"]
    fn desktop_post_reaches_top_level_window() {
        roundtrip(false);
    }

    #[test]
    #[ignore = "desktop: creates a message-only window"]
    fn desktop_post_reaches_message_only_window() {
        roundtrip(true);
    }
}
