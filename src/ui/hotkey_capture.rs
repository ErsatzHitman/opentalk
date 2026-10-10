//! Hotkey capture for the Hotkeys settings page.
//!
//! While the user presses a combination, a temporary `WH_KEYBOARD_LL` hook (own thread + message
//! loop) records it and swallows the keys, so the Win key does not open the Start menu and
//! letters do not type into the window. The daemon's own hook must be suspended meanwhile
//! (`IpcMsg::SuspendHotkeys(true)`, sent by the caller).
//!
//! The finished combo is an Electron-style accelerator ("Control+Super", "Control+Shift+Space",
//! "F8"), the format `daemon::hotkey::parse` reads.
use eframe::egui;
use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

// ---------------------------------------------------------------- pure key model

const VK_BACK: u32 = 0x08;
const VK_TAB: u32 = 0x09;
const VK_RETURN: u32 = 0x0D;
const VK_SHIFT: u32 = 0x10;
const VK_CONTROL: u32 = 0x11;
const VK_MENU: u32 = 0x12;
const VK_PAUSE: u32 = 0x13;
const VK_CAPITAL: u32 = 0x14;
const VK_ESCAPE: u32 = 0x1B;
const VK_SPACE: u32 = 0x20;
const VK_LWIN: u32 = 0x5B;
const VK_RWIN: u32 = 0x5C;
const VK_LSHIFT: u32 = 0xA0;
const VK_RSHIFT: u32 = 0xA1;
const VK_LCONTROL: u32 = 0xA2;
const VK_RCONTROL: u32 = 0xA3;
const VK_LMENU: u32 = 0xA4;
const VK_RMENU: u32 = 0xA5;
/// Pseudo code used internally for "either Win key".
const SUPER: u32 = 0x5B;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Mod {
    Ctrl,
    Alt,
    Shift,
    Super,
}

/// Map a raw virtual key to a modifier (left/right variants collapse).
fn modifier_of(vk: u32) -> Option<Mod> {
    match vk {
        VK_CONTROL | VK_LCONTROL | VK_RCONTROL => Some(Mod::Ctrl),
        VK_MENU | VK_LMENU | VK_RMENU => Some(Mod::Alt),
        VK_SHIFT | VK_LSHIFT | VK_RSHIFT => Some(Mod::Shift),
        VK_LWIN | VK_RWIN => Some(Mod::Super),
        _ => None,
    }
}

/// Electron accelerator name of a non-modifier key; None for keys we cannot name.
pub fn key_name(vk: u32) -> Option<String> {
    Some(match vk {
        0x30..=0x39 | 0x41..=0x5A => char::from_u32(vk)?.to_string(),
        0x70..=0x87 => format!("F{}", vk - 0x6F),
        0x60..=0x69 => format!("num{}", vk - 0x60),
        0x6A => "nummult".into(),
        0x6B => "numadd".into(),
        0x6D => "numsub".into(),
        0x6E => "numdec".into(),
        0x6F => "numdiv".into(),
        VK_SPACE => "Space".into(),
        VK_RETURN => "Enter".into(),
        VK_TAB => "Tab".into(),
        VK_BACK => "Backspace".into(),
        VK_ESCAPE => "Escape".into(),
        VK_CAPITAL => "Capslock".into(),
        VK_PAUSE => "Pause".into(),
        0x21 => "PageUp".into(),
        0x22 => "PageDown".into(),
        0x23 => "End".into(),
        0x24 => "Home".into(),
        0x25 => "Left".into(),
        0x26 => "Up".into(),
        0x27 => "Right".into(),
        0x28 => "Down".into(),
        0x2C => "PrintScreen".into(),
        0x2D => "Insert".into(),
        0x2E => "Delete".into(),
        0x90 => "Numlock".into(),
        0x91 => "Scrolllock".into(),
        0xBA => ";".into(),
        0xBB => "=".into(),
        0xBC => ",".into(),
        0xBD => "-".into(),
        0xBE => ".".into(),
        0xBF => "/".into(),
        0xC0 => "`".into(),
        0xDB => "[".into(),
        0xDC => "\\".into(),
        0xDD => "]".into(),
        0xDE => "'".into(),
        _ => return None,
    })
}

/// Build the accelerator from the keys seen during one capture. `last_main` is the most
/// recently pressed non-modifier key. None when nothing usable was pressed.
fn build_accelerator(seen: &BTreeSet<u32>, last_main: Option<u32>) -> Option<String> {
    let mods: BTreeSet<Mod> = seen.iter().filter_map(|&v| modifier_of(v)).collect();
    let mut parts: Vec<String> = Vec::new();
    for m in &mods {
        parts.push(
            match m {
                Mod::Ctrl => "Control",
                Mod::Alt => "Alt",
                Mod::Shift => "Shift",
                Mod::Super => "Super",
            }
            .into(),
        );
    }
    if let Some(vk) = last_main {
        parts.push(key_name(vk)?);
    }
    if parts.is_empty() { None } else { Some(parts.join("+")) }
}

/// Why an accelerator cannot be used as the dictation hotkey (None = fine).
pub fn validate(accel: &str) -> Option<&'static str> {
    let parts: Vec<&str> = accel.split('+').collect();
    if parts.len() >= 2 {
        return None;
    }
    let k = parts.first().copied().unwrap_or("");
    let is_f = k.len() >= 2 && k.starts_with('F') && k[1..].chars().all(|c| c.is_ascii_digit());
    if is_f {
        None
    } else if matches!(k, "Control" | "Alt" | "Shift" | "Super") {
        Some("A single modifier would trigger constantly. Combine it with another key.")
    } else {
        Some("Add a modifier (Ctrl, Alt, Shift or Win) so normal typing is not captured.")
    }
}

/// Human label: "Control+Super" -> "Ctrl + Win".
pub fn label(accel: &str) -> String {
    accel
        .split('+')
        .filter(|s| !s.is_empty())
        .map(|k| match k {
            "Control" | "CommandOrControl" | "CmdOrCtrl" | "Ctrl" => "Ctrl".to_string(),
            "Super" | "Meta" | "Win" | "Command" | "Cmd" => "Win".to_string(),
            "Option" => "Alt".to_string(),
            other => other.to_string(),
        })
        .collect::<Vec<_>>()
        .join(" + ")
}

/// Keys of an accelerator as individual label parts (for rendering key caps).
pub fn parts(accel: &str) -> Vec<String> {
    label(accel).split(" + ").filter(|s| !s.is_empty()).map(str::to_string).collect()
}

// ---------------------------------------------------------------- capture state machine

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Captured {
    /// A finished combo (accelerator string).
    Combo(String),
    /// Escape alone: abandon the capture.
    Cancelled,
}

#[derive(Default)]
struct Shared {
    held: BTreeSet<u32>,
    seen: BTreeSet<u32>,
    last_main: Option<u32>,
    done: Option<Captured>,
    ctx: Option<egui::Context>,
}

impl Shared {
    /// Normalised key: left/right variants of one modifier collapse to the same code.
    fn norm(vk: u32) -> u32 {
        match modifier_of(vk) {
            Some(Mod::Ctrl) => VK_CONTROL,
            Some(Mod::Alt) => VK_MENU,
            Some(Mod::Shift) => VK_SHIFT,
            Some(Mod::Super) => SUPER,
            None => vk,
        }
    }

    /// Feed a key event. Returns true when the event should be swallowed.
    fn feed(&mut self, vk: u32, down: bool) -> bool {
        let vk = Self::norm(vk);
        if self.done.is_some() {
            // Result already decided: keep swallowing the keys of this gesture until released.
            if down {
                self.held.insert(vk);
                return true;
            }
            return self.held.remove(&vk);
        }
        if down {
            if vk == VK_ESCAPE && self.held.is_empty() && self.seen.is_empty() {
                self.done = Some(Captured::Cancelled);
                self.held.insert(vk);
                self.wake();
                return true;
            }
            self.held.insert(vk);
            self.seen.insert(vk);
            if modifier_of(vk).is_none() {
                self.last_main = Some(vk);
            }
            self.wake();
            true
        } else {
            // Only swallow releases of keys whose press we swallowed (else the OS sees a stuck key).
            let known = self.held.remove(&vk);
            if known && self.held.is_empty() && self.done.is_none() {
                self.done = build_accelerator(&self.seen, self.last_main).map(Captured::Combo);
                if self.done.is_none() {
                    self.seen.clear();
                    self.last_main = None;
                }
                self.wake();
            }
            known
        }
    }

    fn wake(&self) {
        if let Some(c) = &self.ctx {
            c.request_repaint();
        }
    }

    /// Live preview while keys are down.
    fn preview(&self) -> String {
        build_accelerator(&self.seen, self.last_main).map(|a| label(&a)).unwrap_or_default()
    }
}

// ---------------------------------------------------------------- Windows hook plumbing

static ACTIVE: Mutex<Option<Arc<Mutex<Shared>>>> = Mutex::new(None);
/// Accept synthetic (injected) key events too. Off in normal use, matching the daemon, which ignores
/// them; the desktop tests and `OPENTALK_CAPTURE_INJECTED=1` turn it on so input can be scripted.
static ALLOW_INJECTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn allow_injected(on: bool) {
    ALLOW_INJECTED.store(on, std::sync::atomic::Ordering::Relaxed);
}

pub struct Capture {
    shared: Arc<Mutex<Shared>>,
    thread_id: u32,
    join: Option<std::thread::JoinHandle<()>>,
}

#[cfg(windows)]
mod sys {
    use super::*;
    use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::System::Threading::GetCurrentThreadId;
    use windows::Win32::UI::WindowsAndMessaging::{
        CallNextHookEx, DispatchMessageW, GetMessageW, KBDLLHOOKSTRUCT, MSG, PostThreadMessageW, SetWindowsHookExW,
        TranslateMessage, UnhookWindowsHookEx, PeekMessageW, PM_NOREMOVE, WM_USER, WH_KEYBOARD_LL, WM_KEYDOWN, WM_KEYUP, WM_QUIT, WM_SYSKEYDOWN, WM_SYSKEYUP,
    };

    const LLKHF_INJECTED: u32 = 0x10;

    unsafe extern "system" fn proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        if code >= 0 {
            // SAFETY: for WH_KEYBOARD_LL with code >= 0, lparam points to a valid KBDLLHOOKSTRUCT.
            let info = unsafe { &*(lparam.0 as *const KBDLLHOOKSTRUCT) };
            if info.flags.0 & LLKHF_INJECTED == 0 || ALLOW_INJECTED.load(std::sync::atomic::Ordering::Relaxed) {
                let down = matches!(wparam.0 as u32, WM_KEYDOWN | WM_SYSKEYDOWN);
                let up = matches!(wparam.0 as u32, WM_KEYUP | WM_SYSKEYUP);
                if down || up {
                    let shared = ACTIVE.lock().ok().and_then(|g| g.clone());
                    if let Some(s) = shared {
                        let swallow = s.lock().map(|mut st| st.feed(info.vkCode, down)).unwrap_or(false);
                        if swallow {
                            return LRESULT(1);
                        }
                    }
                }
            }
        }
        // SAFETY: forwarding the unmodified hook arguments.
        unsafe { CallNextHookEx(None, code, wparam, lparam) }
    }

    pub fn spawn(shared: Arc<Mutex<Shared>>) -> Option<(u32, std::thread::JoinHandle<()>)> {
        let (tx, rx) = std::sync::mpsc::channel::<Option<u32>>();
        *ACTIVE.lock().ok()? = Some(shared);
        let join = std::thread::Builder::new()
            .name("hotkey-capture".into())
            .spawn(move || {
                // SAFETY: standard LL hook lifecycle on one thread: install, pump messages, unhook.
                unsafe {
                    let hmod = GetModuleHandleW(None).ok();
                    let hook = SetWindowsHookExW(WH_KEYBOARD_LL, Some(proc), hmod.map(|h| windows::Win32::Foundation::HINSTANCE(h.0)), 0);
                    let hook = match hook {
                        Ok(h) => h,
                        Err(_) => {
                            let _ = tx.send(None);
                            return;
                        }
                    };
                    let mut msg = MSG::default();
                    // Create this thread's message queue before announcing its id, so an early
                    // WM_QUIT from `Capture::drop` is queued instead of being dropped.
                    let _ = PeekMessageW(&mut msg, None, WM_USER, WM_USER, PM_NOREMOVE);
                    let _ = tx.send(Some(GetCurrentThreadId()));
                    while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                        let _ = TranslateMessage(&msg);
                        DispatchMessageW(&msg);
                    }
                    let _ = UnhookWindowsHookEx(hook);
                }
            })
            .ok()?;
        match rx.recv_timeout(std::time::Duration::from_secs(2)) {
            Ok(Some(tid)) => Some((tid, join)),
            _ => {
                if let Ok(mut g) = ACTIVE.lock() {
                    *g = None;
                }
                None
            }
        }
    }

    pub fn quit(thread_id: u32) {
        // SAFETY: posting WM_QUIT to our own helper thread's queue.
        unsafe {
            let _ = PostThreadMessageW(thread_id, WM_QUIT, WPARAM(0), LPARAM(0));
        }
    }
}

impl Capture {
    /// Start capturing. None when the hook could not be installed.
    pub fn start(ctx: &egui::Context) -> Option<Capture> {
        if std::env::var_os("OPENTALK_CAPTURE_INJECTED").is_some() {
            allow_injected(true);
        }
        let shared = Arc::new(Mutex::new(Shared { ctx: Some(ctx.clone()), ..Default::default() }));
        let (thread_id, join) = sys::spawn(shared.clone())?;
        Some(Capture { shared, thread_id, join: Some(join) })
    }

    /// (live preview label, finished result if any).
    pub fn poll(&self) -> (String, Option<Captured>) {
        match self.shared.lock() {
            Ok(s) => (s.preview(), s.done.clone()),
            Err(_) => (String::new(), Some(Captured::Cancelled)),
        }
    }

    /// Re-arm after a rejected combo (keeps the hook installed).
    pub fn rearm(&self) {
        if let Ok(mut s) = self.shared.lock() {
            s.seen.clear();
            s.last_main = None;
            if s.held.is_empty() {
                s.done = None;
            }
        }
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        if let Ok(mut g) = ACTIVE.lock() {
            *g = None;
        }
        sys::quit(self.thread_id);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(s: &mut Shared, keys: &[u32]) {
        for &k in keys {
            s.feed(k, true);
        }
        for &k in keys.iter().rev() {
            s.feed(k, false);
        }
    }

    #[test]
    fn modifier_only_combo_with_win() {
        let mut s = Shared::default();
        press(&mut s, &[VK_LCONTROL, VK_LWIN]);
        assert_eq!(s.done, Some(Captured::Combo("Control+Super".into())));
    }

    #[test]
    fn right_variants_collapse_and_order_is_canonical() {
        let mut s = Shared::default();
        press(&mut s, &[VK_RWIN, VK_RSHIFT, VK_RCONTROL, 0x20]);
        assert_eq!(s.done, Some(Captured::Combo("Control+Shift+Super+Space".into())));
    }

    #[test]
    fn function_key_alone_and_letter_combo() {
        let mut s = Shared::default();
        press(&mut s, &[0x77]);
        assert_eq!(s.done, Some(Captured::Combo("F8".into())));
        let mut s = Shared::default();
        press(&mut s, &[VK_MENU, 0x52]);
        assert_eq!(s.done, Some(Captured::Combo("Alt+R".into())));
    }

    #[test]
    fn escape_alone_cancels_but_with_modifier_is_a_key() {
        let mut s = Shared::default();
        s.feed(VK_ESCAPE, true);
        assert_eq!(s.done, Some(Captured::Cancelled));
        let mut s = Shared::default();
        press(&mut s, &[VK_CONTROL, VK_ESCAPE]);
        assert_eq!(s.done, Some(Captured::Combo("Control+Escape".into())));
    }

    #[test]
    fn unknown_release_is_not_swallowed() {
        let mut s = Shared::default();
        assert!(!s.feed(VK_LCONTROL, false), "a key-up without a seen key-down must pass through");
        assert!(s.feed(VK_LCONTROL, true));
    }

    #[test]
    fn result_only_after_all_keys_released() {
        let mut s = Shared::default();
        s.feed(VK_LCONTROL, true);
        s.feed(VK_LWIN, true);
        s.feed(VK_LWIN, false);
        assert!(s.done.is_none(), "Ctrl is still down");
        s.feed(VK_LCONTROL, false);
        assert!(s.done.is_some());
    }

    #[test]
    fn labels_and_validation() {
        assert_eq!(label("Control+Super"), "Ctrl + Win");
        assert_eq!(label("CommandOrControl+Shift+Space"), "Ctrl + Shift + Space");
        assert_eq!(parts("Control+Super"), vec!["Ctrl", "Win"]);
        assert!(validate("Control+Super").is_none());
        assert!(validate("F8").is_none());
        assert!(validate("A").is_some());
        assert!(validate("Control").is_some());
    }

    #[test]
    fn every_nameable_key_is_accepted_by_the_daemon_parser() {
        for vk in 0u32..=0xFF {
            if modifier_of(vk).is_some() {
                continue;
            }
            if let Some(name) = key_name(vk) {
                let accel = format!("Control+{name}");
                let spec = crate::daemon::hotkey::parse(&accel).unwrap_or_else(|e| panic!("vk {vk:#x} -> {accel}: {e}"));
                assert!(spec.ctrl && spec.vk.is_some(), "{accel}");
            }
        }
    }

    #[test]
    fn captured_combos_survive_a_parse_round_trip() {
        let mut s = Shared::default();
        press(&mut s, &[VK_LCONTROL, VK_LSHIFT, VK_LMENU, VK_LWIN, 0x41]);
        let Some(Captured::Combo(a)) = s.done.clone() else { panic!("no combo") };
        let spec = crate::daemon::hotkey::parse(&a).expect("daemon parses it");
        assert!(spec.ctrl && spec.shift && spec.alt && spec.win);
        assert_eq!(spec.vk, Some(0x41));
        assert_eq!(validate(&a), None);
    }

    #[test]
    fn key_names() {
        assert_eq!(key_name(0x41).as_deref(), Some("A"));
        assert_eq!(key_name(0x78).as_deref(), Some("F9"));
        assert_eq!(key_name(0xC0).as_deref(), Some("`"));
        assert_eq!(key_name(0xFF), None);
    }
}

#[cfg(test)]
mod desktop_tests {
    use super::*;
    use windows::Win32::UI::Input::KeyboardAndMouse::{KEYBD_EVENT_FLAGS, KEYEVENTF_KEYUP, keybd_event};

    fn key(vk: u8, down: bool) {
        // SAFETY: plain synthetic key event; no pointers involved.
        unsafe { keybd_event(vk, 0, if down { KEYBD_EVENT_FLAGS(0) } else { KEYEVENTF_KEYUP }, 0) };
        std::thread::sleep(std::time::Duration::from_millis(40));
    }

    fn wait_result(c: &Capture) -> Option<Captured> {
        for _ in 0..100 {
            if let (_, Some(r)) = c.poll() {
                return Some(r);
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        None
    }

    /// Real LL hook + synthetic keys: Ctrl then Win (modifier-only combo), released together.
    #[test]
    #[ignore = "installs a global keyboard hook and injects keys"]
    fn desktop_capture_ctrl_win() {
        if !crate::daemon::paste::require_input_desktop("desktop_capture_ctrl_win") {
            return;
        }
        allow_injected(true);
        let ctx = egui::Context::default();
        let c = Capture::start(&ctx).expect("hook installs");
        key(0xA2, true); // LCtrl
        key(0x5B, true); // LWin
        key(0x5B, false);
        key(0xA2, false);
        let r = wait_result(&c);
        drop(c);
        allow_injected(false);
        assert_eq!(r, Some(Captured::Combo("Control+Super".into())));
    }

    #[test]
    #[ignore = "installs a global keyboard hook and injects keys"]
    fn desktop_capture_escape_cancels_and_f8_works() {
        if !crate::daemon::paste::require_input_desktop("desktop_capture_escape_cancels_and_f8_works") {
            return;
        }
        allow_injected(true);
        let ctx = egui::Context::default();
        let c = Capture::start(&ctx).expect("hook installs");
        key(0x1B, true);
        key(0x1B, false);
        assert_eq!(wait_result(&c), Some(Captured::Cancelled));
        drop(c);
        let c = Capture::start(&ctx).expect("hook installs again after drop");
        key(0x77, true);
        key(0x77, false);
        let r = wait_result(&c);
        drop(c);
        allow_injected(false);
        assert_eq!(r, Some(Captured::Combo("F8".into())));
    }
}
