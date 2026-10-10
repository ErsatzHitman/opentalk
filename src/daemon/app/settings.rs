//! The Settings window lives in a child process (`opentalk.exe --settings <page>`) so the egui/GL
//! stack is only resident while the window is open. At most one child runs; opening again
//! just brings its window to the front.
use std::process::{Child, Command, Stdio};

use windows::Win32::Foundation::{HWND, LPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    AllowSetForegroundWindow, EnumWindows, GetWindowThreadProcessId, IsIconic, IsWindowVisible, SW_RESTORE,
    SetForegroundWindow, ShowWindow,
};
use windows::core::BOOL;

use crate::core::ipc;
use crate::{log_info, log_warn};

/// Pages understood by `ui::run` (`--settings <page>`).
pub const PAGE_HISTORY: &str = "history";
pub const PAGE_DICTIONARY: &str = "dictionary";
#[cfg(test)]
pub const PAGE_SETTINGS: &str = "settings";
pub const PAGE_STT: &str = "settings:stt";

#[derive(Default)]
pub struct SettingsProc {
    child: Option<Child>,
    /// Window + message posted when the child exits (0 = nobody to tell).
    notify: (isize, u32),
}

impl SettingsProc {
    /// `msg` is posted to `hwnd` when a Settings process this spawned exits.
    pub fn new(hwnd: isize, msg: u32) -> SettingsProc {
        SettingsProc { child: None, notify: (hwnd, msg) }
    }

    /// Open `page`, or switch the window that is already open to it and bring it forward.
    pub fn open(&mut self, page: &str) {
        if self.is_open() {
            if let Some(child) = &self.child
                && let Some(hwnd) = search(child.id())
            {
                show_page_in(hwnd, page);
            }
            return;
        }
        // A Settings window started some other way (`opentalk --settings` from a shell).
        if let Some(hwnd) = find_foreign_settings_window() {
            log_info!("settings: already open in another process; showing '{page}' there");
            show_page_in(hwnd, page);
            return;
        }
        let exe = match std::env::current_exe() {
            Ok(e) => e,
            Err(e) => {
                log_warn!("settings: cannot locate the executable: {e}");
                return;
            }
        };
        match Command::new(exe).arg("--settings").arg(page).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn() {
            Ok(child) => {
                log_info!("settings: opened '{page}' (pid {})", child.id());
                // SAFETY: plain call; lets the new process take the foreground from us.
                unsafe {
                    let _ = AllowSetForegroundWindow(child.id());
                }
                watch_exit(child.id(), self.notify);
                self.child = Some(child);
            }
            Err(e) => log_warn!("settings: cannot start the settings window: {e}"),
        }
    }

    /// True while the child process is alive (reaps it when it has exited).
    pub fn is_open(&mut self) -> bool {
        match self.child.as_mut().map(Child::try_wait) {
            Some(Ok(None)) => true,
            Some(_) => {
                self.child = None;
                false
            }
            None => false,
        }
    }

    /// Process id of the open settings window, if any.
    #[cfg(test)]
    pub fn pid(&mut self) -> Option<u32> {
        self.is_open().then(|| self.child.as_ref().map(Child::id)).flatten()
    }

    /// Ask the child to go away (tests only; a daemon quit leaves an open window alone).
    #[cfg(test)]
    pub fn kill(&mut self) {
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

/// Tell the Settings window `hwnd` to show `page` (event-driven: a registered window message the
/// UI process receives in its message pump), then bring it to the foreground.
fn show_page_in(hwnd: HWND, page: &str) {
    if !ipc::post_settings_page(hwnd, crate::ui::parse_page(Some(page))) {
        log_warn!("settings: could not deliver page '{page}' to the open window");
    }
    focus_window(hwnd);
}

/// Post `notify.1` to `notify.0` once process `pid` exits: one thread blocked in
/// `WaitForSingleObject` (no polling) that ends with the process.
fn watch_exit(pid: u32, notify: (isize, u32)) {
    use windows::Win32::Foundation::{CloseHandle, WPARAM};
    use windows::Win32::System::Threading::{INFINITE, OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject};
    use windows::Win32::UI::WindowsAndMessaging::PostMessageW;
    if notify.0 == 0 {
        return;
    }
    // SAFETY: opens a synchronize-only handle that the watcher thread closes.
    let handle = match unsafe { OpenProcess(PROCESS_SYNCHRONIZE, false, pid) } {
        Ok(h) => h.0 as isize,
        Err(e) => {
            log_warn!("settings: cannot watch pid {pid}: {e}");
            return;
        }
    };
    let spawned = std::thread::Builder::new().name("settings-watch".into()).stack_size(64 * 1024).spawn(move || {
        let h = windows::Win32::Foundation::HANDLE(handle as *mut core::ffi::c_void);
        // SAFETY: `h` is the handle opened above, owned by this thread and closed exactly once.
        unsafe {
            WaitForSingleObject(h, INFINITE);
            let _ = CloseHandle(h);
            let _ = PostMessageW(Some(HWND(notify.0 as *mut core::ffi::c_void)), notify.1, WPARAM(0), LPARAM(0));
        }
    });
    if let Err(e) = spawned {
        log_warn!("settings: cannot start the exit watcher: {e}");
        // SAFETY: the thread did not start, so the handle is still ours to close.
        unsafe {
            let _ = windows::Win32::Foundation::CloseHandle(windows::Win32::Foundation::HANDLE(handle as *mut core::ffi::c_void));
        }
    }
}

/// Title of the egui control panel window (`ui::app`).
const SETTINGS_TITLE: &str = "OpenTalk";

struct Search {
    /// Match windows of this process, or (0) visible "OpenTalk" windows of another process
    /// running this executable.
    pid: u32,
    found: Option<HWND>,
}

fn focus_window(hwnd: HWND) {
    // SAFETY: plain window calls on a handle that may have gone away (they then just fail).
    unsafe {
        if IsIconic(hwnd).as_bool() {
            let _ = ShowWindow(hwnd, SW_RESTORE);
        }
        let _ = SetForegroundWindow(hwnd);
    }
}

fn search(pid: u32) -> Option<HWND> {
    let mut search = Search { pid, found: None };
    // SAFETY: `search` outlives the synchronous EnumWindows call; the callback only reads window
    // properties and writes through the pointer it was given.
    unsafe {
        let _ = EnumWindows(Some(enum_proc), LPARAM(&mut search as *mut Search as isize));
    }
    search.found
}

/// A visible Settings window of another process running this same executable.
fn find_foreign_settings_window() -> Option<HWND> {
    search(0)
}

/// Executable file name of `pid`, lowercase.
fn exe_name_of(pid: u32) -> Option<String> {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW};
    // SAFETY: query-only handle, closed before returning; the buffer outlives the call.
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, windows::core::PWSTR(buf.as_mut_ptr()), &mut len).is_ok();
        let _ = CloseHandle(h);
        ok.then(|| String::from_utf16_lossy(&buf[..len as usize]).rsplit('\\').next().unwrap_or_default().to_lowercase())
    }
}

fn our_exe_name() -> Option<String> {
    std::env::current_exe().ok()?.file_name().map(|n| n.to_string_lossy().to_lowercase())
}

unsafe extern "system" fn enum_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
    use windows::Win32::UI::WindowsAndMessaging::GetWindowTextW;
    // SAFETY: lparam is the `&mut Search` passed by `search`.
    let search = unsafe { &mut *(lparam.0 as *mut Search) };
    let mut pid = 0u32;
    // SAFETY: valid out pointer; plain window queries.
    unsafe {
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        if !IsWindowVisible(hwnd).as_bool() {
            return BOOL(1);
        }
        let hit = if search.pid != 0 {
            pid == search.pid
        } else if pid == std::process::id() {
            false
        } else {
            let mut title = [0u16; 32];
            let n = GetWindowTextW(hwnd, &mut title).max(0) as usize;
            String::from_utf16_lossy(&title[..n]) == SETTINGS_TITLE && exe_name_of(pid).is_some_and(|e| Some(e) == our_exe_name())
        };
        if hit {
            search.found = Some(hwnd);
            return BOOL(0);
        }
    }
    BOOL(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_child_means_closed() {
        let mut s = SettingsProc::default();
        assert!(!s.is_open());
        assert_eq!(s.pid(), None);
    }

    #[test]
    fn exited_child_is_reaped() {
        let mut s = SettingsProc::default();
        let child = Command::new("cmd").args(["/C", "exit 0"]).spawn().expect("spawn cmd");
        s.child = Some(child);
        for _ in 0..100 {
            if !s.is_open() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(!s.is_open());
        assert!(s.child.is_none());
    }

    #[test]
    fn exit_watcher_posts_when_the_process_ends() {
        use windows::Win32::UI::WindowsAndMessaging::{
            CreateWindowExW, DestroyWindow, HWND_MESSAGE, MSG, PM_REMOVE, PeekMessageW, WINDOW_EX_STYLE, WM_APP, WS_POPUP,
        };
        // A message-only window on this thread stands in for the daemon window.
        // SAFETY: STATIC is a system class; the window is destroyed below on this thread.
        let hwnd = unsafe {
            CreateWindowExW(WINDOW_EX_STYLE(0), windows::core::w!("STATIC"), None, WS_POPUP, 0, 0, 0, 0, Some(HWND_MESSAGE), None, None, None)
        }
        .expect("message window");
        let child = Command::new("cmd").args(["/C", "exit 0"]).spawn().expect("spawn cmd");
        watch_exit(child.id(), (hwnd.0 as isize, WM_APP + 0x55));
        let t0 = std::time::Instant::now();
        let mut got = false;
        while t0.elapsed() < std::time::Duration::from_secs(5) && !got {
            let mut msg = MSG::default();
            // SAFETY: plain message peek on this thread's queue.
            while unsafe { PeekMessageW(&mut msg, Some(hwnd), 0, 0, PM_REMOVE) }.as_bool() {
                got |= msg.message == WM_APP + 0x55;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        // SAFETY: our own window.
        unsafe {
            let _ = DestroyWindow(hwnd);
        }
        assert!(got, "no exit notification");
    }

    #[test]
    fn pages_match_the_ui_parser() {
        use crate::ui::{Page, parse_page};
        assert_eq!(parse_page(Some(PAGE_HISTORY)).page, Page::History);
        assert_eq!(parse_page(Some(PAGE_DICTIONARY)).page, Page::Dictionary);
        assert!(parse_page(Some(PAGE_SETTINGS)).settings.is_some());
        assert!(parse_page(Some(PAGE_STT)).settings.is_some());
    }
}
