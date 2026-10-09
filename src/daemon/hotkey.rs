//! Global hotkey via WH_KEYBOARD_LL on a dedicated thread. TASK win-input implements.
//! Port of OpenWhispr resources/windows-key-listener.c matching (modifier superset, any press
//! order, missed key-up self-heal, left/right Win) plus: ignore LLKHF_INJECTED, swallow Esc only
//! while `set_esc_capture(true)`, Start-menu suppression for combos containing Win (send mask key
//! VK 0xE8 down/up before the Win key-up passes through), suspend during hotkey capture.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HotkeyEvent {
    Down = 1,
    Up = 2,
    Escape = 3,
}

impl HotkeyEvent {
    pub fn from_wparam(w: usize) -> Option<Self> {
        match w {
            1 => Some(Self::Down),
            2 => Some(Self::Up),
            3 => Some(Self::Escape),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct HotkeySpec {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub win: bool,
    /// Main virtual-key code; None = modifier-only combo (e.g. Control+Super).
    pub vk: Option<u32>,
}

/// Parse an Electron-style accelerator ("Control+Super", "CommandOrControl+Shift+Space", "F8",
/// "RightAlt", ...). Unknown keys are an error (unlike OpenWhispr's silent atoi fallback).
pub fn parse(spec: &str) -> Result<HotkeySpec, String> {
    let _ = spec;
    todo!("win-input")
}

/// Human label, e.g. "Ctrl + Win", "Ctrl + Shift + Space".
pub fn label(spec: &str) -> String {
    let _ = spec;
    todo!("win-input")
}

/// Install the hook thread. Events are posted as `PostMessageW(hwnd, msg, HotkeyEvent as usize, 0)`.
pub fn start(hwnd: isize, msg: u32, spec: &str) -> Result<(), String> {
    let _ = (hwnd, msg, spec);
    todo!("win-input")
}

pub fn set_hotkey(spec: &str) -> Result<(), String> {
    let _ = spec;
    todo!("win-input")
}
pub fn set_esc_capture(on: bool) {
    let _ = on;
    todo!("win-input")
}
pub fn set_suspended(on: bool) {
    let _ = on;
    todo!("win-input")
}
pub fn set_start_menu_mask(on: bool) {
    let _ = on;
    todo!("win-input")
}
