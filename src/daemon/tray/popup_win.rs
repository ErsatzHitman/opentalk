//! Tray popup, Win32 side: one layered popup window on the daemon's UI thread, created when the tray
//! icon is activated and destroyed as soon as it closes, so a closed popup costs nothing (no window,
//! no bitmap, no timer). It takes the keyboard focus (arrows, Enter, Esc) and dismisses itself when
//! it loses activation, which also covers a click anywhere else.
//!
//! The window is `WS_EX_LAYERED | TOOLWINDOW | TOPMOST` and shows a premultiplied BGRA frame
//! rendered by `popup_draw`; it repaints only when the highlight changes. A chosen row is posted to
//! the daemon's window as [`WM_TRAY_POPUP_CMD`] so the command runs from the normal message loop.

use super::popup::{self, Content, Decision, Edge, IRect, KeyOutcome, Layout, MARGIN, Nav, Target};
use super::popup_draw::{self, View};
use super::{TrayCmd, cmd_id};
use crate::core::ipc;
use crate::daemon::pill::kit::{DevRect, Dib, scale_from_dpi};
use std::cell::{Cell, RefCell};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM};
use windows::Win32::Graphics::Gdi::{AC_SRC_ALPHA, AC_SRC_OVER, BLENDFUNCTION, GetMonitorInfoW, HMONITOR, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromPoint};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::SystemInformation::GetTickCount64;
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::Input::KeyboardAndMouse::{GetKeyState, ReleaseCapture, SetCapture, TME_LEAVE, TRACKMOUSEEVENT, TrackMouseEvent, VK_SHIFT};
use windows::Win32::UI::Shell::{ABE_BOTTOM, ABE_LEFT, ABE_RIGHT, ABE_TOP, ABM_GETTASKBARPOS, APPBARDATA, NOTIFYICONIDENTIFIER, SHAppBarMessage, Shell_NotifyIconGetRect};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, GetCursorPos, GetForegroundWindow, HWND_TOPMOST, IDC_ARROW, LoadCursorW, PostMessageW, RegisterClassExW, SW_SHOW,
    SetCursor, SetForegroundWindow, SetWindowPos, ShowWindow, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, ULW_ALPHA, UpdateLayeredWindow, WM_ACTIVATE, WM_CLOSE,
    WM_DISPLAYCHANGE, WM_ERASEBKGND, WM_KEYDOWN, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE, WM_SETCURSOR, WM_SETTINGCHANGE, WNDCLASSEXW, WS_EX_LAYERED,
    WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
};
use windows::core::{PCWSTR, w};

/// Posted to the daemon window when a row is chosen; `wparam` is the command id ([`cmd_id`]).
pub const WM_TRAY_POPUP_CMD: u32 = ipc::WM_OT_INTERNAL_BASE + 0x40;
const CLASS_NAME: PCWSTR = w!("OpenTalkTrayPopup");
const WM_MOUSELEAVE: u32 = 0x02A3;
const WM_DPICHANGED: u32 = 0x02E0;
const WA_INACTIVE: usize = 0;

/// Everything `open` needs, gathered by the daemon when the tray icon is activated.
pub struct Request {
    pub owner: isize,
    pub content: Content,
    /// Full text of the recent dictations, for "click to copy".
    pub texts: Vec<String>,
    /// The tray icon id, to ask the shell for its rectangle.
    pub icon_id: u32,
    pub keyboard: bool,
    pub dark: bool,
}

struct Popup {
    hwnd: HWND,
    owner: HWND,
    content: Content,
    texts: Vec<String>,
    layout: Layout,
    nav: Nav,
    scale: f32,
    dark: bool,
    dib: Dib,
    origin: (i32, i32),
    tracking: bool,
    opened_at: u64,
}

thread_local! {
    static POPUP: RefCell<Option<Popup>> = const { RefCell::new(None) };
    static LAST_CLOSED: Cell<Option<u64>> = const { Cell::new(None) };
}

fn tick() -> u64 {
    // SAFETY: plain query.
    unsafe { GetTickCount64() }
}

/// Run `f` on the open popup. `None` when there is none or it is already borrowed (a message sent
/// while we were calling into Windows), so a re-entrant message is simply ignored.
fn with<R>(f: impl FnOnce(&mut Popup) -> R) -> Option<R> {
    POPUP.with(|c| c.try_borrow_mut().ok().and_then(|mut b| b.as_mut().map(f)))
}

#[cfg(test)]
pub fn is_open() -> bool {
    POPUP.with(|c| c.try_borrow().map(|b| b.is_some()).unwrap_or(true))
}

/// A tray activation: close the popup if it is open, else open it (with the echo and re-click guards).
pub fn activate(req: Request) {
    let opened_at = POPUP.with(|c| c.try_borrow().ok().and_then(|b| b.as_ref().map(|p| p.opened_at)));
    match popup::decide(opened_at, LAST_CLOSED.with(Cell::get), tick()) {
        Decision::Open => open(req),
        Decision::Close => close(),
        Decision::Ignore => {}
    }
}

/// Destroy the popup (no-op when closed) and remember when, for the re-click guard.
pub fn close() {
    let taken = POPUP.with(|c| c.try_borrow_mut().ok().and_then(|mut b| b.take()));
    if let Some(p) = taken {
        LAST_CLOSED.with(|c| c.set(Some(tick())));
        // SAFETY: the window belongs to this thread; the state is already out of the cell, so the
        // messages DestroyWindow sends find no popup.
        unsafe {
            let _ = ReleaseCapture();
            let _ = DestroyWindow(p.hwnd);
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Where the popup goes
// ---------------------------------------------------------------------------------------------

struct Geometry {
    scale: f32,
    /// Top-left of the card in screen px.
    card: (i32, i32),
}

fn irect(r: RECT) -> IRect {
    IRect::new(r.left, r.top, r.right, r.bottom)
}

fn monitor_info(hmon: HMONITOR) -> Option<(IRect, IRect)> {
    let mut mi = MONITORINFO { cbSize: size_of::<MONITORINFO>() as u32, ..Default::default() };
    // SAFETY: valid out-pointer with cbSize set.
    unsafe { GetMonitorInfoW(hmon, &mut mi) }.as_bool().then(|| (irect(mi.rcMonitor), irect(mi.rcWork)))
}

fn monitor_scale(hmon: HMONITOR) -> f32 {
    let (mut x, mut y) = (96u32, 96u32);
    // SAFETY: valid out-pointers.
    unsafe {
        let _ = GetDpiForMonitor(hmon, MDT_EFFECTIVE_DPI, &mut x, &mut y);
    }
    scale_from_dpi(x)
}

/// The shell's rectangle for our tray icon, when it has one (`None` while the icon is in the hidden
/// overflow area or the shell does not answer).
fn icon_rect(owner: HWND, id: u32) -> Option<IRect> {
    let nii = NOTIFYICONIDENTIFIER { cbSize: size_of::<NOTIFYICONIDENTIFIER>() as u32, hWnd: owner, uID: id, ..Default::default() };
    // SAFETY: `nii` is fully initialised and outlives the call.
    unsafe { Shell_NotifyIconGetRect(&nii) }.ok().map(irect)
}

/// The primary taskbar's edge and rectangle (`ABM_GETTASKBARPOS`).
fn taskbar_pos() -> Option<(Edge, IRect)> {
    let mut abd = APPBARDATA { cbSize: size_of::<APPBARDATA>() as u32, ..Default::default() };
    // SAFETY: valid APPBARDATA with cbSize set.
    if unsafe { SHAppBarMessage(ABM_GETTASKBARPOS, &mut abd) } == 0 {
        return None;
    }
    let edge = match abd.uEdge {
        ABE_LEFT => Edge::Left,
        ABE_TOP => Edge::Top,
        ABE_RIGHT => Edge::Right,
        ABE_BOTTOM => Edge::Bottom,
        _ => return None,
    };
    Some((edge, irect(abd.rc)))
}

fn geometry(owner: HWND, icon_id: u32, layout: &Layout) -> Geometry {
    let icon = icon_rect(owner, icon_id);
    let mut cursor = POINT::default();
    // SAFETY: valid out-pointer.
    unsafe {
        let _ = GetCursorPos(&mut cursor);
    }
    let at = icon.filter(|r| !r.is_empty()).map_or((cursor.x, cursor.y), |r| (r.cx(), r.cy()));
    // SAFETY: plain query.
    let hmon = unsafe { MonitorFromPoint(POINT { x: at.0, y: at.1 }, MONITOR_DEFAULTTONEAREST) };
    let scale = monitor_scale(hmon);
    let Some((monitor, mut work)) = monitor_info(hmon) else {
        return Geometry { scale, card: (cursor.x, cursor.y) };
    };
    let bar = taskbar_pos().filter(|(_, r)| r.intersects(&monitor));
    let edge = popup::taskbar_edge(monitor, work, bar.map(|b| b.0));
    if let Some((e, r)) = bar {
        work = popup::work_without_hidden_bar(monitor, work, e, r);
    }
    let anchor = icon.filter(|r| popup::anchor_usable(*r, monitor, work));
    let (cw, ch) = ((layout.card_w * scale).round() as i32, (layout.card_h * scale).round() as i32);
    let gap = (popup::EDGE_GAP * scale).round() as i32;
    Geometry { scale, card: popup::place(cw, ch, anchor, work, edge, gap) }
}

// ---------------------------------------------------------------------------------------------
// Window
// ---------------------------------------------------------------------------------------------

fn register_class() -> bool {
    use std::sync::atomic::{AtomicBool, Ordering};
    static DONE: AtomicBool = AtomicBool::new(false);
    if DONE.load(Ordering::Relaxed) {
        return true;
    }
    // SAFETY: registering a plain window class for this module.
    unsafe {
        let Ok(inst) = GetModuleHandleW(PCWSTR::null()) else { return false };
        let wc = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(wndproc),
            hInstance: inst.into(),
            lpszClassName: CLASS_NAME,
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            ..Default::default()
        };
        // 0 also when the class already exists (a second registration after a hot reload).
        let ok = RegisterClassExW(&wc) != 0 || windows::Win32::Foundation::GetLastError() == windows::Win32::Foundation::ERROR_CLASS_ALREADY_EXISTS;
        DONE.store(ok, Ordering::Relaxed);
        ok
    }
}

fn open(req: Request) {
    close();
    if !register_class() {
        crate::log_warn!("tray popup: window class not registered");
        return;
    }
    let owner = HWND(req.owner as *mut core::ffi::c_void);
    let layout = popup::layout(&req.content);
    let geo = geometry(owner, req.icon_id, &layout);
    let (w, h) = popup_draw::pixel_size(&layout, geo.scale);
    let margin = (MARGIN * geo.scale).round() as i32;
    let origin = (geo.card.0 - margin, geo.card.1 - margin);
    let Some(dib) = Dib::new(w as i32, h as i32) else { return };
    // SAFETY: creating a top-level popup on this thread; the class was registered above.
    let hwnd = unsafe {
        let inst = GetModuleHandleW(PCWSTR::null()).ok();
        CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
            CLASS_NAME,
            w!("OpenTalk"),
            WS_POPUP,
            origin.0,
            origin.1,
            w as i32,
            h as i32,
            None,
            None,
            inst.map(Into::into),
            None,
        )
    };
    let Ok(hwnd) = hwnd else {
        crate::log_warn!("tray popup: CreateWindowExW failed");
        return;
    };
    let nav = if req.keyboard { Nav::for_keyboard(&layout) } else { Nav::default() };
    let mut p = Popup { hwnd, owner, content: req.content, texts: req.texts, layout, nav, scale: geo.scale, dark: req.dark, dib, origin, tracking: false, opened_at: tick() };
    present(&mut p);
    POPUP.with(|c| *c.borrow_mut() = Some(p));
    // SAFETY: our window; activating it gives it the keyboard and lets us see the deactivation.
    unsafe {
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = SetForegroundWindow(hwnd);
        if GetForegroundWindow() != hwnd {
            // The system refused to activate us: take the mouse so a click elsewhere still dismisses.
            let _ = SetCapture(hwnd);
        }
        let _ = SetWindowPos(hwnd, Some(HWND_TOPMOST), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
    }
}

/// Render the current state and push it to the layered window.
fn present(p: &mut Popup) {
    let view = View { content: &p.content, layout: &p.layout, nav: p.nav, dark: p.dark, scale: p.scale, text: true };
    let Some(pm) = popup_draw::render(&view) else { return };
    p.dib.upload(pm.data(), DevRect::full(p.dib.w, p.dib.h));
    let pos = POINT { x: p.origin.0, y: p.origin.1 };
    let size = SIZE { cx: p.dib.w, cy: p.dib.h };
    let src = POINT { x: 0, y: 0 };
    let blend = BLENDFUNCTION { BlendOp: AC_SRC_OVER as u8, BlendFlags: 0, SourceConstantAlpha: 255, AlphaFormat: AC_SRC_ALPHA as u8 };
    // SAFETY: all pointers reference live locals; the DIB's DC stays selected.
    unsafe {
        let _ = UpdateLayeredWindow(p.hwnd, None, Some(&pos), Some(&size), Some(p.dib.dc), Some(&src), windows::Win32::Foundation::COLORREF(0), Some(&blend), ULW_ALPHA);
    }
}

/// Run the chosen row and close.
fn choose(target: Target) {
    let (owner, text) = with(|p| {
        let text = match target {
            Target::Recent(i) => p.texts.get(i).cloned(),
            _ => None,
        };
        (p.owner, text)
    })
    .unwrap_or_default();
    close();
    if let Some(text) = text {
        if !crate::daemon::paste::copy_to_clipboard(&text) {
            crate::log_warn!("tray popup: could not copy to the clipboard");
        }
    } else if let Some(cmd) = target.cmd() {
        run(owner, cmd);
    }
}

/// Post `cmd` to the daemon window.
fn run(owner: HWND, cmd: TrayCmd) {
    // SAFETY: posting to the daemon's window; a dead window just drops the message.
    unsafe {
        let _ = PostMessageW(Some(owner), WM_TRAY_POPUP_CMD, WPARAM(cmd_id(cmd)), LPARAM(0));
    }
}

/// Client pixel -> card-local logical point.
fn to_card(p: &Popup, lparam: LPARAM) -> (f32, f32) {
    let (x, y) = ((lparam.0 & 0xFFFF) as i16 as f32, ((lparam.0 >> 16) & 0xFFFF) as i16 as f32);
    (x / p.scale - MARGIN, y / p.scale - MARGIN)
}

fn track_leave(p: &mut Popup) {
    if p.tracking {
        return;
    }
    let mut tme = TRACKMOUSEEVENT { cbSize: size_of::<TRACKMOUSEEVENT>() as u32, dwFlags: TME_LEAVE, hwndTrack: p.hwnd, dwHoverTime: 0 };
    // SAFETY: valid struct for our window.
    p.tracking = unsafe { TrackMouseEvent(&mut tme) }.is_ok();
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_ACTIVATE if wparam.0 & 0xFFFF == WA_INACTIVE => {
            // Deactivated: another window was clicked or focused. Destroying from inside the
            // activation message is fine, but post it so the state is not torn down mid-call.
            // SAFETY: posting to our own window.
            unsafe {
                let _ = PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
            }
            LRESULT(0)
        }
        WM_CLOSE | WM_DPICHANGED | WM_DISPLAYCHANGE | WM_SETTINGCHANGE => {
            close();
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1),
        WM_SETCURSOR => {
            // SAFETY: stock cursor.
            unsafe { SetCursor(LoadCursorW(None, IDC_ARROW).ok()) };
            LRESULT(1)
        }
        WM_MOUSEMOVE => {
            with(|p| {
                let pt = to_card(p, lparam);
                track_leave(p);
                if p.nav.pointer(&p.layout, Some(pt)) {
                    present(p);
                }
            });
            LRESULT(0)
        }
        WM_MOUSELEAVE => {
            with(|p| {
                p.tracking = false;
                if p.nav.pointer(&p.layout, None) {
                    present(p);
                }
            });
            LRESULT(0)
        }
        WM_LBUTTONDOWN => {
            let outside = with(|p| {
                let (x, y) = to_card(p, lparam);
                let inside = x >= 0.0 && y >= 0.0 && x < p.layout.card_w && y < p.layout.card_h;
                if inside {
                    if p.nav.press(&p.layout, (x, y)) {
                        present(p);
                    }
                    // SAFETY: our window; capture makes sure the button-up comes back to us.
                    unsafe {
                        let _ = SetCapture(p.hwnd);
                    }
                }
                !inside
            });
            if outside == Some(true) {
                close();
            }
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            let target = with(|p| {
                let pt = to_card(p, lparam);
                // SAFETY: releasing our own capture.
                unsafe {
                    let _ = ReleaseCapture();
                }
                let t = p.nav.release(&p.layout, pt);
                if t.is_none() {
                    present(p);
                }
                t
            })
            .flatten();
            if let Some(t) = target {
                choose(t);
            }
            LRESULT(0)
        }
        WM_KEYDOWN => {
            // SAFETY: plain query.
            let shift = unsafe { GetKeyState(VK_SHIFT.0 as i32) } < 0;
            if let Some(key) = popup::key_from_vk(wparam.0 as u32, shift) {
                let out = with(|p| {
                    let o = p.nav.key(&p.layout, key);
                    if o == KeyOutcome::Moved {
                        present(p);
                    }
                    o
                });
                match out {
                    Some(KeyOutcome::Close) => close(),
                    Some(KeyOutcome::Choose(t)) => choose(t),
                    _ => {}
                }
            }
            LRESULT(0)
        }
        // SAFETY: default handling of every other message.
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}
