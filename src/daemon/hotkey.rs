//! Global hotkey via WH_KEYBOARD_LL on a dedicated thread. TASK win-input implements.
//! Port of OpenWhispr resources/windows-key-listener.c matching (modifier superset, any press
//! order, missed key-up self-heal, left/right Win) plus: ignore LLKHF_INJECTED, swallow Esc only
//! while `set_esc_capture(true)`, Start-menu suppression for combos containing Win (send mask key
//! VK 0xE8 down/up before the Win key-up passes through), suspend during hotkey capture.
//!
//! Layout: [`parse`]/[`label`] (accelerator text), [`Matcher`] (pure state machine, unit-tested
//! with an injected [`KeyState`]), and the thin Win32 shell (hook thread, `PostMessageW`, mask
//! injection). The hook callback only does O(1), allocation-free work: one `try_lock`, a few
//! `GetAsyncKeyState` reads and a `PostMessageW`.

use std::sync::Mutex;
use std::sync::TryLockError;
use std::sync::atomic::{AtomicIsize, AtomicU32, Ordering};

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

impl HotkeySpec {
    const EMPTY: HotkeySpec = HotkeySpec { ctrl: false, alt: false, shift: false, win: false, vk: None };

    /// True when the spec requires nothing (it would match every key press).
    fn is_empty(&self) -> bool {
        !self.ctrl && !self.alt && !self.shift && !self.win && self.vk.is_none()
    }

    fn has_modifiers(&self) -> bool {
        self.ctrl || self.alt || self.shift || self.win
    }
}

// ---------------------------------------------------------------------------------------------
// Virtual-key codes (kept local so the parser and matcher are plain, platform-free code).
// ---------------------------------------------------------------------------------------------

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
const VK_PRIOR: u32 = 0x21;
const VK_NEXT: u32 = 0x22;
const VK_END: u32 = 0x23;
const VK_HOME: u32 = 0x24;
const VK_LEFT: u32 = 0x25;
const VK_UP: u32 = 0x26;
const VK_RIGHT: u32 = 0x27;
const VK_DOWN: u32 = 0x28;
const VK_SNAPSHOT: u32 = 0x2C;
const VK_INSERT: u32 = 0x2D;
const VK_DELETE: u32 = 0x2E;
const VK_LWIN: u32 = 0x5B;
const VK_RWIN: u32 = 0x5C;
const VK_APPS: u32 = 0x5D;
const VK_NUMPAD0: u32 = 0x60;
const VK_MULTIPLY: u32 = 0x6A;
const VK_ADD: u32 = 0x6B;
const VK_SUBTRACT: u32 = 0x6D;
const VK_DECIMAL: u32 = 0x6E;
const VK_DIVIDE: u32 = 0x6F;
const VK_F1: u32 = 0x70;
const VK_NUMLOCK: u32 = 0x90;
const VK_SCROLL: u32 = 0x91;
const VK_LSHIFT: u32 = 0xA0;
const VK_RSHIFT: u32 = 0xA1;
const VK_LCONTROL: u32 = 0xA2;
const VK_RCONTROL: u32 = 0xA3;
const VK_LMENU: u32 = 0xA4;
const VK_RMENU: u32 = 0xA5;
const VK_VOLUME_MUTE: u32 = 0xAD;
const VK_VOLUME_DOWN: u32 = 0xAE;
const VK_VOLUME_UP: u32 = 0xAF;
const VK_MEDIA_NEXT_TRACK: u32 = 0xB0;
const VK_MEDIA_PREV_TRACK: u32 = 0xB1;
const VK_MEDIA_STOP: u32 = 0xB2;
const VK_MEDIA_PLAY_PAUSE: u32 = 0xB3;
const VK_OEM_1: u32 = 0xBA;
const VK_OEM_PLUS: u32 = 0xBB;
const VK_OEM_COMMA: u32 = 0xBC;
const VK_OEM_MINUS: u32 = 0xBD;
const VK_OEM_PERIOD: u32 = 0xBE;
const VK_OEM_2: u32 = 0xBF;
const VK_OEM_3: u32 = 0xC0;
const VK_OEM_4: u32 = 0xDB;
const VK_OEM_5: u32 = 0xDC;
const VK_OEM_6: u32 = 0xDD;
const VK_OEM_7: u32 = 0xDE;
/// Unassigned VK used to cancel the Start menu (same one AutoHotkey's #MenuMaskKey defaults to).
const VK_MASK: u32 = 0xE8;

// ---------------------------------------------------------------------------------------------
// Accelerator parsing / labels
// ---------------------------------------------------------------------------------------------

/// Display names for keys that are not a single letter/digit/F-key (also the label vocabulary).
const KEY_LABELS: &[(u32, &str)] = &[
    (VK_BACK, "Backspace"),
    (VK_TAB, "Tab"),
    (VK_RETURN, "Enter"),
    (VK_PAUSE, "Pause"),
    (VK_CAPITAL, "CapsLock"),
    (VK_ESCAPE, "Esc"),
    (VK_SPACE, "Space"),
    (VK_PRIOR, "PageUp"),
    (VK_NEXT, "PageDown"),
    (VK_END, "End"),
    (VK_HOME, "Home"),
    (VK_LEFT, "Left"),
    (VK_UP, "Up"),
    (VK_RIGHT, "Right"),
    (VK_DOWN, "Down"),
    (VK_SNAPSHOT, "PrintScreen"),
    (VK_INSERT, "Insert"),
    (VK_DELETE, "Delete"),
    (VK_LWIN, "Left Win"),
    (VK_RWIN, "Right Win"),
    (VK_APPS, "Menu"),
    (VK_MULTIPLY, "Num *"),
    (VK_ADD, "Num +"),
    (VK_SUBTRACT, "Num -"),
    (VK_DECIMAL, "Num ."),
    (VK_DIVIDE, "Num /"),
    (VK_NUMLOCK, "NumLock"),
    (VK_SCROLL, "ScrollLock"),
    (VK_LSHIFT, "Left Shift"),
    (VK_RSHIFT, "Right Shift"),
    (VK_LCONTROL, "Left Ctrl"),
    (VK_RCONTROL, "Right Ctrl"),
    (VK_LMENU, "Left Alt"),
    (VK_RMENU, "Right Alt"),
    (VK_VOLUME_MUTE, "Volume Mute"),
    (VK_VOLUME_DOWN, "Volume Down"),
    (VK_VOLUME_UP, "Volume Up"),
    (VK_MEDIA_NEXT_TRACK, "Next Track"),
    (VK_MEDIA_PREV_TRACK, "Previous Track"),
    (VK_MEDIA_STOP, "Stop Media"),
    (VK_MEDIA_PLAY_PAUSE, "Play/Pause"),
    (VK_OEM_1, ";"),
    (VK_OEM_PLUS, "="),
    (VK_OEM_COMMA, ","),
    (VK_OEM_MINUS, "-"),
    (VK_OEM_PERIOD, "."),
    (VK_OEM_2, "/"),
    (VK_OEM_3, "`"),
    (VK_OEM_4, "["),
    (VK_OEM_5, "\\"),
    (VK_OEM_6, "]"),
    (VK_OEM_7, "'"),
];

/// Resolve a non-modifier-category key name (case-insensitive) to a virtual-key code.
fn key_by_name(name: &str) -> Option<u32> {
    let n = name.to_ascii_lowercase();
    let mut chars = n.chars();
    if let (Some(c), None) = (chars.next(), chars.next()) {
        return match c {
            'a'..='z' => Some(c.to_ascii_uppercase() as u32),
            '0'..='9' => Some(c as u32),
            '`' => Some(VK_OEM_3),
            '-' => Some(VK_OEM_MINUS),
            '=' | '+' => Some(VK_OEM_PLUS),
            '[' => Some(VK_OEM_4),
            ']' => Some(VK_OEM_6),
            '\\' => Some(VK_OEM_5),
            ';' => Some(VK_OEM_1),
            '\'' => Some(VK_OEM_7),
            ',' => Some(VK_OEM_COMMA),
            '.' => Some(VK_OEM_PERIOD),
            '/' => Some(VK_OEM_2),
            _ => None,
        };
    }
    // F1..F24
    if let Some(num) = n.strip_prefix('f')
        && let Ok(i) = num.parse::<u32>()
        && (1..=24).contains(&i)
        && !num.starts_with('0')
        && !num.starts_with('+')
    {
        return Some(VK_F1 + i - 1);
    }
    // Numpad digits: num0..num9 / numpad0..numpad9
    for prefix in ["numpad", "num"] {
        if let Some(d) = n.strip_prefix(prefix)
            && d.len() == 1
            && let Some(i) = d.chars().next().and_then(|c| c.to_digit(10))
        {
            return Some(VK_NUMPAD0 + i);
        }
    }
    // Direct VK code (OpenWhispr accepted hex); 0x01..0xFE.
    if let Some(hex) = n.strip_prefix("0x") {
        return match u32::from_str_radix(hex, 16) {
            Ok(v) if (1..=0xFE).contains(&v) => Some(v),
            _ => None,
        };
    }
    Some(match n.as_str() {
        "space" | "spacebar" => VK_SPACE,
        "tab" => VK_TAB,
        "esc" | "escape" => VK_ESCAPE,
        "enter" | "return" => VK_RETURN,
        "backspace" => VK_BACK,
        "delete" | "del" => VK_DELETE,
        "insert" | "ins" => VK_INSERT,
        "home" => VK_HOME,
        "end" => VK_END,
        "pageup" | "pgup" | "prior" => VK_PRIOR,
        "pagedown" | "pgdn" | "pgdown" | "next" => VK_NEXT,
        "up" | "uparrow" => VK_UP,
        "down" | "downarrow" => VK_DOWN,
        "left" | "leftarrow" => VK_LEFT,
        "right" | "rightarrow" => VK_RIGHT,
        "capslock" => VK_CAPITAL,
        "numlock" => VK_NUMLOCK,
        "scrolllock" => VK_SCROLL,
        "pause" => VK_PAUSE,
        "printscreen" | "prtsc" | "snapshot" => VK_SNAPSHOT,
        "menu" | "apps" => VK_APPS,
        "numdec" | "numdecimal" => VK_DECIMAL,
        "numadd" | "numplus" => VK_ADD,
        "numsub" | "numminus" | "numsubtract" => VK_SUBTRACT,
        "nummult" | "nummultiply" => VK_MULTIPLY,
        "numdiv" | "numdivide" => VK_DIVIDE,
        "mediaplaypause" | "mediaplay" => VK_MEDIA_PLAY_PAUSE,
        "medianexttrack" | "medianext" => VK_MEDIA_NEXT_TRACK,
        "mediaprevioustrack" | "mediaprev" => VK_MEDIA_PREV_TRACK,
        "mediastop" => VK_MEDIA_STOP,
        "volumeup" => VK_VOLUME_UP,
        "volumedown" => VK_VOLUME_DOWN,
        "volumemute" => VK_VOLUME_MUTE,
        "backquote" | "backtick" | "grave" | "tilde" => VK_OEM_3,
        "minus" => VK_OEM_MINUS,
        "equal" | "equals" | "plus" => VK_OEM_PLUS,
        "leftbracket" | "lbracket" | "openbracket" => VK_OEM_4,
        "rightbracket" | "rbracket" | "closebracket" => VK_OEM_6,
        "backslash" => VK_OEM_5,
        "semicolon" => VK_OEM_1,
        "quote" | "apostrophe" => VK_OEM_7,
        "comma" => VK_OEM_COMMA,
        "period" | "dot" => VK_OEM_PERIOD,
        "slash" => VK_OEM_2,
        // Side-specific modifiers used as the main key.
        "rightalt" | "rightoption" | "ralt" => VK_RMENU,
        "leftalt" | "leftoption" | "lalt" => VK_LMENU,
        "rightcontrol" | "rightctrl" | "rctrl" => VK_RCONTROL,
        "leftcontrol" | "leftctrl" | "lctrl" => VK_LCONTROL,
        "rightshift" | "rshift" => VK_RSHIFT,
        "leftshift" | "lshift" => VK_LSHIFT,
        "rightsuper" | "rightwin" | "rightmeta" | "rightcommand" | "rightcmd" | "rwin" => VK_RWIN,
        "leftsuper" | "leftwin" | "leftmeta" | "leftcommand" | "leftcmd" | "lwin" => VK_LWIN,
        _ => return None,
    })
}

enum Modifier {
    Ctrl,
    Alt,
    Shift,
    Win,
}

fn modifier_by_name(name: &str) -> Option<Modifier> {
    Some(match name.to_ascii_lowercase().as_str() {
        "commandorcontrol" | "cmdorctrl" | "control" | "ctrl" => Modifier::Ctrl,
        "alt" | "option" => Modifier::Alt,
        "shift" => Modifier::Shift,
        "super" | "meta" | "win" | "windows" | "command" | "cmd" => Modifier::Win,
        _ => return None,
    })
}

/// Parse an Electron-style accelerator ("Control+Super", "CommandOrControl+Shift+Space", "F8",
/// "RightAlt", ...). Unknown keys are an error (unlike OpenWhispr's silent atoi fallback).
pub fn parse(spec: &str) -> Result<HotkeySpec, String> {
    let s = spec.trim();
    if s.is_empty() {
        return Err("empty hotkey".into());
    }
    // A trailing "+" key ("Ctrl++") would otherwise read as an empty token.
    let (head, plus_main) = if s == "+" {
        ("", true)
    } else if let Some(h) = s.strip_suffix("++") {
        (h, true)
    } else {
        (s, false)
    };

    let mut out = HotkeySpec::default();
    let mut main = if plus_main { Some(VK_OEM_PLUS) } else { None };
    if !head.is_empty() {
        for tok in head.split('+') {
            let t = tok.trim();
            if t.is_empty() {
                return Err(format!("empty key in hotkey '{s}'"));
            }
            match modifier_by_name(t) {
                Some(Modifier::Ctrl) => out.ctrl = true,
                Some(Modifier::Alt) => out.alt = true,
                Some(Modifier::Shift) => out.shift = true,
                Some(Modifier::Win) => out.win = true,
                None => {
                    let vk = key_by_name(t).ok_or_else(|| format!("unknown key '{t}' in hotkey '{s}'"))?;
                    if main.replace(vk).is_some() {
                        return Err(format!("hotkey '{s}' has more than one non-modifier key"));
                    }
                }
            }
        }
    }
    out.vk = main;
    Ok(out)
}

/// Display name of one virtual key.
fn vk_name(vk: u32) -> String {
    if let Some((_, n)) = KEY_LABELS.iter().find(|(k, _)| *k == vk) {
        return (*n).to_string();
    }
    match vk {
        0x30..=0x39 | 0x41..=0x5A => char::from_u32(vk).map(String::from).unwrap_or_default(),
        0x60..=0x69 => format!("Num {}", vk - VK_NUMPAD0),
        0x70..=0x87 => format!("F{}", vk - VK_F1 + 1),
        _ => format!("0x{vk:02X}"),
    }
}

/// Human label, e.g. "Ctrl + Win", "Ctrl + Shift + Space".
pub fn label(spec: &str) -> String {
    let Ok(p) = parse(spec) else {
        return spec.trim().to_string();
    };
    let mut parts: Vec<String> = Vec::with_capacity(5);
    if p.ctrl {
        parts.push("Ctrl".into());
    }
    if p.alt {
        parts.push("Alt".into());
    }
    if p.shift {
        parts.push("Shift".into());
    }
    if p.win {
        parts.push("Win".into());
    }
    if let Some(vk) = p.vk {
        parts.push(vk_name(vk));
    }
    parts.join(" + ")
}

// ---------------------------------------------------------------------------------------------
// Pure matcher
// ---------------------------------------------------------------------------------------------

/// Physical key state oracle (`GetAsyncKeyState` in production, a fake in tests).
pub trait KeyState {
    fn is_down(&self, vk: u32) -> bool;
}

/// `GetAsyncKeyState`-backed [`KeyState`].
pub struct SysKeys;

impl KeyState for SysKeys {
    fn is_down(&self, vk: u32) -> bool {
        // SAFETY: GetAsyncKeyState has no preconditions; any VK value is accepted.
        let s = unsafe { windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState(vk as i32) };
        (s as u16) & 0x8000 != 0
    }
}

const M_LCTRL: u8 = 1;
const M_RCTRL: u8 = 2;
const M_LALT: u8 = 4;
const M_RALT: u8 = 8;
const M_LSHIFT: u8 = 16;
const M_RSHIFT: u8 = 32;
const M_LWIN: u8 = 64;
const M_RWIN: u8 = 128;
const CTRL_MASK: u8 = M_LCTRL | M_RCTRL;
const ALT_MASK: u8 = M_LALT | M_RALT;
const SHIFT_MASK: u8 = M_LSHIFT | M_RSHIFT;
const WIN_MASK: u8 = M_LWIN | M_RWIN;

/// (bit, side-specific VK) pairs used to resync the modifier bitmask from the OS.
const MOD_KEYS: [(u8, u32); 8] = [
    (M_LCTRL, VK_LCONTROL),
    (M_RCTRL, VK_RCONTROL),
    (M_LALT, VK_LMENU),
    (M_RALT, VK_RMENU),
    (M_LSHIFT, VK_LSHIFT),
    (M_RSHIFT, VK_RSHIFT),
    (M_LWIN, VK_LWIN),
    (M_RWIN, VK_RWIN),
];

/// Modifier bit of a hook VK (generic VK_CONTROL/MENU/SHIFT count as the left key), else 0.
fn mod_bit(vk: u32) -> u8 {
    match vk {
        VK_CONTROL | VK_LCONTROL => M_LCTRL,
        VK_RCONTROL => M_RCTRL,
        VK_MENU | VK_LMENU => M_LALT,
        VK_RMENU => M_RALT,
        VK_SHIFT | VK_LSHIFT => M_LSHIFT,
        VK_RSHIFT => M_RSHIFT,
        VK_LWIN => M_LWIN,
        VK_RWIN => M_RWIN,
        _ => 0,
    }
}

/// What the hook must do for one key event. Fixed-size so the hook never allocates.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Outcome {
    events: [Option<HotkeyEvent>; 2],
    n: u8,
    /// Eat the key event (Esc while capture is on).
    pub swallow: bool,
    /// Inject the Start-menu mask key before this Win key-up is passed on.
    pub mask: bool,
}

impl Outcome {
    fn push(&mut self, ev: HotkeyEvent) {
        if let Some(slot) = self.events.get_mut(self.n as usize) {
            *slot = Some(ev);
            self.n += 1;
        }
    }

    pub fn events(&self) -> impl Iterator<Item = HotkeyEvent> + '_ {
        self.events.iter().flatten().copied()
    }
}

pub struct Matcher<K: KeyState = SysKeys> {
    keys: K,
    spec: HotkeySpec,
    /// Left/right modifier bitmask (`M_*`).
    mods: u8,
    /// A press is in flight (Down emitted, Up not yet).
    active: bool,
    suspended: bool,
    esc_capture: bool,
    /// Send the Start-menu mask for Win-containing combos.
    mask_enabled: bool,
    allow_injected: bool,
    /// The combo fired while a Win key was held; the Win key-up needs the mask.
    win_used: bool,
    /// The Esc key-down was swallowed, so its key-up (and repeats) must be too.
    esc_swallowed: bool,
    /// VK of the most recent non-modifier key-down (0 = none); modifiers count only when they
    /// are the main key. Windows auto-repeats only the last key pressed, so a main-key down after
    /// a different key-down cannot be a repeat. Modifiers are left out because AltGr repeats as
    /// alternating LCtrl/RAlt downs.
    last_down: u32,
}

impl<K: KeyState> Matcher<K> {
    pub const fn with_keys(keys: K, spec: HotkeySpec) -> Self {
        Self {
            keys,
            spec,
            mods: 0,
            active: false,
            suspended: false,
            esc_capture: false,
            mask_enabled: true,
            allow_injected: false,
            win_used: false,
            esc_swallowed: false,
            last_down: 0,
        }
    }

    pub fn spec(&self) -> &HotkeySpec {
        &self.spec
    }

    pub fn is_active(&self) -> bool {
        self.active
    }

    /// Replace the combo. Returns `Up` when a press was in flight (the caller posts it).
    pub fn set_spec(&mut self, spec: HotkeySpec) -> Option<HotkeyEvent> {
        self.spec = spec;
        self.reset()
    }

    /// While suspended every key passes through untouched. Returns `Up` if a press was cut short.
    pub fn set_suspended(&mut self, on: bool) -> Option<HotkeyEvent> {
        self.suspended = on;
        self.reset()
    }

    pub fn set_esc_capture(&mut self, on: bool) {
        self.esc_capture = on;
    }

    pub fn set_start_menu_mask(&mut self, on: bool) {
        self.mask_enabled = on;
    }

    pub fn set_allow_injected(&mut self, on: bool) {
        self.allow_injected = on;
    }

    /// Drop transient state and re-read the modifiers from the OS.
    fn reset(&mut self) -> Option<HotkeyEvent> {
        let was_active = std::mem::take(&mut self.active);
        self.win_used = false;
        self.esc_swallowed = false;
        self.last_down = 0;
        self.mods = 0;
        self.sync_mods(0);
        was_active.then_some(HotkeyEvent::Up)
    }

    /// Convenience wrapper over [`Self::step`]: `(events, swallow)`.
    pub fn on_key(&mut self, vk: u32, down: bool, injected: bool) -> (Vec<HotkeyEvent>, bool) {
        let o = self.step(vk, down, injected);
        (o.events().collect(), o.swallow)
    }

    /// Re-read modifier state from the OS for every modifier except the one in the current hook
    /// event (`GetAsyncKeyState` is unreliable for that one). Heals missed key-ups (Win+L etc).
    fn sync_mods(&mut self, current_vk: u32) {
        let skip = mod_bit(current_vk);
        for (bit, vk) in MOD_KEYS {
            if bit == skip {
                continue;
            }
            if self.keys.is_down(vk) {
                self.mods |= bit;
            } else {
                self.mods &= !bit;
            }
        }
    }

    fn required_ok(&self) -> bool {
        let s = &self.spec;
        (!s.ctrl || self.mods & CTRL_MASK != 0)
            && (!s.alt || self.mods & ALT_MASK != 0)
            && (!s.shift || self.mods & SHIFT_MASK != 0)
            && (!s.win || self.mods & WIN_MASK != 0)
    }

    /// Does the combo involve a Win key (so releasing it would open the Start menu)?
    fn combo_has_win(&self) -> bool {
        self.spec.win || matches!(self.spec.vk, Some(VK_LWIN | VK_RWIN))
    }

    /// Process one key event. O(1), allocation-free.
    pub fn step(&mut self, vk: u32, down: bool, injected: bool) -> Outcome {
        let mut out = Outcome::default();
        if self.suspended || (injected && !self.allow_injected) {
            return out;
        }
        let prev_down = self.last_down;
        if down && (mod_bit(vk) == 0 || self.spec.vk == Some(vk)) {
            self.last_down = vk;
        }

        // Esc: the key-up of a swallowed Esc is always eaten, even if capture turned off since.
        if vk == VK_ESCAPE && self.spec.vk != Some(VK_ESCAPE) {
            if down && self.esc_capture {
                if !self.esc_swallowed {
                    self.esc_swallowed = true;
                    out.push(HotkeyEvent::Escape);
                }
                out.swallow = true;
                return out;
            }
            if !down && self.esc_swallowed {
                self.esc_swallowed = false;
                out.swallow = true;
                return out;
            }
        }

        let bit = mod_bit(vk);
        if bit != 0 {
            if down {
                self.mods |= bit;
            } else {
                self.mods &= !bit;
            }
        }
        if bit != 0 || self.active || self.spec.has_modifiers() {
            self.sync_mods(vk);
        }

        if !self.spec.is_empty() {
            // A required modifier was released (or its key-up was missed): the press is over.
            if self.active && !self.required_ok() {
                self.active = false;
                out.push(HotkeyEvent::Up);
            }
            // Self-heal a missed main-key key-up (not valid for the key in this very event).
            if self.active
                && let Some(t) = self.spec.vk
                && vk != t
                && !self.keys.is_down(t)
            {
                self.active = false;
                out.push(HotkeyEvent::Up);
            }

            match self.spec.vk {
                None => {
                    // Modifier-only: any key-down while all required modifiers are held.
                    if down && !self.active && self.required_ok() {
                        self.active = true;
                        out.push(HotkeyEvent::Down);
                    }
                }
                Some(t) if vk == t => {
                    if down {
                        // Active and the last key down: auto-repeat, ignored. Active after some
                        // other key went down: the key-up was missed (e.g. released on the Win+L
                        // secure desktop), so this is a new press.
                        if self.active && prev_down != t {
                            self.active = false;
                            out.push(HotkeyEvent::Up);
                        }
                        if !self.active && self.required_ok() {
                            self.active = true;
                            out.push(HotkeyEvent::Down);
                        }
                    } else if self.active {
                        self.active = false;
                        out.push(HotkeyEvent::Up);
                    }
                }
                Some(_) => {}
            }

            if self.active && self.combo_has_win() && self.mods & WIN_MASK != 0 {
                self.win_used = true;
            }
        }

        // The Win key goes up after a Win combo fired: cancel the Start menu.
        if bit & WIN_MASK != 0 && !down && self.win_used && self.mask_enabled {
            out.mask = true;
        }
        if self.mods & WIN_MASK == 0 {
            self.win_used = false;
        }
        out
    }
}

// ---------------------------------------------------------------------------------------------
// Win32 shell
// ---------------------------------------------------------------------------------------------

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_KEYBOARD, KEYBD_EVENT_FLAGS, KEYBDINPUT, KEYEVENTF_KEYUP, SendInput, VIRTUAL_KEY,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DispatchMessageW, GetMessageW, HC_ACTION, KBDLLHOOKSTRUCT, LLKHF_INJECTED,
    LLKHF_LOWER_IL_INJECTED, MSG, PostMessageW, PostThreadMessageW, SetWindowsHookExW, TranslateMessage,
    UnhookWindowsHookEx, WH_KEYBOARD_LL, WM_KEYDOWN, WM_KEYUP, WM_QUIT, WM_SYSKEYDOWN, WM_SYSKEYUP,
};

/// `dwExtraInfo` stamped on our own mask key so the hook never reacts to it ("OTK1").
const OWN_MAGIC: usize = 0x4F54_4B31;

static MATCHER: Mutex<Matcher<SysKeys>> = Mutex::new(Matcher::with_keys(SysKeys, HotkeySpec::EMPTY));
static TARGET_HWND: AtomicIsize = AtomicIsize::new(0);
static TARGET_MSG: AtomicU32 = AtomicU32::new(0);
static HOOK_THREAD: AtomicU32 = AtomicU32::new(0);
/// Join handle of the running hook thread; also serialises `start`/`stop`.
static HOOK_JOIN: Mutex<Option<std::thread::JoinHandle<()>>> = Mutex::new(None);

fn lock_matcher() -> std::sync::MutexGuard<'static, Matcher<SysKeys>> {
    MATCHER.lock().unwrap_or_else(|p| p.into_inner())
}

/// Post one event to the daemon window. Allocation-free; safe to call from the hook.
fn post(ev: HotkeyEvent) {
    let hwnd = TARGET_HWND.load(Ordering::Relaxed);
    let msg = TARGET_MSG.load(Ordering::Relaxed);
    if msg == 0 {
        return;
    }
    // SAFETY: PostMessageW is thread-safe; a stale/invalid HWND only makes it fail.
    let _ = unsafe { PostMessageW(Some(HWND(hwnd as *mut _)), msg, WPARAM(ev as usize), LPARAM(0)) };
}

/// Inject VK 0xE8 down/up so Windows sees "another key was pressed" and skips the Start menu.
fn inject_mask() {
    let key = |flags: KEYBD_EVENT_FLAGS| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(VK_MASK as u16),
                wScan: 0,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: OWN_MAGIC,
            },
        },
    };
    let inputs = [key(KEYBD_EVENT_FLAGS(0)), key(KEYEVENTF_KEYUP)];
    // SAFETY: `inputs` is a valid slice of fully-initialised INPUT structs; size matches.
    unsafe { SendInput(&inputs, std::mem::size_of::<INPUT>() as i32) };
}

unsafe extern "system" fn hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 && lparam.0 != 0 {
        // SAFETY: for HC_ACTION the OS passes a valid KBDLLHOOKSTRUCT in lParam for the call.
        let kbd = unsafe { &*(lparam.0 as *const KBDLLHOOKSTRUCT) };
        let w = wparam.0 as u32;
        let down = w == WM_KEYDOWN || w == WM_SYSKEYDOWN;
        let up = w == WM_KEYUP || w == WM_SYSKEYUP;
        if (down || up) && kbd.dwExtraInfo != OWN_MAGIC {
            let injected = kbd.flags.0 & (LLKHF_INJECTED.0 | LLKHF_LOWER_IL_INJECTED.0) != 0;
            let out = match MATCHER.try_lock() {
                Ok(mut m) => m.step(kbd.vkCode, down, injected),
                Err(TryLockError::Poisoned(p)) => p.into_inner().step(kbd.vkCode, down, injected),
                // A setter holds the lock for microseconds; passing one key through is fine.
                Err(TryLockError::WouldBlock) => Outcome::default(),
            };
            for ev in out.events() {
                post(ev);
            }
            if out.mask {
                inject_mask();
            }
            if out.swallow {
                return LRESULT(1);
            }
        }
    }
    // SAFETY: forwarding the unmodified hook arguments.
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

fn hook_thread_main(ready: std::sync::mpsc::Sender<Result<(), String>>) {
    // SAFETY: standard LL hook installation; the callback is a valid `extern "system"` fn and
    // this thread pumps messages for as long as the hook lives.
    let hook = match unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(hook_proc), None, 0) } {
        Ok(h) => h,
        Err(e) => {
            let _ = ready.send(Err(format!("SetWindowsHookExW failed: {e}")));
            return;
        }
    };
    // SAFETY: GetCurrentThreadId has no preconditions.
    let tid = unsafe { windows::Win32::System::Threading::GetCurrentThreadId() };
    HOOK_THREAD.store(tid, Ordering::SeqCst);
    let _ = ready.send(Ok(()));
    let mut msg = MSG::default();
    // SAFETY: plain message loop on the thread that owns the hook.
    unsafe {
        while GetMessageW(&mut msg, None, 0, 0).0 > 0 {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        let _ = UnhookWindowsHookEx(hook);
    }
    HOOK_THREAD.store(0, Ordering::SeqCst);
}

/// Install the hook thread. Events are posted as `PostMessageW(hwnd, msg, HotkeyEvent as usize, 0)`.
pub fn start(hwnd: isize, msg: u32, spec: &str) -> Result<(), String> {
    let parsed = parse(spec)?;
    // Held for the whole call so a concurrent `stop`/`start` cannot see a half-started thread.
    let mut slot = HOOK_JOIN.lock().unwrap_or_else(|p| p.into_inner());
    {
        let mut m = lock_matcher();
        if std::env::var("OPENTALK_ALLOW_INJECTED").is_ok_and(|v| v == "1") {
            m.set_allow_injected(true);
        }
        TARGET_HWND.store(hwnd, Ordering::SeqCst);
        TARGET_MSG.store(msg, Ordering::SeqCst);
        m.set_spec(parsed);
    }
    if slot.as_ref().is_some_and(|h| !h.is_finished()) {
        return Ok(());
    }
    // A previous thread that died on its own: reap it before starting a new one.
    if let Some(dead) = slot.take() {
        let _ = dead.join();
    }
    let (tx, rx) = std::sync::mpsc::channel();
    let handle = std::thread::Builder::new()
        .name("hotkey-hook".into())
        .spawn(move || hook_thread_main(tx))
        .map_err(|e| format!("cannot spawn hook thread: {e}"))?;
    match rx.recv().unwrap_or_else(|_| Err("hook thread exited early".into())) {
        Ok(()) => {
            *slot = Some(handle);
            Ok(())
        }
        Err(e) => {
            let _ = handle.join();
            Err(e)
        }
    }
}

/// Remove the hook and end its thread (the daemon normally just exits instead). Returns once the
/// hook is gone, so an immediate `start` cannot race the old thread's shutdown.
pub fn stop() {
    let mut slot = HOOK_JOIN.lock().unwrap_or_else(|p| p.into_inner());
    let Some(handle) = slot.take() else { return };
    let tid = HOOK_THREAD.load(Ordering::SeqCst);
    if tid != 0 {
        // SAFETY: posting WM_QUIT to a thread id; failure just means the thread is gone.
        let _ = unsafe { PostThreadMessageW(tid, WM_QUIT, WPARAM(0), LPARAM(0)) };
    }
    let _ = handle.join();
}

pub fn set_hotkey(spec: &str) -> Result<(), String> {
    let parsed = parse(spec)?;
    let cut = lock_matcher().set_spec(parsed);
    if let Some(ev) = cut {
        post(ev);
    }
    Ok(())
}

pub fn set_esc_capture(on: bool) {
    lock_matcher().set_esc_capture(on);
}

pub fn set_suspended(on: bool) {
    let cut = lock_matcher().set_suspended(on);
    if let Some(ev) = cut {
        post(ev);
    }
}

pub fn set_start_menu_mask(on: bool) {
    lock_matcher().set_start_menu_mask(on);
}

/// Let automation-injected (`SendInput`) keys through; production ignores them.
pub fn set_allow_injected(on: bool) {
    lock_matcher().set_allow_injected(on);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::HashSet;
    use std::rc::Rc;

    type Down = Rc<RefCell<HashSet<u32>>>;

    struct Fake(Down);
    impl KeyState for Fake {
        fn is_down(&self, vk: u32) -> bool {
            self.0.borrow().contains(&vk)
        }
    }

    struct H {
        m: Matcher<Fake>,
        down: Down,
    }

    fn harness(spec: &str) -> H {
        let down: Down = Rc::default();
        let m = Matcher::with_keys(Fake(down.clone()), parse(spec).unwrap());
        H { m, down }
    }

    use HotkeyEvent::{Down as D, Escape as E, Up as U};

    impl H {
        /// Physical press: the OS key state updates after the hook has seen the event.
        fn press(&mut self, vk: u32) -> (Vec<HotkeyEvent>, bool) {
            let r = self.m.on_key(vk, true, false);
            self.down.borrow_mut().insert(vk);
            r
        }
        fn release(&mut self, vk: u32) -> (Vec<HotkeyEvent>, bool) {
            let r = self.m.on_key(vk, false, false);
            self.down.borrow_mut().remove(&vk);
            r
        }
        fn press_ev(&mut self, vk: u32) -> Vec<HotkeyEvent> {
            self.press(vk).0
        }
        fn release_ev(&mut self, vk: u32) -> Vec<HotkeyEvent> {
            self.release(vk).0
        }
        fn step_full(&mut self, vk: u32, down: bool) -> Outcome {
            let o = self.m.step(vk, down, false);
            if down {
                self.down.borrow_mut().insert(vk);
            } else {
                self.down.borrow_mut().remove(&vk);
            }
            o
        }
    }

    // ---- parse / label ----

    fn p(s: &str) -> HotkeySpec {
        parse(s).unwrap_or_else(|e| panic!("{s}: {e}"))
    }

    #[test]
    fn parse_modifier_aliases() {
        for s in ["CommandOrControl", "CmdOrCtrl", "Ctrl", "Control", "control", "CTRL"] {
            assert_eq!(p(s), HotkeySpec { ctrl: true, ..Default::default() }, "{s}");
        }
        for s in ["Alt", "Option"] {
            assert!(p(s).alt && p(s).vk.is_none(), "{s}");
        }
        assert!(p("Shift").shift);
        for s in ["Super", "Meta", "Win", "Command", "Cmd", "Windows"] {
            assert!(p(s).win, "{s}");
        }
    }

    #[test]
    fn parse_combos() {
        assert_eq!(p("Control+Super"), HotkeySpec { ctrl: true, win: true, ..Default::default() });
        assert_eq!(p("Super+Control"), p("Control+Super"));
        assert_eq!(
            p("CommandOrControl+Shift+Space"),
            HotkeySpec { ctrl: true, shift: true, vk: Some(VK_SPACE), ..Default::default() }
        );
        assert_eq!(p("Ctrl + Alt + F11").vk, Some(0x7A));
        assert_eq!(p("F8"), HotkeySpec { vk: Some(0x77), ..Default::default() });
    }

    #[test]
    fn parse_function_keys() {
        for i in 1..=24u32 {
            assert_eq!(p(&format!("F{i}")).vk, Some(0x6F + i), "F{i}");
        }
        assert!(parse("F0").is_err());
        assert!(parse("F25").is_err());
        assert!(parse("F01").is_err());
    }

    #[test]
    fn parse_letters_digits_punct() {
        assert_eq!(p("a").vk, Some(b'A' as u32));
        assert_eq!(p("Z").vk, Some(b'Z' as u32));
        assert_eq!(p("7").vk, Some(b'7' as u32));
        assert_eq!(p("`").vk, Some(VK_OEM_3));
        assert_eq!(p("Backquote").vk, Some(VK_OEM_3));
        assert_eq!(p("-").vk, Some(VK_OEM_MINUS));
        assert_eq!(p("=").vk, Some(VK_OEM_PLUS));
        assert_eq!(p("[").vk, Some(VK_OEM_4));
        assert_eq!(p("]").vk, Some(VK_OEM_6));
        assert_eq!(p("\\").vk, Some(VK_OEM_5));
        assert_eq!(p(";").vk, Some(VK_OEM_1));
        assert_eq!(p("'").vk, Some(VK_OEM_7));
        assert_eq!(p(",").vk, Some(VK_OEM_COMMA));
        assert_eq!(p(".").vk, Some(VK_OEM_PERIOD));
        assert_eq!(p("/").vk, Some(VK_OEM_2));
        assert_eq!(p("Ctrl+`").vk, Some(VK_OEM_3));
        // A "+" main key.
        assert_eq!(p("Ctrl++"), HotkeySpec { ctrl: true, vk: Some(VK_OEM_PLUS), ..Default::default() });
        assert_eq!(p("+").vk, Some(VK_OEM_PLUS));
    }

    #[test]
    fn parse_special_keys() {
        let cases = [
            ("Space", VK_SPACE),
            ("Tab", VK_TAB),
            ("Esc", VK_ESCAPE),
            ("Escape", VK_ESCAPE),
            ("Pause", VK_PAUSE),
            ("ScrollLock", VK_SCROLL),
            ("Insert", VK_INSERT),
            ("Home", VK_HOME),
            ("End", VK_END),
            ("PageUp", VK_PRIOR),
            ("PageDown", VK_NEXT),
            ("CapsLock", VK_CAPITAL),
            ("NumLock", VK_NUMLOCK),
            ("Up", VK_UP),
            ("Down", VK_DOWN),
            ("Left", VK_LEFT),
            ("Right", VK_RIGHT),
            ("Enter", VK_RETURN),
            ("Backspace", VK_BACK),
            ("Delete", VK_DELETE),
            ("PrintScreen", VK_SNAPSHOT),
            ("Num0", 0x60),
            ("Num9", 0x69),
            ("numadd", VK_ADD),
            ("numsub", VK_SUBTRACT),
            ("nummult", VK_MULTIPLY),
            ("numdiv", VK_DIVIDE),
            ("numdec", VK_DECIMAL),
            ("MediaPlayPause", VK_MEDIA_PLAY_PAUSE),
            ("MediaNextTrack", VK_MEDIA_NEXT_TRACK),
            ("MediaPreviousTrack", VK_MEDIA_PREV_TRACK),
            ("MediaStop", VK_MEDIA_STOP),
            ("VolumeUp", VK_VOLUME_UP),
            ("VolumeDown", VK_VOLUME_DOWN),
            ("VolumeMute", VK_VOLUME_MUTE),
            ("0x75", 0x75),
        ];
        for (s, vk) in cases {
            assert_eq!(p(s).vk, Some(vk), "{s}");
        }
    }

    #[test]
    fn parse_right_side_keys() {
        assert_eq!(p("RightAlt").vk, Some(VK_RMENU));
        assert_eq!(p("RightOption").vk, Some(VK_RMENU));
        assert_eq!(p("RightControl").vk, Some(VK_RCONTROL));
        assert_eq!(p("RightCtrl").vk, Some(VK_RCONTROL));
        assert_eq!(p("RightShift").vk, Some(VK_RSHIFT));
        for s in ["RightSuper", "RightWin", "RightMeta", "RightCommand", "RightCmd"] {
            assert_eq!(p(s).vk, Some(VK_RWIN), "{s}");
        }
        // "Right" alone is the arrow key, not a modifier prefix.
        assert_eq!(p("Right").vk, Some(VK_RIGHT));
        assert_eq!(p("Ctrl+RightAlt"), HotkeySpec { ctrl: true, vk: Some(VK_RMENU), ..Default::default() });
    }

    #[test]
    fn parse_rejects_bad_input() {
        for s in ["", "  ", "Ctrl+", "+Ctrl", "Ctrl+Foo", "Foo", "A+B", "Ctrl+F1+F2", "0x00", "0x1FF", "0xZZ", "Ctrl+Shift+"] {
            assert!(parse(s).is_err(), "{s:?} should be rejected");
        }
    }

    #[test]
    fn labels() {
        assert_eq!(label("Control+Super"), "Ctrl + Win");
        assert_eq!(label("Super+Control"), "Ctrl + Win");
        assert_eq!(label("CommandOrControl+Shift+Space"), "Ctrl + Shift + Space");
        assert_eq!(label("F8"), "F8");
        assert_eq!(label("ctrl+alt+shift+win+a"), "Ctrl + Alt + Shift + Win + A");
        assert_eq!(label("RightAlt"), "Right Alt");
        assert_eq!(label("Ctrl+`"), "Ctrl + `");
        assert_eq!(label("Ctrl+Num5"), "Ctrl + Num 5");
        assert_eq!(label("not a key"), "not a key");
    }

    // ---- modifier-only combos ----

    #[test]
    fn modifier_only_any_order() {
        let mut h = harness("Control+Super");
        assert!(h.press_ev(VK_LCONTROL).is_empty());
        assert_eq!(h.press_ev(VK_LWIN), vec![D]);
        assert_eq!(h.release_ev(VK_LCONTROL), vec![U]);
        assert!(h.release_ev(VK_LWIN).is_empty());

        // Win first, then Ctrl.
        assert!(h.press_ev(VK_LWIN).is_empty());
        assert_eq!(h.press_ev(VK_LCONTROL), vec![D]);
        assert_eq!(h.release_ev(VK_LWIN), vec![U]);
        assert!(h.release_ev(VK_LCONTROL).is_empty());
    }

    #[test]
    fn modifier_only_right_side_keys() {
        let mut h = harness("Ctrl+Win");
        h.press_ev(VK_RCONTROL);
        assert_eq!(h.press_ev(VK_RWIN), vec![D]);
        assert_eq!(h.release_ev(VK_RWIN), vec![U]);
        h.release_ev(VK_RCONTROL);
        // Mixed sides.
        h.press_ev(VK_LCONTROL);
        assert_eq!(h.press_ev(VK_RWIN), vec![D]);
    }

    #[test]
    fn modifier_only_superset_modifiers() {
        let mut h = harness("Control+Super");
        h.press_ev(VK_LSHIFT);
        h.press_ev(VK_LCONTROL);
        assert_eq!(h.press_ev(VK_LWIN), vec![D]);
        // Shift (not required) released: still active.
        assert!(h.release_ev(VK_LSHIFT).is_empty());
        assert!(h.m.is_active());
        assert_eq!(h.release_ev(VK_LCONTROL), vec![U]);
    }

    #[test]
    fn modifier_only_not_enough_modifiers() {
        let mut h = harness("Control+Super");
        assert!(h.press_ev(VK_LCONTROL).is_empty());
        assert!(h.press_ev(b'C' as u32).is_empty());
        assert!(h.release_ev(b'C' as u32).is_empty());
        assert!(h.press_ev(VK_LSHIFT).is_empty());
        assert!(!h.m.is_active());
    }

    #[test]
    fn modifier_only_single_down_per_press() {
        let mut h = harness("Control+Super");
        h.press_ev(VK_LCONTROL);
        assert_eq!(h.press_ev(VK_LWIN), vec![D]);
        // Further keys while held do not re-fire.
        assert!(h.press_ev(VK_LSHIFT).is_empty());
        assert!(h.press_ev(b'A' as u32).is_empty());
        // Auto-repeat of the Win key (down again).
        assert!(h.press_ev(VK_LWIN).is_empty());
    }

    #[test]
    fn modifier_only_refires_after_release() {
        let mut h = harness("Alt+Shift");
        h.press_ev(VK_LMENU);
        assert_eq!(h.press_ev(VK_LSHIFT), vec![D]);
        assert_eq!(h.release_ev(VK_LSHIFT), vec![U]);
        assert_eq!(h.press_ev(VK_LSHIFT), vec![D]);
    }

    #[test]
    fn single_modifier_combo() {
        let mut h = harness("Alt");
        assert_eq!(h.press_ev(VK_LMENU), vec![D]);
        assert_eq!(h.release_ev(VK_LMENU), vec![U]);
        // Right Alt also counts.
        assert_eq!(h.press_ev(VK_RMENU), vec![D]);
    }

    // ---- compound keys ----

    #[test]
    fn compound_key_down_up() {
        let mut h = harness("Control+Shift+Space");
        assert!(h.press_ev(VK_LCONTROL).is_empty());
        assert!(h.press_ev(VK_LSHIFT).is_empty());
        assert_eq!(h.press_ev(VK_SPACE), vec![D]);
        assert_eq!(h.release_ev(VK_SPACE), vec![U]);
        assert!(h.release_ev(VK_LSHIFT).is_empty());
        assert!(h.release_ev(VK_LCONTROL).is_empty());
    }

    #[test]
    fn compound_key_requires_all_modifiers() {
        let mut h = harness("Control+Shift+Space");
        h.press_ev(VK_LCONTROL);
        assert!(h.press_ev(VK_SPACE).is_empty());
        assert!(h.release_ev(VK_SPACE).is_empty());
        // Space alone.
        h.release_ev(VK_LCONTROL);
        assert!(h.press_ev(VK_SPACE).is_empty());
    }

    #[test]
    fn compound_key_superset_modifiers() {
        let mut h = harness("Control+Space");
        h.press_ev(VK_LCONTROL);
        h.press_ev(VK_LMENU);
        assert_eq!(h.press_ev(VK_SPACE), vec![D]);
    }

    #[test]
    fn compound_key_modifier_first_release_ends_press_once() {
        let mut h = harness("Control+Space");
        h.press_ev(VK_LCONTROL);
        assert_eq!(h.press_ev(VK_SPACE), vec![D]);
        assert_eq!(h.release_ev(VK_LCONTROL), vec![U]);
        // Space up afterwards must not emit a second Up.
        assert!(h.release_ev(VK_SPACE).is_empty());
    }

    #[test]
    fn compound_key_other_key_does_not_match() {
        let mut h = harness("Control+Space");
        h.press_ev(VK_LCONTROL);
        assert!(h.press_ev(b'A' as u32).is_empty());
        assert!(h.release_ev(b'A' as u32).is_empty());
    }

    #[test]
    fn plain_function_key_any_modifiers() {
        let mut h = harness("F8");
        assert_eq!(h.press_ev(0x77), vec![D]);
        assert_eq!(h.release_ev(0x77), vec![U]);
        // Superset: with modifiers held it still fires.
        h.press_ev(VK_LCONTROL);
        assert_eq!(h.press_ev(0x77), vec![D]);
        assert_eq!(h.release_ev(0x77), vec![U]);
        // Other keys don't.
        assert!(h.press_ev(0x78).is_empty());
    }

    #[test]
    fn auto_repeat_ignored() {
        let mut h = harness("F8");
        assert_eq!(h.press_ev(0x77), vec![D]);
        for _ in 0..10 {
            assert!(h.press_ev(0x77).is_empty());
        }
        assert_eq!(h.release_ev(0x77), vec![U]);
        assert!(h.release_ev(0x77).is_empty());
    }

    #[test]
    fn main_key_not_swallowed() {
        let mut h = harness("F8");
        assert!(!h.press(0x77).1);
        assert!(!h.release(0x77).1);
        let mut h = harness("Control+Shift+Space");
        h.press_ev(VK_LCONTROL);
        h.press_ev(VK_LSHIFT);
        assert!(!h.press(VK_SPACE).1);
    }

    #[test]
    fn right_side_main_key() {
        let mut h = harness("RightAlt");
        // Left Alt is a different key.
        assert!(h.press_ev(VK_LMENU).is_empty());
        assert!(h.release_ev(VK_LMENU).is_empty());
        assert_eq!(h.press_ev(VK_RMENU), vec![D]);
        assert!(h.press_ev(VK_RMENU).is_empty());
        assert_eq!(h.release_ev(VK_RMENU), vec![U]);

        let mut h = harness("RightControl");
        assert!(h.press_ev(VK_LCONTROL).is_empty());
        assert_eq!(h.press_ev(VK_RCONTROL), vec![D]);
        assert_eq!(h.release_ev(VK_RCONTROL), vec![U]);

        let mut h = harness("RightShift");
        assert_eq!(h.press_ev(VK_RSHIFT), vec![D]);
        assert_eq!(h.release_ev(VK_RSHIFT), vec![U]);

        let mut h = harness("RightWin");
        assert!(h.press_ev(VK_LWIN).is_empty());
        assert_eq!(h.press_ev(VK_RWIN), vec![D]);
        assert_eq!(h.release_ev(VK_RWIN), vec![U]);
    }

    #[test]
    fn right_side_main_key_with_modifier() {
        let mut h = harness("Ctrl+RightAlt");
        assert!(h.press_ev(VK_RMENU).is_empty());
        h.release_ev(VK_RMENU);
        h.press_ev(VK_LCONTROL);
        assert_eq!(h.press_ev(VK_RMENU), vec![D]);
        assert_eq!(h.release_ev(VK_LCONTROL), vec![U]);
    }

    #[test]
    fn generic_modifier_vks_count() {
        // Some synthetic sources report VK_CONTROL/VK_MENU/VK_SHIFT instead of the L/R codes.
        let mut h = harness("Control+Shift+Space");
        h.m.on_key(VK_CONTROL, true, false);
        h.down.borrow_mut().insert(VK_LCONTROL);
        h.m.on_key(VK_SHIFT, true, false);
        h.down.borrow_mut().insert(VK_LSHIFT);
        assert_eq!(h.press_ev(VK_SPACE), vec![D]);
    }

    // ---- self-heal ----

    #[test]
    fn heals_missed_main_key_up() {
        let mut h = harness("F8");
        assert_eq!(h.press_ev(0x77), vec![D]);
        // The F8 key-up was never delivered; the OS says F8 is up.
        h.down.borrow_mut().remove(&0x77);
        assert_eq!(h.press_ev(b'A' as u32), vec![U]);
        // A new F8 press works.
        assert_eq!(h.press_ev(0x77), vec![D]);
    }

    #[test]
    fn missed_main_key_up_then_same_key_is_a_new_press() {
        let mut h = harness("F8");
        assert_eq!(h.press_ev(0x77), vec![D]);
        // Win+L while F8 is held; F8, Win and L are released on the secure desktop (unseen).
        h.press_ev(VK_LWIN);
        h.press_ev(b'L' as u32);
        h.down.borrow_mut().clear();
        // The first event after unlock is the next F8 press: it ends the stale press and starts anew.
        assert_eq!(h.press_ev(0x77), vec![U, D]);
        assert!(h.press_ev(0x77).is_empty(), "auto-repeat");
        assert_eq!(h.release_ev(0x77), vec![U]);
    }

    #[test]
    fn altgr_repeat_is_not_a_new_press() {
        // Holding AltGr repeats as LCtrl, RAlt, LCtrl, RAlt, ...
        let mut h = harness("RightAlt");
        h.press_ev(VK_LCONTROL);
        assert_eq!(h.press_ev(VK_RMENU), vec![D]);
        for _ in 0..3 {
            assert!(h.press_ev(VK_LCONTROL).is_empty());
            assert!(h.press_ev(VK_RMENU).is_empty());
        }
        h.release_ev(VK_LCONTROL);
        assert_eq!(h.release_ev(VK_RMENU), vec![U]);
    }

    #[test]
    fn repeat_survives_another_keys_release() {
        let mut h = harness("F8");
        h.press_ev(VK_LSHIFT);
        assert_eq!(h.press_ev(0x77), vec![D]);
        assert!(h.release_ev(VK_LSHIFT).is_empty());
        assert!(h.press_ev(0x77).is_empty(), "still auto-repeat: no other key went down");
        assert_eq!(h.release_ev(0x77), vec![U]);
    }

    #[test]
    fn heals_missed_modifier_up_for_modifier_only() {
        let mut h = harness("Control+Super");
        h.press_ev(VK_LCONTROL);
        assert_eq!(h.press_ev(VK_LWIN), vec![D]);
        // Win+L style: Win key-up missed, OS now reports it up.
        h.down.borrow_mut().remove(&VK_LWIN);
        assert_eq!(h.press_ev(b'X' as u32), vec![U]);
        assert!(!h.m.is_active());
    }

    #[test]
    fn stale_modifier_does_not_fire_after_missed_up() {
        let mut h = harness("Control+Super");
        // Win goes down and its key-up is lost (e.g. Win+L lock).
        h.press_ev(VK_LWIN);
        h.down.borrow_mut().remove(&VK_LWIN);
        // Later the user presses Ctrl alone: stale Win must not complete the combo.
        assert!(h.press_ev(VK_LCONTROL).is_empty());
        assert!(h.press_ev(b'C' as u32).is_empty());
        assert!(!h.m.is_active());
    }

    #[test]
    fn heals_missed_modifier_up_for_compound() {
        let mut h = harness("Control+Space");
        h.press_ev(VK_LCONTROL);
        assert_eq!(h.press_ev(VK_SPACE), vec![D]);
        h.down.borrow_mut().remove(&VK_LCONTROL);
        // Space auto-repeat arrives, Ctrl is physically up.
        assert_eq!(h.press_ev(VK_SPACE), vec![U]);
        assert!(!h.m.is_active());
    }

    // ---- Esc ----

    #[test]
    fn esc_passes_through_when_capture_off() {
        let mut h = harness("Control+Super");
        let (ev, swallow) = h.press(VK_ESCAPE);
        assert!(ev.is_empty());
        assert!(!swallow);
        assert!(!h.release(VK_ESCAPE).1);
    }

    #[test]
    fn esc_swallowed_when_capture_on() {
        let mut h = harness("Control+Super");
        h.m.set_esc_capture(true);
        let (ev, swallow) = h.press(VK_ESCAPE);
        assert_eq!(ev, vec![E]);
        assert!(swallow);
        // Auto-repeat: swallowed, no second event.
        let (ev, swallow) = h.press(VK_ESCAPE);
        assert!(ev.is_empty());
        assert!(swallow);
        // Key-up is swallowed too.
        let (ev, swallow) = h.release(VK_ESCAPE);
        assert!(ev.is_empty());
        assert!(swallow);
        // Next Esc press reports again.
        assert_eq!(h.press(VK_ESCAPE).0, vec![E]);
    }

    #[test]
    fn esc_up_swallowed_even_if_capture_turned_off_between() {
        let mut h = harness("F8");
        h.m.set_esc_capture(true);
        assert!(h.press(VK_ESCAPE).1);
        h.m.set_esc_capture(false);
        assert!(h.release(VK_ESCAPE).1);
        // Unrelated later Esc passes.
        assert!(!h.press(VK_ESCAPE).1);
        assert!(!h.release(VK_ESCAPE).1);
    }

    #[test]
    fn esc_capture_does_not_disturb_active_press() {
        let mut h = harness("Control+Super");
        h.m.set_esc_capture(true);
        h.press_ev(VK_LCONTROL);
        assert_eq!(h.press_ev(VK_LWIN), vec![D]);
        assert_eq!(h.press(VK_ESCAPE), (vec![E], true));
        assert!(h.m.is_active());
    }

    // ---- injected events ----

    #[test]
    fn injected_events_ignored_by_default() {
        let mut m = Matcher::with_keys(Fake(Rc::default()), p("F8"));
        assert_eq!(m.on_key(0x77, true, true), (vec![], false));
        assert_eq!(m.on_key(0x77, false, true), (vec![], false));
        m.set_esc_capture(true);
        assert_eq!(m.on_key(VK_ESCAPE, true, true), (vec![], false));
    }

    #[test]
    fn injected_events_allowed_when_enabled() {
        let mut m = Matcher::with_keys(Fake(Rc::default()), p("F8"));
        m.set_allow_injected(true);
        assert_eq!(m.on_key(0x77, true, true).0, vec![D]);
        assert_eq!(m.on_key(0x77, false, true).0, vec![U]);
    }

    #[test]
    fn injected_modifiers_do_not_arm_a_combo() {
        let down: Down = Rc::default();
        let mut m = Matcher::with_keys(Fake(down.clone()), p("Control+Space"));
        // Injected Ctrl is ignored by the tracker; the OS state is empty so nothing is held.
        m.on_key(VK_LCONTROL, true, true);
        assert!(m.on_key(VK_SPACE, true, false).0.is_empty());
    }

    // ---- Start-menu mask ----

    #[test]
    fn mask_sent_before_win_up_ctrl_first() {
        let mut h = harness("Control+Super");
        assert!(!h.step_full(VK_LCONTROL, true).mask);
        let o = h.step_full(VK_LWIN, true);
        assert_eq!(o.events().collect::<Vec<_>>(), vec![D]);
        assert!(!o.mask);
        // Win released first: the mask goes out with that key-up, which still passes.
        let o = h.step_full(VK_LWIN, false);
        assert!(o.mask);
        assert!(!o.swallow);
        assert_eq!(o.events().collect::<Vec<_>>(), vec![U]);
        let o = h.step_full(VK_LCONTROL, false);
        assert!(!o.mask);
    }

    #[test]
    fn mask_sent_when_ctrl_released_before_win() {
        let mut h = harness("Control+Super");
        h.step_full(VK_LWIN, true);
        h.step_full(VK_LCONTROL, true);
        let o = h.step_full(VK_LCONTROL, false);
        assert!(!o.mask);
        assert_eq!(o.events().collect::<Vec<_>>(), vec![U]);
        // Combo already ended, but Win is still the same press: mask on its release.
        assert!(h.step_full(VK_LWIN, false).mask);
    }

    #[test]
    fn mask_applies_to_right_win_and_win_main_key() {
        let mut h = harness("Control+Super");
        h.step_full(VK_LCONTROL, true);
        h.step_full(VK_RWIN, true);
        assert!(h.step_full(VK_RWIN, false).mask);

        let mut h = harness("RightWin");
        h.step_full(VK_RWIN, true);
        assert!(h.step_full(VK_RWIN, false).mask);

        let mut h = harness("Win+Space");
        h.step_full(VK_LWIN, true);
        assert_eq!(h.step_full(VK_SPACE, true).events().collect::<Vec<_>>(), vec![D]);
        h.step_full(VK_SPACE, false);
        assert!(h.step_full(VK_LWIN, false).mask);
    }

    #[test]
    fn mask_not_sent_for_plain_win_tap() {
        let mut h = harness("Control+Super");
        assert!(!h.step_full(VK_LWIN, true).mask);
        // Plain Win tap must still open the Start menu.
        assert!(!h.step_full(VK_LWIN, false).mask);
    }

    #[test]
    fn mask_not_sent_for_combo_without_win() {
        let mut h = harness("Control+Shift+Space");
        h.step_full(VK_LWIN, true);
        h.step_full(VK_LCONTROL, true);
        h.step_full(VK_LSHIFT, true);
        h.step_full(VK_SPACE, true);
        h.step_full(VK_SPACE, false);
        assert!(!h.step_full(VK_LWIN, false).mask);
    }

    #[test]
    fn mask_state_resets_between_win_presses() {
        let mut h = harness("Control+Super");
        h.step_full(VK_LCONTROL, true);
        h.step_full(VK_LWIN, true);
        assert!(h.step_full(VK_LWIN, false).mask);
        h.step_full(VK_LCONTROL, false);
        // Next Win tap on its own is not masked.
        h.step_full(VK_LWIN, true);
        assert!(!h.step_full(VK_LWIN, false).mask);
    }

    #[test]
    fn mask_can_be_disabled() {
        let mut h = harness("Control+Super");
        h.m.set_start_menu_mask(false);
        h.step_full(VK_LCONTROL, true);
        h.step_full(VK_LWIN, true);
        assert!(!h.step_full(VK_LWIN, false).mask);
    }

    #[test]
    fn mask_stale_state_cleared_after_missed_win_up() {
        let mut h = harness("Control+Super");
        h.step_full(VK_LCONTROL, true);
        h.step_full(VK_LWIN, true);
        // Win key-up missed entirely.
        h.down.borrow_mut().remove(&VK_LWIN);
        h.step_full(VK_LCONTROL, false);
        h.step_full(b'A' as u32, true);
        h.step_full(b'A' as u32, false);
        // A later plain Win tap is not masked.
        h.step_full(VK_LWIN, true);
        assert!(!h.step_full(VK_LWIN, false).mask);
    }

    // ---- suspension / reconfiguration ----

    #[test]
    fn suspended_ignores_everything() {
        let mut h = harness("F8");
        h.m.set_esc_capture(true);
        assert_eq!(h.m.set_suspended(true), None);
        assert_eq!(h.press(0x77), (vec![], false));
        assert_eq!(h.press(VK_ESCAPE), (vec![], false));
        assert_eq!(h.release(0x77), (vec![], false));
        h.release(VK_ESCAPE);
        assert_eq!(h.m.set_suspended(false), None);
        assert_eq!(h.press_ev(0x77), vec![D]);
    }

    #[test]
    fn suspending_mid_press_emits_up() {
        let mut h = harness("F8");
        assert_eq!(h.press_ev(0x77), vec![D]);
        assert_eq!(h.m.set_suspended(true), Some(U));
        assert!(!h.m.is_active());
        h.release(0x77);
        h.m.set_suspended(false);
        assert_eq!(h.press_ev(0x77), vec![D]);
    }

    #[test]
    fn resume_resyncs_modifiers() {
        let mut h = harness("Control+Super");
        h.m.set_suspended(true);
        // Modifiers pressed while suspended are picked up on resume.
        h.press(VK_LCONTROL);
        h.press(VK_LWIN);
        h.m.set_suspended(false);
        assert_eq!(h.press_ev(b'A' as u32), vec![D]);
    }

    #[test]
    fn set_spec_mid_press_emits_up_and_applies() {
        let mut h = harness("F8");
        assert_eq!(h.press_ev(0x77), vec![D]);
        assert_eq!(h.m.set_spec(p("F9")), Some(U));
        h.release(0x77);
        assert!(h.press_ev(0x77).is_empty());
        assert_eq!(h.press_ev(0x78), vec![D]);
    }

    #[test]
    fn empty_spec_never_fires() {
        let mut m = Matcher::with_keys(Fake(Rc::default()), HotkeySpec::default());
        assert_eq!(m.on_key(0x41, true, false), (vec![], false));
        assert_eq!(m.on_key(VK_LCONTROL, true, false), (vec![], false));
    }

    #[test]
    fn outcome_is_small_and_copy() {
        // The hook relies on this being a plain value (no heap).
        fn assert_copy<T: Copy>() {}
        assert_copy::<Outcome>();
        assert!(std::mem::size_of::<Outcome>() <= 16);
    }

    #[test]
    fn hotkey_event_roundtrip() {
        for e in [D, U, E] {
            assert_eq!(HotkeyEvent::from_wparam(e as usize), Some(e));
        }
        assert_eq!(HotkeyEvent::from_wparam(0), None);
        assert_eq!(HotkeyEvent::from_wparam(4), None);
    }

    // ---- desktop smoke (real hook + SendInput) ----

    mod desktop {
        use super::super::*;
        use std::sync::Mutex;
        use std::time::{Duration, Instant};
        use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
        use windows::Win32::System::LibraryLoader::GetModuleHandleW;
        use windows::Win32::UI::Input::KeyboardAndMouse::{
            INPUT, INPUT_0, INPUT_KEYBOARD, KEYBD_EVENT_FLAGS, KEYBDINPUT, KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP,
            SendInput, VIRTUAL_KEY,
        };
        use windows::Win32::UI::WindowsAndMessaging::{
            CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetClassNameW, GetForegroundWindow,
            HWND_MESSAGE, PM_REMOVE, PeekMessageW, RegisterClassW, WINDOW_EX_STYLE, WINDOW_STYLE, WM_APP, WNDCLASSW,
        };
        use windows::core::w;

        static SEEN: Mutex<Vec<HotkeyEvent>> = Mutex::new(Vec::new());
        // Serialises the two desktop tests: they share the process-global hook state.
        static LOCK: Mutex<()> = Mutex::new(());

        unsafe extern "system" fn wndproc(h: HWND, m: u32, w: WPARAM, l: LPARAM) -> LRESULT {
            if m == WM_APP + 7 {
                if let Some(ev) = HotkeyEvent::from_wparam(w.0) {
                    SEEN.lock().unwrap().push(ev);
                }
                return LRESULT(0);
            }
            // SAFETY: default processing for everything else.
            unsafe { DefWindowProcW(h, m, w, l) }
        }

        fn key(vk: u32, up: bool) -> INPUT {
            let mut flags = if up { KEYEVENTF_KEYUP } else { KEYBD_EVENT_FLAGS(0) };
            if vk == VK_LWIN || vk == VK_RWIN {
                flags |= KEYEVENTF_EXTENDEDKEY;
            }
            INPUT {
                r#type: INPUT_KEYBOARD,
                Anonymous: INPUT_0 {
                    ki: KEYBDINPUT { wVk: VIRTUAL_KEY(vk as u16), wScan: 0, dwFlags: flags, time: 0, dwExtraInfo: 0 },
                },
            }
        }

        fn send(seq: &[(u32, bool)]) {
            let inputs: Vec<INPUT> = seq.iter().map(|&(vk, up)| key(vk, up)).collect();
            // SAFETY: valid INPUT slice.
            unsafe { SendInput(&inputs, std::mem::size_of::<INPUT>() as i32) };
        }

        fn make_window() -> HWND {
            // SAFETY: registering a private class and creating a message-only window.
            unsafe {
                let hi = GetModuleHandleW(None).unwrap();
                let wc = WNDCLASSW {
                    lpfnWndProc: Some(wndproc),
                    hInstance: hi.into(),
                    lpszClassName: w!("OpenTalkHotkeySmoke"),
                    ..Default::default()
                };
                RegisterClassW(&wc);
                CreateWindowExW(
                    WINDOW_EX_STYLE(0),
                    w!("OpenTalkHotkeySmoke"),
                    w!(""),
                    WINDOW_STYLE(0),
                    0,
                    0,
                    0,
                    0,
                    Some(HWND_MESSAGE),
                    None,
                    Some(hi.into()),
                    None,
                )
                .unwrap()
            }
        }

        /// Pump the window's messages until `want` events arrived or the timeout passes.
        fn pump_until(hwnd: HWND, want: usize, ms: u64) -> Vec<HotkeyEvent> {
            let end = Instant::now() + Duration::from_millis(ms);
            let mut msg = windows::Win32::UI::WindowsAndMessaging::MSG::default();
            while Instant::now() < end {
                // SAFETY: standard PeekMessage pump for our own window.
                unsafe {
                    while PeekMessageW(&mut msg, Some(hwnd), 0, 0, PM_REMOVE).as_bool() {
                        DispatchMessageW(&msg);
                    }
                }
                if SEEN.lock().unwrap().len() >= want {
                    break;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            std::mem::take(&mut *SEEN.lock().unwrap())
        }

        #[test]
        #[ignore = "desktop: installs a real WH_KEYBOARD_LL hook and injects keys"]
        fn desktop_hook_smoke() {
            if !crate::daemon::paste::require_input_desktop("desktop_hook_smoke") {
                return;
            }
            let _g = LOCK.lock().unwrap_or_else(|p| p.into_inner());
            let hwnd = make_window();
            set_allow_injected(true);
            start(hwnd.0 as isize, WM_APP + 7, "Control+Shift+F23").expect("hook installs");
            SEEN.lock().unwrap().clear();

            send(&[(VK_LCONTROL, false), (VK_LSHIFT, false), (0x86, false)]);
            let down = pump_until(hwnd, 1, 1500);
            assert_eq!(down, vec![HotkeyEvent::Down], "combo down");
            send(&[(0x86, true), (VK_LSHIFT, true), (VK_LCONTROL, true)]);
            let up = pump_until(hwnd, 1, 1500);
            assert_eq!(up, vec![HotkeyEvent::Up], "combo up");

            // Esc is swallowed and reported only while capture is on.
            set_esc_capture(true);
            send(&[(VK_ESCAPE, false), (VK_ESCAPE, true)]);
            let esc = pump_until(hwnd, 1, 1500);
            assert_eq!(esc, vec![HotkeyEvent::Escape]);
            set_esc_capture(false);
            send(&[(VK_ESCAPE, false), (VK_ESCAPE, true)]);
            assert!(pump_until(hwnd, 1, 300).is_empty(), "esc ignored when capture is off");

            // Suspended: nothing arrives.
            set_suspended(true);
            send(&[(VK_LCONTROL, false), (VK_LSHIFT, false), (0x86, false), (0x86, true), (VK_LSHIFT, true), (VK_LCONTROL, true)]);
            assert!(pump_until(hwnd, 1, 300).is_empty(), "suspended");
            set_suspended(false);

            // Live reconfiguration.
            set_hotkey("F22").unwrap();
            send(&[(0x85, false), (0x85, true)]);
            assert_eq!(pump_until(hwnd, 2, 1500), vec![HotkeyEvent::Down, HotkeyEvent::Up]);

            set_allow_injected(false);
            send(&[(0x85, false), (0x85, true)]);
            assert!(pump_until(hwnd, 1, 300).is_empty(), "injected ignored again");

            stop();
            // SAFETY: destroying the window created above on this thread.
            unsafe {
                let _ = DestroyWindow(hwnd);
            }
        }

        /// `stop()` must wait for the old hook thread, or an immediate `start()` sees it alive,
        /// skips installing a new hook, and the dying thread takes the only hook with it.
        #[test]
        #[ignore = "desktop: installs and removes a real WH_KEYBOARD_LL hook repeatedly"]
        fn desktop_hook_stop_then_start_is_reliable() {
            if !crate::daemon::paste::require_input_desktop("desktop_hook_stop_then_start_is_reliable") {
                return;
            }
            let _g = LOCK.lock().unwrap_or_else(|p| p.into_inner());
            let hwnd = make_window();
            set_allow_injected(true);
            for round in 0..20 {
                start(hwnd.0 as isize, WM_APP + 7, "F22").expect("hook installs");
                SEEN.lock().unwrap().clear();
                send(&[(0x85, false), (0x85, true)]);
                let got = pump_until(hwnd, 2, 1500);
                assert_eq!(got, vec![HotkeyEvent::Down, HotkeyEvent::Up], "round {round}");
                stop();
            }
            // Idempotent.
            stop();
            set_allow_injected(false);
            // SAFETY: destroying the window created above on this thread.
            unsafe {
                let _ = DestroyWindow(hwnd);
            }
        }

        fn foreground_class() -> String {
            let mut buf = [0u16; 128];
            // SAFETY: buffer is valid for the call.
            let n = unsafe {
                let h = GetForegroundWindow();
                GetClassNameW(h, &mut buf)
            };
            String::from_utf16_lossy(&buf[..n.max(0) as usize])
        }

        /// A bare Win key-up opens Start; modifier chords never do, so the mask is exercised
        /// with a Win-only hotkey. `mask` off is the negative control (Start must open).
        fn start_menu_opens(mask: bool) -> bool {
            let hwnd = make_window();
            set_allow_injected(true);
            start(hwnd.0 as isize, WM_APP + 7, "RightWin").expect("hook installs");
            set_start_menu_mask(mask);
            SEEN.lock().unwrap().clear();

            send(&[(VK_RWIN, false)]);
            assert_eq!(pump_until(hwnd, 1, 1500), vec![HotkeyEvent::Down]);
            send(&[(VK_RWIN, true)]);
            assert_eq!(pump_until(hwnd, 1, 1500), vec![HotkeyEvent::Up]);
            std::thread::sleep(Duration::from_millis(600));
            let class = foreground_class();
            let opened = matches!(class.as_str(), "Windows.UI.Core.CoreWindow" | "XamlExplorerHostIslandWindow");
            if opened {
                send(&[(VK_ESCAPE, false), (VK_ESCAPE, true)]);
                std::thread::sleep(Duration::from_millis(400));
            }
            set_start_menu_mask(true);
            set_allow_injected(false);
            stop();
            // SAFETY: destroying the window created above on this thread.
            unsafe {
                let _ = DestroyWindow(hwnd);
            }
            opened
        }

        #[test]
        #[ignore = "desktop: injects a RightWin tap and checks the Start menu stayed closed"]
        fn desktop_start_menu_mask() {
            if !crate::daemon::paste::require_input_desktop("desktop_start_menu_mask") {
                return;
            }
            let _g = LOCK.lock().unwrap_or_else(|p| p.into_inner());
            assert!(!start_menu_opens(true), "Start menu opened despite the mask");
        }

        #[test]
        #[ignore = "desktop: negative control, opens the Start menu briefly"]
        fn desktop_start_menu_opens_without_mask() {
            if !crate::daemon::paste::require_input_desktop("desktop_start_menu_opens_without_mask") {
                return;
            }
            let _g = LOCK.lock().unwrap_or_else(|p| p.into_inner());
            assert!(start_menu_opens(false), "control: a bare Win tap should open Start");
        }
    }
}
