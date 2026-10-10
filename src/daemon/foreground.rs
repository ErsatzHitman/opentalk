//! Foreground-app tracking for the app bubble: the icon is resolved *before* the hotkey is pressed
//! and follows the app the user switches to while dictating.
//!
//! - **Event, not poll.** One out-of-context `SetWinEventHook(EVENT_SYSTEM_FOREGROUND)` (no DLL is
//!   injected; the callback runs on the main thread inside its message loop) forwards the new
//!   window's handle down a channel. Between focus changes nothing runs: no timer, no polling.
//! - **One worker.** A long-lived, low-priority thread blocks on that channel. For each new
//!   foreground window it skips shell surfaces ([`is_ignored`]), describes the window, resolves the
//!   app's icon into the small LRU in `appicon` (so the hotkey press finds it ready), publishes the
//!   window as [`last`] and tells the UI thread, which retargets a running dictation.
//! - **Installed while `show_app_bubble` is on** and removed (hook unhooked, worker stopped) when it
//!   is turned off. Keeping it for the daemon's lifetime is what makes the very first press instant;
//!   the cost is one `PostMessage`-sized callback per focus change (a human-paced event).
//!
//! The paste-target rule lives here too, as pure functions: [`resolve`] and [`Follow`].
use std::cell::RefCell;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Mutex, MutexGuard};

use windows::Win32::Foundation::HWND;
use windows::Win32::System::Threading::{
    GetCurrentProcessId, GetCurrentThread, SetThreadPriority, THREAD_MODE_BACKGROUND_BEGIN, THREAD_PRIORITY_LOWEST,
};
use windows::Win32::UI::Accessibility::{HWINEVENTHOOK, SetWinEventHook, UnhookWinEvent};
use windows::Win32::UI::WindowsAndMessaging::{
    EVENT_SYSTEM_FOREGROUND, GetForegroundWindow, OBJID_WINDOW, WINEVENT_OUTOFCONTEXT, WINEVENT_SKIPOWNPROCESS,
};

use super::appicon;
use super::paste::{self, Target};

// ---------------------------------------------------------------------------------------------
// The rule: which windows can be a paste target
// ---------------------------------------------------------------------------------------------

/// Window classes of shell surfaces that take the foreground for a moment while the user is on
/// their way to an app: the taskbar and its tray overflow flyouts, the Alt-Tab / Task View
/// switchers, the input-method switcher. (`explorer.exe`'s real windows, class `CabinetWClass`,
/// are ordinary targets, so this is by class and not by process.)
const TRANSIENT_CLASSES: &[&str] = &[
    "Shell_TrayWnd",
    "Shell_SecondaryTrayWnd",
    "TopLevelWindowForOverflowXamlIsland",
    "NotifyIconOverflowWindow",
    "XamlExplorerHostIslandWindow",
    "MultitaskingViewFrame",
    "TaskSwitcherWnd",
    "TaskSwitcherOverlayWnd",
    "ForegroundStaging",
    "Shell_InputSwitchTopLevelWindow",
    "DV2ControlHost",
    "Shell_LightDismissOverlay",
];

/// Processes that only host shell flyouts (Start, Search, the Calendar / Quick Settings / Action
/// Center panes, the emoji panel). Lower-case file names.
const TRANSIENT_EXES: &[&str] = &["startmenuexperiencehost.exe", "searchhost.exe", "searchapp.exe", "shellexperiencehost.exe", "textinputhost.exe"];

/// A transient shell surface (never a paste target; the previous target stays).
pub fn is_transient(class_name: &str, exe_name: &str) -> bool {
    TRANSIENT_CLASSES.iter().any(|c| c.eq_ignore_ascii_case(class_name)) || TRANSIENT_EXES.iter().any(|e| e.eq_ignore_ascii_case(exe_name))
}

/// Can `t` not be the target? The daemon's own windows (the pill, the tray menu: `own_pid`) and
/// transient shell surfaces. The Settings window is a separate OpenTalk process and stays a real
/// target: dictating into the Dictionary page is a legitimate use, and the bubble then says so.
pub fn is_ignored(t: &Target, own_pid: u32) -> bool {
    (own_pid != 0 && t.pid == own_pid) || is_transient(&t.class_name, &t.exe_name)
}

/// The target for `fresh` (the window in front right now) given `last` (the last valid foreground
/// window the tracker saw): the front window unless it is ignorable, in which case the previous
/// target stays. `None` only when there has never been a valid one.
pub fn resolve(fresh: Option<Target>, last: Option<&Target>, own_pid: u32) -> Option<Target> {
    match fresh {
        Some(t) if !is_ignored(&t, own_pid) => Some(t),
        _ => last.cloned(),
    }
}

/// Identity of an app for the bubble (what its icon is keyed by): the executable, or the process
/// when the path is unreadable.
fn app_key(t: &Target) -> String {
    if t.exe_path.is_empty() { format!("pid:{}", t.pid) } else { t.exe_path.to_ascii_lowercase() }
}

/// What a newly seen foreground window means for the dictation in progress.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Change {
    /// The window already is the target.
    Same,
    /// Another window of the same app: the paste target moves, the bubble does not.
    Window,
    /// Another app: the paste target moves and the bubble swaps icon and name.
    App,
}

/// The target a running dictation currently points at, i.e. what the bubble shows.
#[derive(Debug, Default)]
pub struct Follow {
    shown: Option<Target>,
    active: bool,
}

impl Follow {
    /// A dictation starts pointing at `target`.
    pub fn begin(&mut self, target: Option<Target>) {
        self.shown = target;
        self.active = true;
    }

    /// The dictation is over.
    pub fn end(&mut self) {
        self.shown = None;
        self.active = false;
    }

    #[cfg(test)]
    pub fn active(&self) -> bool {
        self.active
    }

    /// Is the bubble currently for the app at `exe_path`?
    pub fn shows(&self, exe_path: &str) -> bool {
        self.shown.as_ref().is_some_and(|t| t.exe_path.eq_ignore_ascii_case(exe_path))
    }

    /// Fold in a valid (non-ignorable) foreground window.
    pub fn observe(&mut self, seen: &Target) -> Change {
        if !self.active {
            return Change::Same;
        }
        let change = match &self.shown {
            Some(cur) if cur.hwnd == seen.hwnd => Change::Same,
            Some(cur) if app_key(cur) == app_key(seen) => Change::Window,
            _ => Change::App,
        };
        if change != Change::Same {
            self.shown = Some(seen.clone());
        }
        change
    }
}

thread_local! {
    /// The UI thread's view of the running dictation.
    static FOLLOW: RefCell<Follow> = RefCell::new(Follow::default());
}

/// A dictation (or a retry) starts pointing at `target` (UI thread).
pub fn follow_begin(target: Option<Target>) {
    FOLLOW.with(|f| f.borrow_mut().begin(target));
}

/// The dictation ended (UI thread).
pub fn follow_end() {
    FOLLOW.with(|f| f.borrow_mut().end());
}

/// Is the bubble currently for the app at `exe_path` (UI thread)?
pub fn follow_shows(exe_path: &str) -> bool {
    FOLLOW.with(|f| f.borrow().shows(exe_path))
}

/// A valid foreground window was seen (UI thread): what changed for the running dictation.
pub fn follow_observe(seen: &Target) -> Change {
    FOLLOW.with(|f| f.borrow_mut().observe(seen))
}

pub fn own_pid() -> u32 {
    // SAFETY: trivially safe query.
    unsafe { GetCurrentProcessId() }
}

// ---------------------------------------------------------------------------------------------
// The tracker: hook + worker
// ---------------------------------------------------------------------------------------------

/// The last valid foreground window the worker described.
static LAST: Mutex<Option<Target>> = Mutex::new(None);

fn lock_last() -> MutexGuard<'static, Option<Target>> {
    LAST.lock().unwrap_or_else(|e| e.into_inner())
}

/// The last valid (non-ignorable) foreground window, if the tracker is running and has seen one.
pub fn last() -> Option<Target> {
    lock_last().clone()
}

struct Tracker {
    hook: HWINEVENTHOOK,
}

impl Drop for Tracker {
    fn drop(&mut self) {
        HOOK_TX.with(|t| *t.borrow_mut() = None);
        // SAFETY: a hook this thread installed and has not removed yet.
        unsafe {
            let _ = UnhookWinEvent(self.hook);
        }
        // Dropping the sender (above) ends the worker's `recv`; it exits on its own.
        *lock_last() = None;
    }
}

thread_local! {
    static TRACKER: RefCell<Option<Tracker>> = const { RefCell::new(None) };
    /// The hook callback's way to the worker (the callback runs on the installing thread).
    static HOOK_TX: RefCell<Option<Sender<isize>>> = const { RefCell::new(None) };
}

/// Out-of-context hook callback: just hands the window to the worker (it runs inside the main
/// thread's message loop, so it must stay trivial).
unsafe extern "system" fn on_foreground(_hook: HWINEVENTHOOK, _event: u32, hwnd: HWND, id_object: i32, _id_child: i32, _thread: u32, _time: u32) {
    if id_object != OBJID_WINDOW.0 || hwnd.is_invalid() {
        return;
    }
    HOOK_TX.with(|t| {
        if let Some(tx) = t.borrow().as_ref() {
            let _ = tx.send(hwnd.0 as isize);
        }
    });
}

/// Start or stop tracking to match `enabled` (UI thread; idempotent). `px` is the icon size to
/// pre-resolve and `on_change` runs on the worker thread after a new app was published (it should
/// post a message to the UI thread).
pub fn configure(enabled: bool, px: u32, on_change: Box<dyn Fn() + Send>) {
    let running = TRACKER.with(|t| t.borrow().is_some());
    if enabled == running {
        return;
    }
    if !enabled {
        TRACKER.with(|t| *t.borrow_mut() = None);
        return;
    }
    let (tx, rx) = channel::<isize>();
    let spawned = std::thread::Builder::new().name("fg-icon".into()).stack_size(512 * 1024).spawn(move || worker(rx, px, on_change));
    if spawned.is_err() {
        return;
    }
    // SAFETY: an out-of-context hook with a static callback, removed in `Tracker::drop` on this thread.
    let hook = unsafe { SetWinEventHook(EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_FOREGROUND, None, Some(on_foreground), 0, 0, WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS) };
    if hook.is_invalid() {
        return; // `tx` drops here: the worker ends
    }
    // Seed with the window in front now, so the first hotkey press already finds its icon.
    // SAFETY: trivially safe query.
    let fg = unsafe { GetForegroundWindow() };
    if !fg.is_invalid() {
        let _ = tx.send(fg.0 as isize);
    }
    HOOK_TX.with(|t| *t.borrow_mut() = Some(tx));
    TRACKER.with(|t| *t.borrow_mut() = Some(Tracker { hook }));
}

/// The first of `windows` (newest first) that is a valid target, described.
fn describe_newest(windows: &[isize], own: u32) -> Option<Target> {
    windows.iter().rev().find_map(|&h| {
        let t = paste::describe_window(HWND(h as *mut core::ffi::c_void))?;
        (!is_ignored(&t, own)).then_some(t)
    })
}

fn worker(rx: Receiver<isize>, px: u32, on_change: Box<dyn Fn() + Send>) {
    // SAFETY: affects only this thread. Background mode also lowers its I/O and memory priority.
    unsafe {
        let me = GetCurrentThread();
        if SetThreadPriority(me, THREAD_MODE_BACKGROUND_BEGIN).is_err() {
            let _ = SetThreadPriority(me, THREAD_PRIORITY_LOWEST);
        }
    }
    let own = own_pid();
    // Blocks while idle. A burst (Alt-Tab, a taskbar click) is handled as one: the newest window
    // that is a real target wins, which keeps an earlier real app when the burst ends on a shell
    // surface.
    while let Ok(first) = rx.recv() {
        let mut burst = vec![first];
        burst.extend(rx.try_iter());
        let Some(t) = describe_newest(&burst, own) else { continue };
        let same = lock_last().as_ref().is_some_and(|l| l.hwnd == t.hwnd && l.exe_path == t.exe_path);
        if same {
            continue;
        }
        appicon::prefetch(&t, px);
        *lock_last() = Some(t);
        on_change();
    }
}

// ---------------------------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn win(hwnd: isize, pid: u32, exe: &str, class: &str) -> Target {
        Target {
            hwnd,
            pid,
            exe_path: format!("C:\\Apps\\{exe}"),
            exe_name: exe.to_ascii_lowercase(),
            class_name: class.into(),
            ..Target::default()
        }
    }

    const OWN: u32 = 4242;

    #[test]
    fn shell_surfaces_and_own_windows_are_ignored() {
        for (class, exe) in [
            ("Shell_TrayWnd", "explorer.exe"),
            ("Shell_SecondaryTrayWnd", "explorer.exe"),
            ("TopLevelWindowForOverflowXamlIsland", "explorer.exe"),
            ("NotifyIconOverflowWindow", "explorer.exe"),
            ("XamlExplorerHostIslandWindow", "explorer.exe"),
            ("MultitaskingViewFrame", "explorer.exe"),
            ("Windows.UI.Core.CoreWindow", "StartMenuExperienceHost.exe"),
            ("Windows.UI.Core.CoreWindow", "SearchHost.exe"),
            ("Windows.UI.Core.CoreWindow", "ShellExperienceHost.exe"),
        ] {
            assert!(is_ignored(&win(1, 7, exe, class), OWN), "{class} / {exe}");
        }
        // the daemon's own windows, whatever their class
        assert!(is_ignored(&win(1, OWN, "opentalk.exe", "OpenTalkPill"), OWN));
    }

    #[test]
    fn real_apps_are_not_ignored() {
        for (class, exe) in [
            ("CabinetWClass", "explorer.exe"), // a File Explorer window is a real target
            ("Notepad", "notepad.exe"),
            ("Chrome_WidgetWin_1", "chrome.exe"),
            ("ApplicationFrameWindow", "ApplicationFrameHost.exe"),
            ("CASCADIA_HOSTING_WINDOW_CLASS", "WindowsTerminal.exe"),
            ("Progman", "explorer.exe"), // the desktop: the user chose it, nothing is guessed
        ] {
            assert!(!is_ignored(&win(1, 7, exe, class), OWN), "{class} / {exe}");
        }
        // Settings is another OpenTalk process: a real target (dictating into the Dictionary page)
        assert!(!is_ignored(&win(1, 99, "opentalk.exe", "Window Class"), OWN));
        // an unreadable exe path is not a reason to ignore
        assert!(!is_ignored(&Target { hwnd: 1, pid: 7, ..Target::default() }, OWN));
        // and an unknown own pid (0) never matches a process
        assert!(!is_ignored(&Target { hwnd: 1, pid: 0, ..Target::default() }, 0));
    }

    #[test]
    fn the_front_window_wins_unless_it_is_ignorable() {
        let notepad = win(10, 7, "notepad.exe", "Notepad");
        let chrome = win(20, 8, "chrome.exe", "Chrome_WidgetWin_1");
        let taskbar = win(30, 9, "explorer.exe", "Shell_TrayWnd");
        // a real app in front replaces the previous one
        assert_eq!(resolve(Some(chrome.clone()), Some(&notepad), OWN), Some(chrome.clone()));
        // a shell surface in front keeps the previous target
        assert_eq!(resolve(Some(taskbar.clone()), Some(&notepad), OWN), Some(notepad.clone()));
        // so does the daemon's own window, and nothing in front
        assert_eq!(resolve(Some(win(40, OWN, "opentalk.exe", "Tray")), Some(&notepad), OWN), Some(notepad.clone()));
        assert_eq!(resolve(None, Some(&notepad), OWN), Some(notepad));
        // nothing known at all
        assert_eq!(resolve(Some(taskbar), None, OWN), None);
        assert_eq!(resolve(Some(chrome.clone()), None, OWN), Some(chrome));
    }

    #[test]
    fn the_bubble_follows_the_app_switched_to_mid_recording() {
        let notepad = win(10, 7, "notepad.exe", "Notepad");
        let notepad2 = win(11, 7, "notepad.exe", "Notepad");
        let chrome = win(20, 8, "chrome.exe", "Chrome_WidgetWin_1");
        let mut f = Follow::default();
        // nothing is followed outside a dictation
        assert_eq!(f.observe(&chrome), Change::Same);

        f.begin(Some(notepad.clone()));
        assert_eq!(f.observe(&notepad), Change::Same, "the same window again changes nothing");
        assert_eq!(f.observe(&notepad2), Change::Window, "another window of the app moves the target only");
        assert_eq!(f.observe(&chrome), Change::App, "another app swaps the bubble");
        assert_eq!(f.observe(&chrome), Change::Same);
        // switching back is a change again, and the target is whatever was last seen
        assert_eq!(f.observe(&notepad), Change::App);
        f.end();
        assert!(!f.active());
        assert_eq!(f.observe(&chrome), Change::Same, "over: nothing follows");
    }

    #[test]
    fn a_dictation_that_started_with_no_app_picks_up_the_first_one() {
        let mut f = Follow::default();
        f.begin(None);
        assert_eq!(f.observe(&win(10, 7, "notepad.exe", "Notepad")), Change::App);
    }

    #[test]
    fn apps_are_told_apart_by_executable_case_insensitively() {
        let a = win(1, 7, "Notepad.exe", "Notepad");
        let mut b = win(2, 8, "notepad.exe", "Notepad");
        b.exe_path = a.exe_path.to_ascii_uppercase();
        let mut f = Follow::default();
        f.begin(Some(a));
        assert_eq!(f.observe(&b), Change::Window);
        // an unreadable path falls back to the process
        let (u1, u2) = (Target { hwnd: 5, pid: 50, ..Target::default() }, Target { hwnd: 6, pid: 51, ..Target::default() });
        f.begin(Some(u1.clone()));
        assert_eq!(f.observe(&Target { hwnd: 7, ..u1 }), Change::Window);
        assert_eq!(f.observe(&u2), Change::App);
    }

    /// Needs a real, unlocked desktop. The tracker seeds itself with the window in front, resolves
    /// that app's icon into the cache before anyone asks, publishes it, and keeps it when the taskbar
    /// takes the foreground; switching it off clears everything.
    #[test]
    #[ignore]
    fn desktop_tracker_prefetches_publishes_and_ignores_the_taskbar() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::time::{Duration, Instant};
        use windows::Win32::UI::WindowsAndMessaging::{DispatchMessageW, FindWindowW, MSG, PM_REMOVE, PeekMessageW, SetForegroundWindow, TranslateMessage};
        use windows::core::w;

        if !paste::require_input_desktop("desktop_tracker_prefetches_publishes_and_ignores_the_taskbar") {
            return;
        }
        fn pump(ms: u64, until: &dyn Fn() -> bool) {
            let end = Instant::now() + Duration::from_millis(ms);
            while Instant::now() < end && !until() {
                // SAFETY: a plain message pump on the thread that owns the hook.
                unsafe {
                    let mut m = MSG::default();
                    while PeekMessageW(&mut m, None, 0, 0, PM_REMOVE).as_bool() {
                        let _ = TranslateMessage(&m);
                        DispatchMessageW(&m);
                    }
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        let fired = Arc::new(AtomicUsize::new(0));
        let f = fired.clone();
        configure(true, 64, Box::new(move || {
            f.fetch_add(1, Ordering::SeqCst);
        }));
        pump(3000, &|| last().is_some());
        let Some(front) = last() else {
            eprintln!("SKIPPED: the foreground window is a shell surface or unreadable");
            configure(false, 64, Box::new(|| {}));
            return;
        };
        assert!(!is_ignored(&front, own_pid()), "a published window is always a valid target: {front:?}");
        assert!(fired.load(Ordering::SeqCst) >= 1, "the UI was told");
        assert!(appicon::cached(&front, 64).is_some() || front.exe_path.is_empty(), "the icon is ready before any hotkey: {front:?}");
        // the taskbar in front keeps the previous target
        // SAFETY: plain window lookups; SetForegroundWindow may be refused, which is fine here.
        unsafe {
            if let Ok(tray) = FindWindowW(w!("Shell_TrayWnd"), None) {
                let _ = SetForegroundWindow(tray);
            }
        }
        pump(600, &|| false);
        assert!(last().is_some_and(|t| !is_ignored(&t, own_pid())));
        configure(false, 64, Box::new(|| {}));
        assert!(last().is_none(), "switching the bubble off clears the tracker");
    }
}
