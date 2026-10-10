//! Signalling to a running daemon (a second launch, `--settings [page]`, `--bench`) via window
//! messages to its hidden window (class `DAEMON_WINDOW_CLASS`). Config/history live in files;
//! messages only notify.
use windows::Win32::Foundation::{CloseHandle, ERROR_ACCESS_DENIED, ERROR_ALREADY_EXISTS, GetLastError, HANDLE, HWND, LPARAM, WPARAM};
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::WindowsAndMessaging::{
    AllowSetForegroundWindow, FindWindowExW, FindWindowW, GetWindowThreadProcessId, HWND_MESSAGE, PostMessageW,
    SMTO_ABORTIFHUNG, SendMessageTimeoutW,
};
use windows::core::{HSTRING, PCWSTR};

use crate::core::nav::Route;

pub const DAEMON_WINDOW_CLASS: &str = "OpenTalkDaemon";
pub const SINGLE_INSTANCE_MUTEX: &str = "Local\\OpenTalk.Daemon";

/// WM_APP-based message ids handled by the daemon window.
pub const WM_APP: u32 = 0x8000;
pub const WM_OT_RELOAD: u32 = WM_APP + 1; // config.json changed
/// Sent by the old Settings window around its hotkey capture; ignored (the popup captures in the
/// daemon). The id stays reserved.
pub const WM_OT_SUSPEND_HOTKEYS: u32 = WM_APP + 2;
/// An older build's second launch: opens the Home popup (same as `OpenPage(Home)`).
pub const WM_OT_OPEN_SETTINGS: u32 = WM_APP + 3;
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
/// Open the tray popup at a page. `wparam` is [`Route::encode`]; a stray or undecodable value
/// opens Home. Sent by `opentalk --settings [page]` and by a second launch.
pub const WM_OT_OPEN_PAGE: u32 = WM_APP + 11;
/// Last id of the `WM_OT_*` range (the daemon lets lower-integrity senders use all of them).
pub const WM_OT_LAST: u32 = WM_OT_OPEN_PAGE;
/// First id the daemon may use for its own internal messages.
pub const WM_OT_INTERNAL_BASE: u32 = WM_APP + 0x100;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IpcMsg {
    /// Re-read `config.json` (after an external edit).
    Reload,
    /// Open the tray popup at this page.
    OpenPage(Route),
    Toggle,
    Cancel,
    RetryLast,
    Quit,
    Start,
    Stop,
}

impl IpcMsg {
    pub fn encode(self) -> (u32, usize) {
        match self {
            IpcMsg::Reload => (WM_OT_RELOAD, 0),
            IpcMsg::OpenPage(r) => (WM_OT_OPEN_PAGE, r.encode()),
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
    if matches!(msg, IpcMsg::OpenPage(_)) {
        allow_foreground(hwnd);
    }
    let (id, wparam) = msg.encode();
    // SAFETY: plain PostMessageW to a window handle; a stale handle just makes the call fail.
    unsafe { PostMessageW(Some(hwnd), id, WPARAM(wparam), LPARAM(0)).is_ok() }
}

/// Let the process that owns `hwnd` take the foreground: the popup it is about to open for us
/// needs the keyboard, and Windows only grants that to the process the user is working with (or
/// one it delegates to). Harmless when this process has no foreground right to give.
fn allow_foreground(hwnd: HWND) {
    let mut pid = 0u32;
    // SAFETY: `pid` outlives the call; a stale handle just leaves it 0.
    unsafe {
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        if pid != 0 {
            let _ = AllowSetForegroundWindow(pid);
        }
    }
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
        // Ids an older build may still send keep their numbers (Settings-process hotkey suspend,
        // open Home).
        assert_eq!((WM_OT_SUSPEND_HOTKEYS, WM_OT_OPEN_SETTINGS), (WM_APP + 2, WM_APP + 3));
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
    fn open_page_carries_the_encoded_route() {
        use crate::core::nav::{PickerId, Section};
        assert_eq!(WM_OT_OPEN_PAGE, WM_APP + 11);
        assert_eq!(WM_OT_LAST, WM_OT_OPEN_PAGE, "the lower-integrity message filter covers it");
        let routes = [Route::Home, Route::History, Route::HistoryEntry(7), Route::Dictionary, Route::SettingsIndex, Route::Settings(Section::SpeechToText), Route::Picker(PickerId::Language)];
        for r in routes {
            let (id, w) = IpcMsg::OpenPage(r).encode();
            assert_eq!(id, WM_OT_OPEN_PAGE);
            assert_eq!(Route::decode(w), Some(r), "{r:?}");
        }
        assert_eq!(Route::decode(0), None, "a stray message is not a route");
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
            assert!(post_to(&class, IpcMsg::Reload));
            assert!(post_to(&class, IpcMsg::OpenPage(Route::Settings(crate::core::nav::Section::SpeechToText))));
            assert!(post_to(&class, IpcMsg::Quit));

            let mut m = MSG::default();
            assert!(PeekMessageW(&mut m, Some(hwnd), WM_APP, WM_APP + 0x100, PM_REMOVE).as_bool());
            assert_eq!((m.message, m.wParam.0), (WM_OT_RELOAD, 0));
            assert!(PeekMessageW(&mut m, Some(hwnd), WM_APP, WM_APP + 0x100, PM_REMOVE).as_bool());
            assert_eq!(m.message, WM_OT_OPEN_PAGE);
            assert_eq!(Route::decode(m.wParam.0), Some(Route::Settings(crate::core::nav::Section::SpeechToText)));
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
