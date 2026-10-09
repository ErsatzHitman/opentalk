//! System tray icon (Shell_NotifyIconW) + popup menu. TASK win-output implements.
//! Left click -> TrayCmd::OpenSettings; right click -> menu.
//! Menu: Start/Stop listening, Show/Hide floating icon, Open OpenTalk, History, Dictionary,
//! Retry last, separator, Quit OpenTalk. Tooltip "OpenTalk - Voice Dictation".
//!
//! The icon (OpenWhispr logo: ring + three bars) is drawn at runtime at the system small-icon size,
//! white on a dark taskbar and black on a light one, so no resource file is needed. The window proc
//! of the owner must forward `callback_msg` to [`Tray::handle`] and the registered `TaskbarCreated`
//! message (see [`taskbar_created_message`]) to [`Tray::readd`].
use std::ffi::c_void;

use tiny_skia::{LineCap, Paint, PathBuilder, Pixmap, Stroke, Transform};
use windows::Win32::Foundation::{HWND, LPARAM, POINT, WPARAM};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RegGetValueW};
use windows::Win32::UI::Shell::{
    NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY, NOTIFYICONDATAW,
    Shell_NotifyIconW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreateIcon, CreatePopupMenu, DestroyIcon, DestroyMenu, GetCursorPos,
    GetSystemMetrics, HICON, HMENU, MF_CHECKED, MF_GRAYED, MF_SEPARATOR, MF_STRING, PostMessageW,
    RegisterWindowMessageW, SM_CXSMICON, SetForegroundWindow, SetMenuDefaultItem, TPM_RETURNCMD,
    TPM_RIGHTBUTTON, TrackPopupMenu, WM_CONTEXTMENU, WM_LBUTTONUP, WM_NULL, WM_RBUTTONUP,
};
use windows::core::{PCSTR, PCWSTR, w};

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
}

#[derive(Clone, Copy, Debug, Default)]
pub struct TrayState {
    pub listening: bool,
    pub floating_icon_visible: bool,
    pub can_retry: bool,
}

const TOOLTIP: &str = "OpenTalk - Voice Dictation";
const ICON_ID: u32 = 1;

// ---------------------------------------------------------------------------------------------
// Menu model (pure, testable)
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct MenuEntry {
    pub cmd: Option<TrayCmd>,
    pub label: &'static str,
    pub checked: bool,
    pub disabled: bool,
    /// Rendered bold: what a double-click would do.
    pub default: bool,
}

impl MenuEntry {
    const SEPARATOR: MenuEntry =
        MenuEntry { cmd: None, label: "", checked: false, disabled: false, default: false };

    fn item(cmd: TrayCmd, label: &'static str) -> MenuEntry {
        MenuEntry { cmd: Some(cmd), label, checked: false, disabled: false, default: false }
    }

    fn is_separator(&self) -> bool {
        self.cmd.is_none()
    }
}

/// Menu command ids are the 1-based position of the command in this table.
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

pub(crate) fn menu_model(state: TrayState) -> Vec<MenuEntry> {
    let mut open = MenuEntry::item(TrayCmd::OpenSettings, "Open OpenTalk");
    open.default = true;
    let mut floating = MenuEntry::item(TrayCmd::ToggleFloatingIcon, "Show floating icon");
    floating.checked = state.floating_icon_visible;
    let mut retry = MenuEntry::item(TrayCmd::RetryLast, "Retry last");
    retry.disabled = !state.can_retry;
    vec![
        MenuEntry::item(
            TrayCmd::ToggleListening,
            if state.listening { "Stop listening" } else { "Start listening" },
        ),
        floating,
        MenuEntry::SEPARATOR,
        open,
        MenuEntry::item(TrayCmd::OpenHistory, "History"),
        MenuEntry::item(TrayCmd::OpenDictionary, "Dictionary"),
        retry,
        MenuEntry::SEPARATOR,
        MenuEntry::item(TrayCmd::Quit, "Quit OpenTalk"),
    ]
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

/// Opt the process's popup menus into dark mode when Windows is dark (uxtheme ordinals 135 and
/// 136; harmless no-ops when unavailable).
fn allow_dark_menus() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        // SAFETY: ordinal 135 is `SetPreferredAppMode(int)` (1903+) / `AllowDarkModeForApp(BOOL)`
        // (1809); both take one int-sized argument. 136 is `FlushMenuThemes()`.
        unsafe {
            let Ok(lib) = LoadLibraryW(w!("uxtheme.dll")) else { return };
            if let Some(f) = GetProcAddress(lib, PCSTR(135 as *const u8)) {
                let f: unsafe extern "system" fn(i32) -> i32 = std::mem::transmute(f);
                f(1); // APPMODE_ALLOWDARK
            }
            if let Some(f) = GetProcAddress(lib, PCSTR(136 as *const u8)) {
                let f: unsafe extern "system" fn() = std::mem::transmute(f);
                f();
            }
        }
    });
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

impl Tray {
    /// Add the icon. Shell notifications arrive at `hwnd` as message `callback_msg`.
    ///
    /// A failed `NIM_ADD` (Explorer not running yet, e.g. at login) is not an error: the icon is
    /// added when `TaskbarCreated` reaches [`Tray::readd`].
    pub fn create(hwnd: isize, callback_msg: u32) -> Result<Tray, String> {
        allow_dark_menus();
        let icon = make_icon()?;
        let nid = notify_data(hwnd, callback_msg, icon);
        // SAFETY: nid is fully initialised.
        let added = unsafe { Shell_NotifyIconW(NIM_ADD, &nid) }.as_bool();
        if !added {
            crate::log_warn!("tray: NIM_ADD failed; waiting for TaskbarCreated");
        }
        Ok(Tray { hwnd, callback_msg, icon, added })
    }

    /// Handle a `callback_msg` (lparam from the window proc). Shows the menu on right click.
    pub fn handle(&mut self, hwnd: isize, lparam: isize, state: TrayState) -> Option<TrayCmd> {
        match (lparam as usize & 0xFFFF) as u32 {
            WM_LBUTTONUP => Some(TrayCmd::OpenSettings),
            WM_RBUTTONUP | WM_CONTEXTMENU => show_menu(hwnd, state),
            _ => None,
        }
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
        if !self.added {
            crate::log_warn!("tray: could not (re-)add icon");
        }
    }

    pub fn remove(&mut self) {
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

/// Pop the context menu at the cursor and return the chosen command.
fn show_menu(hwnd: isize, state: TrayState) -> Option<TrayCmd> {
    let owner = HWND(hwnd as *mut c_void);
    // SAFETY: the menu is created, shown and destroyed within this function; the owner window is
    // the caller's live message window.
    unsafe {
        let menu: HMENU = CreatePopupMenu().ok()?;
        let mut default_id = 0usize;
        for e in menu_model(state) {
            if e.is_separator() {
                let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
                continue;
            }
            let id = e.cmd.map_or(0, cmd_id);
            let mut flags = MF_STRING;
            if e.checked {
                flags |= MF_CHECKED;
            }
            if e.disabled {
                flags |= MF_GRAYED;
            }
            if e.default {
                default_id = id;
            }
            let label: Vec<u16> = e.label.encode_utf16().chain(std::iter::once(0)).collect();
            let _ = AppendMenuW(menu, flags, id, PCWSTR(label.as_ptr()));
        }
        if default_id != 0 {
            let _ = SetMenuDefaultItem(menu, default_id as u32, 0);
        }

        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        // Required so the menu closes when the user clicks elsewhere.
        let _ = SetForegroundWindow(owner);
        let chosen = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_RIGHTBUTTON, pt.x, pt.y, Some(0), owner, None);
        // Documented workaround so the next click on the tray icon is not swallowed.
        let _ = PostMessageW(Some(owner), WM_NULL, WPARAM(0), LPARAM(0));
        let _ = DestroyMenu(menu);
        cmd_from_id(chosen.0 as u32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn menu_labels_follow_state() {
        let idle = menu_model(TrayState::default());
        assert_eq!(idle[0].label, "Start listening");
        assert!(!idle[1].checked);
        let retry = idle.iter().find(|e| e.cmd == Some(TrayCmd::RetryLast)).unwrap();
        assert!(retry.disabled);

        let busy = menu_model(TrayState { listening: true, floating_icon_visible: true, can_retry: true });
        assert_eq!(busy[0].label, "Stop listening");
        assert!(busy[1].checked);
        let retry = busy.iter().find(|e| e.cmd == Some(TrayCmd::RetryLast)).unwrap();
        assert!(!retry.disabled);
    }

    #[test]
    fn menu_order_and_default() {
        let m = menu_model(TrayState::default());
        let cmds: Vec<_> = m.iter().filter_map(|e| e.cmd).collect();
        assert_eq!(
            cmds,
            vec![
                TrayCmd::ToggleListening,
                TrayCmd::ToggleFloatingIcon,
                TrayCmd::OpenSettings,
                TrayCmd::OpenHistory,
                TrayCmd::OpenDictionary,
                TrayCmd::RetryLast,
                TrayCmd::Quit,
            ]
        );
        assert_eq!(m.last().unwrap().label, "Quit OpenTalk");
        assert!(m[m.len() - 2].is_separator());
        let defaults: Vec<_> = m.iter().filter(|e| e.default).collect();
        assert_eq!(defaults.len(), 1);
        assert_eq!(defaults[0].cmd, Some(TrayCmd::OpenSettings));
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
                PCWSTR::null(),
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
        assert_eq!(
            t.handle(hwnd.0 as isize, WM_LBUTTONUP as isize, TrayState::default()),
            Some(TrayCmd::OpenSettings)
        );
        t.remove();
        assert!(!t.added);
        // SAFETY: window created above on this thread.
        unsafe {
            let _ = windows::Win32::UI::WindowsAndMessaging::DestroyWindow(hwnd);
        }
    }
}
