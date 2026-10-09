//! Daemon <-> settings-process signalling via window messages to the daemon's hidden
//! message window (class `DAEMON_WINDOW_CLASS`). Config/history live in files; messages only notify.
//! TASK core-data implements `notify_daemon` (FindWindowW + PostMessageW) and `daemon_running`.
use windows::Win32::Foundation::{CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE, HWND, LPARAM, WPARAM};
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::WindowsAndMessaging::{FindWindowExW, FindWindowW, HWND_MESSAGE, PostMessageW};
use windows::core::{HSTRING, PCWSTR};


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
/// First id the daemon may use for its own internal messages.
pub const WM_OT_INTERNAL_BASE: u32 = WM_APP + 0x100;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IpcMsg { Reload, SuspendHotkeys(bool), OpenSettings, Toggle, Cancel, RetryLast, Quit }

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
        }
    }
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
/// If the mutex cannot be created at all (should not happen) we fail open and return a guard
/// that owns nothing, so a broken environment never locks the user out of the app.
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
        assert!(WM_OT_QUIT < WM_OT_INTERNAL_BASE);
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
