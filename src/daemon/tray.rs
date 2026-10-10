//! System tray icon (Shell_NotifyIconW) and the popup it opens.
//!
//! Activating the icon (left click, right click, keyboard Enter / Space, also from the hidden
//! overflow area) opens a small flat popup near the tray instead of a classic menu: see [`popup`]
//! (model, layout, placement), `popup_draw` (software rendering) and `popup_win` (the window). The
//! icon asks for `NOTIFYICON_VERSION_4` so it receives `NIN_SELECT` / `NIN_KEYSELECT`; on a shell
//! that refuses, the plain `WM_LBUTTONUP` / `WM_RBUTTONUP` messages are used instead.
//!
//! The icon (OpenWhispr logo: ring + three bars) is drawn at runtime at the system small-icon size,
//! white on a dark taskbar and black on a light one, so no resource file is needed. The window proc
//! of the owner must forward `callback_msg` to [`Tray::activation`] and the registered `TaskbarCreated`
//! message (see [`taskbar_created_message`]) to [`Tray::readd`], and [`WM_TRAY_POPUP_CMD`] to
//! [`command_from_popup`].
mod popup;
mod popup_draw;
mod popup_win;

pub use popup_draw::cli_render as cli_render_popup;
pub use popup_win::WM_TRAY_POPUP_CMD;

use std::ffi::c_void;

use tiny_skia::{LineCap, Paint, PathBuilder, Pixmap, Stroke, Transform};
use windows::Win32::Foundation::HWND;
use windows::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RegGetValueW};
use windows::Win32::UI::Shell::{
    NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY, NIM_SETVERSION, NOTIFYICONDATAW,
    Shell_NotifyIconW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateIcon, DestroyIcon, GetSystemMetrics, HICON, RegisterWindowMessageW, SM_CXSMICON,
    WM_CONTEXTMENU, WM_LBUTTONUP, WM_RBUTTONUP,
};
use windows::core::w;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrayCmd {
    ToggleListening,
    ToggleFloatingIcon,
    OpenSettings,
    OpenHistory,
    OpenDictionary,
    RetryLast,
    Quit,
}

pub struct Tray {
    hwnd: isize,
    callback_msg: u32,
    icon: HICON,
    /// Whether Explorer currently shows our icon.
    added: bool,
    /// The shell accepted `NOTIFYICON_VERSION_4` (`NIN_SELECT` / `NIN_KEYSELECT` / `WM_CONTEXTMENU`).
    v4: bool,
}

/// What the popup needs to know about the daemon when the icon is activated.
#[derive(Clone, Debug, Default)]
pub struct TrayState {
    pub listening: bool,
    pub transcribing: bool,
    pub floating_icon_visible: bool,
    pub can_retry: bool,
    /// The dictation hotkey as the pill shows it, e.g. "Ctrl + Win".
    pub hotkey: String,
    /// The app theme resolved to dark / light.
    pub dark: bool,
    /// Full text of the newest dictations (empty when history is off).
    pub recent: Vec<String>,
}

impl TrayState {
    fn content(&self) -> popup::Content {
        use popup::Phase;
        popup::Content {
            phase: if self.transcribing {
                Phase::Transcribing
            } else if self.listening {
                Phase::Recording
            } else {
                Phase::Ready
            },
            floating_pill_visible: self.floating_icon_visible,
            can_retry: self.can_retry,
            hotkey: self.hotkey.clone(),
            recent: self.recent.iter().map(|t| popup::preview_line(t)).filter(|t| !t.is_empty()).take(popup::MAX_RECENT).collect(),
        }
    }
}

/// A tray event that opens (or closes) the popup.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Activation {
    /// Triggered from the keyboard: the popup starts with its first row focused.
    pub keyboard: bool,
}

const TOOLTIP: &str = "OpenTalk - Voice Dictation";
const ICON_ID: u32 = 1;
const NOTIFYICON_VERSION_4: u32 = 4;
/// `NIN_SELECT` (`WM_USER`) and `NIN_KEYSELECT` (`NIN_SELECT | NINF_KEY`).
const NIN_SELECT: u32 = 0x400;
const NIN_KEYSELECT: u32 = 0x401;

/// What a notification message means: any activation of the icon, once. Version 4 reports a click as
/// `WM_LBUTTONUP` followed by `NIN_SELECT` (and a right click as `WM_RBUTTONUP` then
/// `WM_CONTEXTMENU`), so only the second of each pair counts; the legacy protocol has just the first.
pub(crate) fn classify(v4: bool, event: u32) -> Option<Activation> {
    match (v4, event) {
        (true, NIN_SELECT | WM_CONTEXTMENU) => Some(Activation { keyboard: false }),
        (true, NIN_KEYSELECT) => Some(Activation { keyboard: true }),
        (false, WM_LBUTTONUP | WM_RBUTTONUP) => Some(Activation { keyboard: false }),
        _ => None,
    }
}

/// Toggle the popup for an activation: open it near the tray, or close it if it is open. `owner` is
/// the daemon window the chosen row is posted back to.
pub fn activate(owner: isize, act: Activation, state: &TrayState) {
    popup_win::activate(popup_win::Request {
        owner,
        content: state.content(),
        texts: state.recent.clone(),
        icon_id: ICON_ID,
        keyboard: act.keyboard,
        dark: state.dark,
    });
}

/// Is the popup open right now?
pub fn popup_is_open() -> bool {
    popup_win::is_open()
}

/// Show `state` in the open popup (no-op when it is closed).
pub fn refresh_popup(state: &TrayState) {
    popup_win::refresh(state.content(), state.recent.clone());
}

// ---------------------------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------------------------

/// Command ids carried by [`WM_TRAY_POPUP_CMD`] are the 1-based position of the command in this table.
const CMDS: [TrayCmd; 7] = [
    TrayCmd::ToggleListening,
    TrayCmd::ToggleFloatingIcon,
    TrayCmd::OpenSettings,
    TrayCmd::OpenHistory,
    TrayCmd::OpenDictionary,
    TrayCmd::RetryLast,
    TrayCmd::Quit,
];

fn cmd_id(cmd: TrayCmd) -> usize {
    CMDS.iter().position(|c| *c == cmd).map_or(0, |i| i + 1)
}

fn cmd_from_id(id: u32) -> Option<TrayCmd> {
    (id as usize).checked_sub(1).and_then(|i| CMDS.get(i).copied())
}

/// The command a [`WM_TRAY_POPUP_CMD`] message carries (`wparam`).
pub fn command_from_popup(wparam: usize) -> Option<TrayCmd> {
    cmd_from_id(wparam as u32)
}

// ---------------------------------------------------------------------------------------------
// Icon
// ---------------------------------------------------------------------------------------------

/// True when the taskbar uses the light theme (dark glyphs needed).
fn taskbar_is_light() -> bool {
    let mut data = 1u32;
    let mut size = size_of::<u32>() as u32;
    // SAFETY: `data`/`size` are valid out parameters for a REG_DWORD read.
    let rc = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            w!("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize"),
            w!("SystemUsesLightTheme"),
            RRF_RT_REG_DWORD,
            None,
            Some(&mut data as *mut u32 as *mut c_void),
            Some(&mut size),
        )
    };
    rc.0 == 0 && data != 0
}

/// The OpenWhispr logo (24-unit viewBox: ring r=10, three bars, stroke 2, round caps) as straight
/// RGBA, `size` x `size`, single colour. Empty for a zero size.
pub(crate) fn render_logo(size: u32, light_glyph: bool) -> Vec<u8> {
    let Some(mut pm) = Pixmap::new(size, size) else { return Vec::new() };
    let mut paint = Paint::default();
    let c = if light_glyph { 255 } else { 0 };
    paint.set_color_rgba8(c, c, c, 255);
    paint.anti_alias = true;
    let stroke = Stroke { width: 2.0, line_cap: LineCap::Round, ..Stroke::default() };
    let ts = Transform::from_scale(size as f32 / 24.0, size as f32 / 24.0);

    if let Some(ring) = PathBuilder::from_circle(12.0, 12.0, 10.0) {
        pm.stroke_path(&ring, &paint, &stroke, ts, None);
    }
    let mut pb = PathBuilder::new();
    for (x, y0, y1) in [(8.75, 10.0, 14.0), (12.0, 8.0, 16.0), (15.25, 10.0, 14.0)] {
        pb.move_to(x, y0);
        pb.line_to(x, y1);
    }
    if let Some(bars) = pb.finish() {
        pm.stroke_path(&bars, &paint, &stroke, ts, None);
    }

    // tiny-skia stores premultiplied RGBA; icons want straight alpha.
    let mut out = Vec::with_capacity((size * size * 4) as usize);
    for p in pm.pixels() {
        let d = p.demultiply();
        out.extend_from_slice(&[d.red(), d.green(), d.blue(), d.alpha()]);
    }
    out
}

/// Build an HICON from straight RGBA.
fn icon_from_rgba(rgba: &[u8], size: u32) -> Option<HICON> {
    let n = (size * size) as usize;
    if rgba.len() < n * 4 {
        return None;
    }
    let mut bgra = Vec::with_capacity(n * 4);
    for p in rgba.chunks_exact(4).take(n) {
        bgra.extend_from_slice(&[p[2], p[1], p[0], p[3]]);
    }
    // 1bpp AND mask, rows padded to 16 bits; a set bit means transparent.
    let stride = (size as usize).div_ceil(16) * 2;
    let mut and = vec![0u8; stride * size as usize];
    for y in 0..size as usize {
        for x in 0..size as usize {
            if rgba[(y * size as usize + x) * 4 + 3] == 0 {
                and[y * stride + x / 8] |= 0x80 >> (x % 8);
            }
        }
    }
    // SAFETY: both buffers are sized for a size x size icon (32bpp XOR, 1bpp AND).
    unsafe { CreateIcon(None, size as i32, size as i32, 1, 32, and.as_ptr(), bgra.as_ptr()) }.ok()
}

fn make_icon() -> Result<HICON, String> {
    // SAFETY: trivial metric query.
    let size = (unsafe { GetSystemMetrics(SM_CXSMICON) }).clamp(16, 64) as u32;
    let rgba = render_logo(size, !taskbar_is_light());
    icon_from_rgba(&rgba, size).ok_or_else(|| "could not create tray icon".to_owned())
}

// ---------------------------------------------------------------------------------------------
// Shell
// ---------------------------------------------------------------------------------------------

/// The registered `TaskbarCreated` broadcast; forward it to [`Tray::readd`].
pub fn taskbar_created_message() -> u32 {
    // SAFETY: wide literal, NUL terminated.
    unsafe { RegisterWindowMessageW(w!("TaskbarCreated")) }
}

fn notify_data(hwnd: isize, msg: u32, icon: HICON) -> NOTIFYICONDATAW {
    let mut nid = NOTIFYICONDATAW {
        cbSize: size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: HWND(hwnd as *mut c_void),
        uID: ICON_ID,
        uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP,
        uCallbackMessage: msg,
        hIcon: icon,
        ..Default::default()
    };
    for (dst, src) in nid.szTip.iter_mut().zip(TOOLTIP.encode_utf16().take(127)) {
        *dst = src;
    }
    nid
}

/// Ask the shell for the version 4 callback protocol; `false` when it refuses.
fn set_version_4(hwnd: isize, msg: u32, icon: HICON) -> bool {
    let mut nid = notify_data(hwnd, msg, icon);
    nid.Anonymous.uVersion = NOTIFYICON_VERSION_4;
    // SAFETY: nid is fully initialised.
    unsafe { Shell_NotifyIconW(NIM_SETVERSION, &nid) }.as_bool()
}

impl Tray {
    /// Add the icon. Shell notifications arrive at `hwnd` as message `callback_msg`.
    ///
    /// A failed `NIM_ADD` (Explorer not running yet, e.g. at login) is not an error: the icon is
    /// added when `TaskbarCreated` reaches [`Tray::readd`].
    pub fn create(hwnd: isize, callback_msg: u32) -> Result<Tray, String> {
        let icon = make_icon()?;
        let nid = notify_data(hwnd, callback_msg, icon);
        // SAFETY: nid is fully initialised.
        let added = unsafe { Shell_NotifyIconW(NIM_ADD, &nid) }.as_bool();
        if !added {
            crate::log_warn!("tray: NIM_ADD failed; waiting for TaskbarCreated");
        }
        let v4 = added && set_version_4(hwnd, callback_msg, icon);
        Ok(Tray { hwnd, callback_msg, icon, added, v4 })
    }

    /// Interpret a `callback_msg` (its `lparam` from the window proc): `Some` when the icon was
    /// activated and the popup should toggle.
    pub fn activation(&self, lparam: isize) -> Option<Activation> {
        classify(self.v4, (lparam as usize & 0xFFFF) as u32)
    }

    /// Re-add after Explorer restarts (TaskbarCreated). Also refreshes the glyph colour, so it can
    /// double as the response to a taskbar theme change.
    pub fn readd(&mut self) {
        if let Ok(new) = make_icon() {
            let old = std::mem::replace(&mut self.icon, new);
            // SAFETY: `old` was created by us; the shell copies the icon on NIM_ADD/NIM_MODIFY.
            let nid = notify_data(self.hwnd, self.callback_msg, self.icon);
            unsafe {
                self.added = Shell_NotifyIconW(NIM_ADD, &nid).as_bool()
                    || Shell_NotifyIconW(NIM_MODIFY, &nid).as_bool();
                let _ = DestroyIcon(old);
            }
        } else {
            let nid = notify_data(self.hwnd, self.callback_msg, self.icon);
            // SAFETY: nid is fully initialised.
            self.added = unsafe {
                Shell_NotifyIconW(NIM_ADD, &nid).as_bool() || Shell_NotifyIconW(NIM_MODIFY, &nid).as_bool()
            };
        }
        self.v4 = self.added && set_version_4(self.hwnd, self.callback_msg, self.icon);
        if !self.added {
            crate::log_warn!("tray: could not (re-)add icon");
        }
    }

    pub fn remove(&mut self) {
        popup_win::close();
        if self.added {
            let nid = notify_data(self.hwnd, self.callback_msg, self.icon);
            // SAFETY: nid identifies our icon (hWnd + uID).
            unsafe {
                let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
            }
            self.added = false;
        }
        if !self.icon.is_invalid() {
            // SAFETY: icon was created by CreateIcon and is no longer referenced by the shell.
            unsafe {
                let _ = DestroyIcon(self.icon);
            }
            self.icon = HICON::default();
        }
    }
}

impl Drop for Tray {
    fn drop(&mut self) {
        self.remove();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_activation_opens_the_popup_once() {
        // Version 4: the second message of each pair, plus the keyboard.
        assert_eq!(classify(true, NIN_SELECT), Some(Activation { keyboard: false }));
        assert_eq!(classify(true, WM_CONTEXTMENU), Some(Activation { keyboard: false }));
        assert_eq!(classify(true, NIN_KEYSELECT), Some(Activation { keyboard: true }));
        assert_eq!(classify(true, WM_LBUTTONUP), None, "v4 sends NIN_SELECT right after");
        assert_eq!(classify(true, WM_RBUTTONUP), None, "v4 sends WM_CONTEXTMENU right after");
        // Legacy: the mouse-up messages.
        assert_eq!(classify(false, WM_LBUTTONUP), Some(Activation { keyboard: false }));
        assert_eq!(classify(false, WM_RBUTTONUP), Some(Activation { keyboard: false }));
        assert_eq!(classify(false, NIN_SELECT), None);
        // Mouse moves, balloon events and the like never open it.
        for ev in [0x0200u32 /* WM_MOUSEMOVE */, 0x0201 /* WM_LBUTTONDOWN */, 0x402 /* NIN_BALLOONSHOW */, 0x405, 0] {
            assert_eq!(classify(true, ev), None);
            assert_eq!(classify(false, ev), None);
        }
    }

    #[test]
    fn popup_content_follows_the_daemon_state() {
        use popup::Phase;
        let idle = TrayState { floating_icon_visible: true, hotkey: "Ctrl + Win".into(), ..TrayState::default() };
        let c = idle.content();
        assert_eq!((c.phase, c.floating_pill_visible, c.can_retry), (Phase::Ready, true, false));
        assert_eq!(c.hotkey, "Ctrl + Win");
        assert!(c.recent.is_empty());
        let rec = TrayState { listening: true, can_retry: true, ..idle.clone() };
        assert_eq!(rec.content().phase, Phase::Recording);
        assert!(rec.content().can_retry);
        // Transcribing wins over a still-set listening flag.
        let busy = TrayState { listening: true, transcribing: true, ..idle.clone() };
        assert_eq!(busy.content().phase, Phase::Transcribing);
        // Recent entries are collapsed to one line, blanks dropped, at most two kept.
        let many = TrayState { recent: vec!["  one\n two ".into(), "   ".into(), "three".into(), "four".into()], ..idle };
        assert_eq!(many.content().recent, ["one two", "three"]);
    }

    #[test]
    fn popup_commands_reach_the_daemon_window_by_id() {
        for c in CMDS {
            assert_eq!(command_from_popup(cmd_id(c)), Some(c));
        }
        assert_eq!(command_from_popup(0), None);
        assert_eq!(WM_TRAY_POPUP_CMD, crate::core::ipc::WM_OT_INTERNAL_BASE + 0x40);
    }

    #[test]
    fn command_ids_round_trip() {
        for c in CMDS {
            assert_eq!(cmd_from_id(cmd_id(c) as u32), Some(c));
        }
        assert_eq!(cmd_from_id(0), None);
        assert_eq!(cmd_from_id(99), None);
    }

    #[test]
    fn logo_is_drawn_in_requested_colour() {
        for (size, light) in [(16u32, true), (32, false), (24, true)] {
            let px = render_logo(size, light);
            assert_eq!(px.len(), (size * size * 4) as usize);
            let opaque: Vec<&[u8]> = px.chunks_exact(4).filter(|p| p[3] > 200).collect();
            assert!(opaque.len() > 10, "{size}: ring and bars drawn");
            let want = if light { 255 } else { 0 };
            assert!(opaque.iter().all(|p| p[0] == want && p[1] == want && p[2] == want));
            // The corners stay transparent (ring inside a 24-unit box).
            assert_eq!(px[3], 0);
            // The centre bar covers the middle of the icon.
            let c = size as usize / 2;
            let strongest = (c - 1..=c)
                .flat_map(|y| (c - 1..=c).map(move |x| (y, x)))
                .map(|(y, x)| px[(y * size as usize + x) * 4 + 3])
                .max()
                .unwrap();
            assert!(strongest > 150, "{size}: centre alpha {strongest}");
        }
    }

    /// Dumps the tray glyph (white on dark, 8x nearest-neighbour) for eyeballing.
    #[test]
    fn logo_preview_png() {
        let size = 32u32;
        let rgba = render_logo(size, true);
        let scale = 8u32;
        let mut pm = Pixmap::new(size * scale, size * scale).unwrap();
        pm.fill(tiny_skia::Color::from_rgba8(32, 32, 32, 255));
        for y in 0..size * scale {
            for x in 0..size * scale {
                let i = (((y / scale) * size + x / scale) * 4) as usize;
                let a = rgba[i + 3] as u32;
                let bg = 32u32;
                let v = ((rgba[i] as u32 * a + bg * (255 - a)) / 255) as u8;
                let px = tiny_skia::ColorU8::from_rgba(v, v, v, 255).premultiply();
                pm.pixels_mut()[(y * size * scale + x) as usize] = px;
            }
        }
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target");
        let _ = std::fs::create_dir_all(&dir);
        pm.save_png(dir.join("tray_logo.png")).unwrap();
    }

    #[test]
    fn icon_handle_can_be_created() {
        let rgba = render_logo(32, true);
        let h = icon_from_rgba(&rgba, 32).expect("icon");
        // SAFETY: handle we just created.
        unsafe {
            let _ = DestroyIcon(h);
        }
        assert!(icon_from_rgba(&rgba[..10], 32).is_none());
    }

    #[test]
    fn taskbar_message_is_registered() {
        assert!(taskbar_created_message() >= 0xC000);
    }

    /// Adds a real tray icon to the taskbar, then removes it.
    #[test]
    #[ignore]
    fn desktop_tray_add_remove() {
        let hwnd = unsafe {
            windows::Win32::UI::WindowsAndMessaging::CreateWindowExW(
                windows::Win32::UI::WindowsAndMessaging::WINDOW_EX_STYLE(0),
                w!("STATIC"),
                windows::core::PCWSTR::null(),
                windows::Win32::UI::WindowsAndMessaging::WS_POPUP,
                0,
                0,
                0,
                0,
                Some(windows::Win32::UI::WindowsAndMessaging::HWND_MESSAGE),
                None,
                None,
                None,
            )
        }
        .expect("window");
        let mut t = Tray::create(hwnd.0 as isize, 0x8001).expect("tray");
        assert!(t.added, "NIM_ADD succeeded");
        t.readd();
        assert!(t.added);
        assert!(t.activation(NIN_SELECT as isize).is_some() || t.activation(WM_LBUTTONUP as isize).is_some());
        t.remove();
        assert!(!t.added);
        // SAFETY: window created above on this thread.
        unsafe {
            let _ = windows::Win32::UI::WindowsAndMessaging::DestroyWindow(hwnd);
        }
    }

    /// Opens the real popup on the desktop, drives it with the keyboard model, and closes it.
    #[test]
    #[ignore]
    fn desktop_tray_popup_opens_and_closes() {
        use windows::Win32::UI::WindowsAndMessaging::{CreateWindowExW, DestroyWindow, HWND_MESSAGE, WINDOW_EX_STYLE, WS_POPUP};
        // SAFETY: a message-only STATIC window on this thread, destroyed below.
        let owner = unsafe { CreateWindowExW(WINDOW_EX_STYLE(0), w!("STATIC"), windows::core::PCWSTR::null(), WS_POPUP, 0, 0, 0, 0, Some(HWND_MESSAGE), None, None, None) }.expect("window");
        let state = TrayState { floating_icon_visible: true, hotkey: "Ctrl + Win".into(), recent: vec!["Hello from the tray test".into()], ..TrayState::default() };
        let act = Activation { keyboard: true };
        activate(owner.0 as isize, act, &state);
        assert!(popup_win::is_open(), "the popup opened");
        // Another activation straight away is the echo of the opening click and is ignored.
        activate(owner.0 as isize, act, &state);
        assert!(popup_win::is_open());
        popup_win::close();
        assert!(!popup_win::is_open());
        // The click that deactivated it must not reopen it.
        activate(owner.0 as isize, act, &state);
        assert!(!popup_win::is_open(), "re-click guard");
        // SAFETY: window created above on this thread.
        unsafe {
            let _ = DestroyWindow(owner);
        }
    }
}
