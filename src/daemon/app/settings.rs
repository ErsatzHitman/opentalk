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

use crate::{log_info, log_warn};

/// Pages understood by `ui::run` (`--settings <page>`).
pub const PAGE_HISTORY: &str = "history";
pub const PAGE_DICTIONARY: &str = "dictionary";
pub const PAGE_SETTINGS: &str = "settings";
pub const PAGE_STT: &str = "settings:stt";

#[derive(Default)]
pub struct SettingsProc {
    child: Option<Child>,
}

impl SettingsProc {
    /// Open `page`, or focus the window that is already open.
    pub fn open(&mut self, page: &str) {
        if self.is_open() {
            if let Some(child) = &self.child {
                focus_process_window(child.id());
            }
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
    pub fn pid(&mut self) -> Option<u32> {
        self.is_open().then(|| self.child.as_ref().map(Child::id)).flatten()
    }

    /// Ask the child to go away (daemon quit with the window open is left alone; this is for tests).
    pub fn kill(&mut self) {
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

struct Search {
    pid: u32,
    found: Option<HWND>,
}

/// Bring the first visible top-level window of `pid` to the foreground.
fn focus_process_window(pid: u32) {
    let mut search = Search { pid, found: None };
    // SAFETY: `search` outlives the synchronous EnumWindows call; the callback only reads window
    // properties and writes through the pointer it was given.
    unsafe {
        let _ = EnumWindows(Some(enum_proc), LPARAM(&mut search as *mut Search as isize));
        if let Some(hwnd) = search.found {
            if IsIconic(hwnd).as_bool() {
                let _ = ShowWindow(hwnd, SW_RESTORE);
            }
            let _ = SetForegroundWindow(hwnd);
        }
    }
}

unsafe extern "system" fn enum_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
    // SAFETY: lparam is the `&mut Search` passed by `focus_process_window`.
    let search = unsafe { &mut *(lparam.0 as *mut Search) };
    let mut pid = 0u32;
    // SAFETY: valid out pointer.
    unsafe {
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        if pid == search.pid && IsWindowVisible(hwnd).as_bool() {
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
    fn pages_match_the_ui_parser() {
        use crate::ui::{Page, parse_page};
        assert_eq!(parse_page(Some(PAGE_HISTORY)).page, Page::History);
        assert_eq!(parse_page(Some(PAGE_DICTIONARY)).page, Page::Dictionary);
        assert!(parse_page(Some(PAGE_SETTINGS)).settings.is_some());
        assert!(parse_page(Some(PAGE_STT)).settings.is_some());
    }
}
