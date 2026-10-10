//! Tray popup, Win32 side: one layered popup window on the daemon's UI thread, created when the tray
//! icon is activated and destroyed as soon as it closes, so a closed popup costs nothing (no window,
//! no bitmap; the only timer, for the status strip, exists while a toast is showing). It takes the
//! keyboard focus and dismisses itself when it loses activation, which also covers a click anywhere
//! else.
//!
//! The window is `WS_EX_LAYERED | TOOLWINDOW | TOPMOST` and shows a premultiplied BGRA frame
//! rendered by `popup_draw` (the Home menu) or `page::draw` (every other route). Home rows that open
//! a page navigate inside the same window: the window is resized and re-placed (the bottom edge stays
//! put above a bottom taskbar), Back returns. A chosen command row is posted to the daemon's window
//! as [`WM_TRAY_POPUP_CMD`]; settings changes and other page actions reach the daemon through the
//! [`page::mailbox`] followed by [`WM_TRAY_POPUP_ACT`].

use super::page::draw::{self as page_draw, Frame};
use super::page::metrics::GdiMetrics;
use super::page::pager::{Outward, PResp, Pager};
use super::page::session::{Clip, CursorKind, K, KeyEv, key_from_vk};
use super::page::{self, Route, TestResult, Waker};
use super::popup::{self, Content, Decision, Edge, IRect, KeyOutcome, Layout, MARGIN, Nav, Target};
use super::popup_draw::{self, View};
use super::{TrayCmd, cmd_id};
use crate::core::config::Config;
use crate::core::ipc;
use crate::daemon::pill::kit::{DevRect, Dib, resolve_dark, scale_from_dpi};
use std::cell::{Cell, RefCell};
use std::sync::Mutex;
use std::sync::atomic::{AtomicIsize, Ordering};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM};
use windows::Win32::Graphics::Gdi::{AC_SRC_ALPHA, AC_SRC_OVER, BLENDFUNCTION, GetMonitorInfoW, HMONITOR, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromPoint};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::SystemInformation::GetTickCount64;
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::Input::Ime::{CANDIDATEFORM, CFS_CANDIDATEPOS, CFS_POINT, COMPOSITIONFORM, ImmGetContext, ImmReleaseContext, ImmSetCandidateWindow, ImmSetCompositionWindow};
use windows::Win32::UI::Input::KeyboardAndMouse::{GetKeyState, ReleaseCapture, SetCapture, TME_LEAVE, TRACKMOUSEEVENT, TrackMouseEvent, VK_CONTROL, VK_MENU, VK_SHIFT};
use windows::Win32::UI::Shell::{ABE_BOTTOM, ABE_LEFT, ABE_RIGHT, ABE_TOP, ABM_GETTASKBARPOS, APPBARDATA, NOTIFYICONIDENTIFIER, SHAppBarMessage, Shell_NotifyIconGetRect, ShellExecuteW};
use windows::Win32::UI::WindowsAndMessaging::{
    CS_DBLCLKS, CreateWindowExW, DefWindowProcW, DestroyWindow, GetCursorPos, GetForegroundWindow, HWND_TOPMOST, IDC_ARROW, IDC_HAND, IDC_IBEAM, KillTimer, LoadCursorW, PostMessageW,
    RegisterClassExW, SW_SHOW, SW_SHOWNORMAL, SetCursor, SetForegroundWindow, SetTimer, SetWindowPos, ShowWindow, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, ULW_ALPHA, UpdateLayeredWindow,
    WM_ACTIVATE, WM_APP, WM_CHAR, WM_CLOSE, WM_DISPLAYCHANGE, WM_ERASEBKGND, WM_IME_COMPOSITION, WM_IME_STARTCOMPOSITION, WM_KEYDOWN, WM_LBUTTONDBLCLK, WM_LBUTTONDOWN, WM_LBUTTONUP,
    WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_SETCURSOR, WM_SETTINGCHANGE, WM_SYSKEYDOWN, WM_TIMER, WNDCLASSEXW, WS_EX_LAYERED, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
};
use windows::core::{HSTRING, PCWSTR, w};

/// Posted to the daemon window when a row is chosen; `wparam` is the command id ([`cmd_id`]).
pub const WM_TRAY_POPUP_CMD: u32 = ipc::WM_OT_INTERNAL_BASE + 0x40;
/// Posted to the daemon window after the popup queued actions in the [`page::mailbox`]: drain it,
/// apply each [`page::PopupAction`], save, then hand a fresh config back with [`set_config`].
pub const WM_TRAY_POPUP_ACT: u32 = ipc::WM_OT_INTERNAL_BASE + 0x41;
/// Posted to the popup window by a worker: call the current page's `wake`.
const WM_POPUP_WAKE: u32 = WM_APP + 0x20;
/// Posted to the popup window: a provider test finished (see [`deliver_test`]).
const WM_POPUP_TEST: u32 = WM_APP + 0x21;
const TIMER_TOAST: usize = 1;
const CLASS_NAME: PCWSTR = w!("OpenTalkTrayPopup");
const WM_MOUSELEAVE: u32 = 0x02A3;
const WM_DPICHANGED: u32 = 0x02E0;
const WA_INACTIVE: usize = 0;
const MK_LBUTTON: usize = 0x0001;

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
    /// What to show first: `Home` is the menu, anything else a page.
    pub route: Route,
}

/// Where the popup is anchored, computed once at open so a page change can re-place the card.
#[derive(Clone, Copy)]
struct Anchor {
    icon: Option<IRect>,
    work: IRect,
    edge: Edge,
    scale: f32,
}

struct Popup {
    hwnd: HWND,
    owner: HWND,
    content: Content,
    texts: Vec<String>,
    layout: Layout,
    nav: Nav,
    /// `Some` while a page (not Home) is showing.
    pager: Option<Pager>,
    metrics: GdiMetrics,
    anchor: Anchor,
    /// Card height of every page (logical px).
    page_h: f32,
    scale: f32,
    dark: bool,
    dib: Dib,
    origin: (i32, i32),
    tracking: bool,
    opened_at: u64,
    /// Whether the hotkey capture was started (so closing ends it exactly once).
    capturing: bool,
    surrogate: page::edit::Utf16Acc,
}

thread_local! {
    static POPUP: RefCell<Option<Popup>> = const { RefCell::new(None) };
    static LAST_CLOSED: Cell<Option<u64>> = const { Cell::new(None) };
}

/// The popup window, for workers that deliver results (0 when closed).
static POPUP_HWND: AtomicIsize = AtomicIsize::new(0);
static TEST_RESULTS: Mutex<Vec<TestResult>> = Mutex::new(Vec::new());
/// The two functions that start and stop the daemon's hotkey capture.
type CaptureHooks = (fn(), fn());
/// Start / stop the daemon's in-process hotkey capture; set by the host at startup.
static CAPTURE_HOOKS: Mutex<Option<CaptureHooks>> = Mutex::new(None);

fn tick() -> u64 {
    // SAFETY: plain query.
    unsafe { GetTickCount64() }
}

/// Run `f` on the open popup. `None` when there is none or it is already borrowed (a message sent
/// while we were calling into Windows), so a re-entrant message is simply ignored.
fn with<R>(f: impl FnOnce(&mut Popup) -> R) -> Option<R> {
    POPUP.with(|c| c.try_borrow_mut().ok().and_then(|mut b| b.as_mut().map(f)))
}

pub fn is_open() -> bool {
    POPUP.with(|c| c.try_borrow().map(|b| b.is_some()).unwrap_or(true))
}

/// Register the functions that start and stop the in-daemon hotkey capture (`hotkey::begin_capture`
/// / `hotkey::end_capture`). Until then a hotkey field simply shows "Press the keys" and waits.
#[allow(dead_code)] // called by the host once it is wired (merge step)
pub fn set_capture_hooks(begin: fn(), end: fn()) {
    *CAPTURE_HOOKS.lock().unwrap_or_else(|e| e.into_inner()) = Some((begin, end));
}

fn capture_hook(begin: bool) {
    let hooks = *CAPTURE_HOOKS.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((b, e)) = hooks {
        if begin { b() } else { e() }
    }
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

/// Show `route` now: navigate the open popup there, or open one (no toggle, no echo guard). This is
/// what `--settings <page>`, a second launch, the first run and the pill's menu use.
#[allow(dead_code)] // called by the host once it is wired (merge step)
pub fn open_route(req: Request) {
    if is_open() {
        let route = req.route;
        with(|p| {
            p.content = req.content;
            p.texts = req.texts;
            p.layout = popup::layout(&p.content);
        });
        navigate(route);
    } else {
        open(req);
    }
}

/// Navigate the open popup to `route` (Home shows the menu).
#[allow(dead_code)] // called by the host once it is wired (merge step)
pub fn navigate(route: Route) {
    with(|p| goto(p, route));
}

/// The daemon's state changed while the popup is open (a hotkey press started or stopped a
/// dictation, a transcription finished): show the new state in place, so the Start / Stop row never
/// lies. A change that would resize the Home card (a Retry or recent row appearing) closes the popup
/// instead; the next click opens it with the new rows. A page is never closed for it: the Home
/// content is only stored for when the user goes back.
pub fn refresh(content: Content, texts: Vec<String>) {
    let resized = with(|p| {
        if p.content == content && p.texts == texts {
            return false;
        }
        let layout = popup::layout(&content);
        if p.pager.is_some() {
            (p.content, p.texts, p.layout) = (content, texts, layout);
            return false;
        }
        if layout.card_h != p.layout.card_h || layout.rows.len() != p.layout.rows.len() {
            return true;
        }
        if p.nav.active.is_some_and(|i| !layout.rows[i].selectable()) {
            p.nav.active = None;
            p.nav.pressed = false;
        }
        (p.content, p.texts, p.layout) = (content, texts, layout);
        present(p);
        false
    });
    if resized == Some(true) {
        close();
    }
}

/// A fresh config from the daemon (after it applied a `SetConfig`): the open page re-reads it, and the
/// theme follows at once.
#[allow(dead_code)] // called by the host once it is wired (merge step)
pub fn set_config(cfg: Config) {
    with(|p| {
        let dark = resolve_dark(cfg.theme);
        if let Some(pg) = p.pager.as_mut() {
            pg.set_cfg(cfg, &p.metrics);
            pg.dark = dark;
            p.dark = dark;
            present(p);
        } else if p.dark != dark {
            p.dark = dark;
            present(p);
        }
    });
}

/// A provider test finished on a worker thread. Safe to call from any thread; dropped when no popup
/// is open any more.
#[allow(dead_code)] // called by the host once it is wired (merge step)
pub fn deliver_test(result: TestResult) {
    let hwnd = POPUP_HWND.load(Ordering::Acquire);
    if hwnd == 0 {
        return;
    }
    TEST_RESULTS.lock().unwrap_or_else(|e| e.into_inner()).push(result);
    // SAFETY: posting to a window; if it died meanwhile the message is dropped.
    unsafe {
        let _ = PostMessageW(Some(HWND(hwnd as *mut core::ffi::c_void)), WM_POPUP_TEST, WPARAM(0), LPARAM(0));
    }
}

/// The hotkey capture's live preview ("Control+Super" so far); call on the UI thread.
#[allow(dead_code)] // called by the host once it is wired (merge step)
pub fn capture_update(text: String) {
    with(|p| {
        let mut clip = WinClip;
        if let Some(pg) = p.pager.as_mut() {
            let r = pg.capture_update(text, &p.metrics, &mut clip);
            apply(p, r);
        }
    });
}

/// The hotkey capture finished with this accelerator; call on the UI thread.
#[allow(dead_code)] // called by the host once it is wired (merge step)
pub fn capture_done(accel: String) {
    with(|p| {
        let mut clip = WinClip;
        if let Some(pg) = p.pager.as_mut() {
            p.capturing = false;
            let r = pg.capture_done(accel, &p.metrics, &mut clip);
            apply(p, r);
        }
    });
}

/// Destroy the popup (no-op when closed) and remember when, for the re-click guard. Dirty fields are
/// committed and a running hotkey capture is ended first, so closing never loses an edit.
pub fn close() {
    let taken = POPUP.with(|c| c.try_borrow_mut().ok().and_then(|mut b| b.take()));
    if let Some(mut p) = taken {
        POPUP_HWND.store(0, Ordering::Release);
        if let Some(mut pg) = p.pager.take() {
            let mut clip = WinClip;
            let r = pg.close(&p.metrics, &mut clip);
            apply_outs(&mut p, r);
        }
        if std::mem::take(&mut p.capturing) {
            capture_hook(false);
        }
        LAST_CLOSED.with(|c| c.set(Some(tick())));
        // SAFETY: the window belongs to this thread; the state is already out of the cell, so the
        // messages DestroyWindow sends find no popup.
        unsafe {
            let _ = KillTimer(Some(p.hwnd), TIMER_TOAST);
            let _ = ReleaseCapture();
            let _ = DestroyWindow(p.hwnd);
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Where the popup goes
// ---------------------------------------------------------------------------------------------

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

fn anchor(owner: HWND, icon_id: u32) -> Anchor {
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
        let work = IRect::new(cursor.x - 2000, cursor.y - 2000, cursor.x + 2000, cursor.y + 2000);
        return Anchor { icon: None, work, edge: Edge::Bottom, scale };
    };
    let bar = taskbar_pos().filter(|(_, r)| r.intersects(&monitor));
    let edge = popup::taskbar_edge(monitor, work, bar.map(|b| b.0));
    if let Some((e, r)) = bar {
        work = popup::work_without_hidden_bar(monitor, work, e, r);
    }
    let icon = icon.filter(|r| popup::anchor_usable(*r, monitor, work));
    Anchor { icon, work, edge, scale }
}

/// Top-left of a `w` x `h` logical card in screen px.
fn place_card(a: &Anchor, w: f32, h: f32) -> (i32, i32) {
    let (cw, ch) = ((w * a.scale).round() as i32, (h * a.scale).round() as i32);
    let gap = (popup::EDGE_GAP * a.scale).round() as i32;
    popup::place(cw, ch, a.icon, a.work, a.edge, gap)
}

// ---------------------------------------------------------------------------------------------
// Window
// ---------------------------------------------------------------------------------------------

fn register_class() -> bool {
    use std::sync::atomic::AtomicBool;
    static DONE: AtomicBool = AtomicBool::new(false);
    if DONE.load(Ordering::Relaxed) {
        return true;
    }
    // SAFETY: registering a plain window class for this module.
    unsafe {
        let Ok(inst) = GetModuleHandleW(PCWSTR::null()) else { return false };
        let wc = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            style: CS_DBLCLKS,
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

/// Window size in device px of the current view.
fn view_px(p: &Popup) -> (u32, u32) {
    match &p.pager {
        Some(pg) => page_draw::window_px(pg.card_h, p.scale),
        None => popup_draw::pixel_size(&p.layout, p.scale),
    }
}

/// The card of the current view in logical px.
fn card_size(p: &Popup) -> (f32, f32) {
    match &p.pager {
        Some(pg) => (page::CARD_W_PAGE, pg.card_h),
        None => (p.layout.card_w, p.layout.card_h),
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
    let anchor = anchor(owner, req.icon_id);
    let page_h = popup::page_card_h(anchor.work.h(), anchor.scale);
    let scale = anchor.scale;
    let home = req.route == Route::Home;
    let (cw, ch) = if home { (layout.card_w, layout.card_h) } else { (page::CARD_W_PAGE, page_h) };
    let card = place_card(&anchor, cw, ch);
    let margin = (MARGIN * scale).round() as i32;
    let origin = (card.0 - margin, card.1 - margin);
    let (w, h) = if home { popup_draw::pixel_size(&layout, scale) } else { page_draw::window_px(page_h, scale) };
    let Some(dib) = Dib::new(w as i32, h as i32) else { return };
    // SAFETY: creating a top-level popup on this thread; the class was registered above.
    let hwnd = unsafe {
        let inst = GetModuleHandleW(PCWSTR::null()).ok();
        CreateWindowExW(WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_TOPMOST, CLASS_NAME, w!("OpenTalk"), WS_POPUP, origin.0, origin.1, w as i32, h as i32, None, None, inst.map(Into::into), None)
    };
    let Ok(hwnd) = hwnd else {
        crate::log_warn!("tray popup: CreateWindowExW failed");
        return;
    };
    let nav = if req.keyboard { Nav::for_keyboard(&layout) } else { Nav::default() };
    let mut p = Popup {
        hwnd,
        owner,
        content: req.content,
        texts: req.texts,
        layout,
        nav,
        pager: None,
        metrics: GdiMetrics { scale },
        anchor,
        page_h,
        scale,
        dark: req.dark,
        dib,
        origin,
        tracking: false,
        opened_at: tick(),
        capturing: false,
        surrogate: Default::default(),
    };
    if !home {
        start_pager(&mut p, req.route);
    }
    present(&mut p);
    POPUP.with(|c| *c.borrow_mut() = Some(p));
    POPUP_HWND.store(hwnd.0 as isize, Ordering::Release);
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

/// The wake-up a worker uses to reach the page (posts to this popup's window).
fn make_waker(hwnd: HWND) -> Waker {
    let h = hwnd.0 as isize;
    std::sync::Arc::new(move || {
        // SAFETY: posting to a window; if it died meanwhile the message is dropped.
        unsafe {
            let _ = PostMessageW(Some(HWND(h as *mut core::ffi::c_void)), WM_POPUP_WAKE, WPARAM(0), LPARAM(0));
        }
    })
}

/// Create the pager (the working config is read from disk once) and show `route` in it.
fn start_pager(p: &mut Popup, route: Route) {
    let mut pg = Pager::new(Config::load(), p.dark, p.page_h, make_waker(p.hwnd));
    let mut clip = WinClip;
    let r = pg.open(route, &p.metrics, &mut clip);
    p.pager = Some(pg);
    apply_outs(p, r);
}

/// Show `route`: a page, or Home.
fn goto(p: &mut Popup, route: Route) {
    if route == Route::Home {
        go_home(p);
        return;
    }
    if let Some(pg) = p.pager.as_mut() {
        let mut clip = WinClip;
        let r = pg.open(route, &p.metrics, &mut clip);
        apply(p, r);
        return;
    }
    start_pager(p, route);
    resize_view(p);
    present(p);
}

fn go_home(p: &mut Popup) {
    if let Some(mut pg) = p.pager.take() {
        let mut clip = WinClip;
        let r = pg.close(&p.metrics, &mut clip);
        apply_outs(p, r);
    }
    if std::mem::take(&mut p.capturing) {
        capture_hook(false);
    }
    p.nav = Nav::default();
    // SAFETY: our window.
    unsafe {
        let _ = KillTimer(Some(p.hwnd), TIMER_TOAST);
    }
    resize_view(p);
    present(p);
}

/// The view changed size (Home <-> page): new bitmap, re-placed card.
fn resize_view(p: &mut Popup) {
    let (w, h) = view_px(p);
    if (w as i32, h as i32) != (p.dib.w, p.dib.h) {
        let Some(dib) = Dib::new(w as i32, h as i32) else { return };
        p.dib = dib;
    }
    let (cw, ch) = card_size(p);
    let card = place_card(&p.anchor, cw, ch);
    let margin = (MARGIN * p.scale).round() as i32;
    p.origin = (card.0 - margin, card.1 - margin);
}

/// Render the current state and push it to the layered window.
fn present(p: &mut Popup) {
    let pm = match &p.pager {
        Some(pg) => page_draw::render(&Frame { pager: pg, scale: p.scale, text: true, m: &p.metrics }),
        None => popup_draw::render(&View { content: &p.content, layout: &p.layout, nav: p.nav, dark: p.dark, scale: p.scale, text: true }),
    };
    let Some(pm) = pm else { return };
    if (pm.width() as i32, pm.height() as i32) != (p.dib.w, p.dib.h) {
        return;
    }
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

// ---------------------------------------------------------------------------------------------
// Pager plumbing
// ---------------------------------------------------------------------------------------------

/// The system clipboard for the page editor.
struct WinClip;

impl Clip for WinClip {
    fn get(&mut self) -> Option<String> {
        crate::daemon::paste::read_clipboard_text()
    }
    fn set(&mut self, text: &str) -> bool {
        crate::daemon::paste::copy_to_clipboard(text)
    }
}

/// Carry out what the pager asked for (everything but redrawing).
fn apply_outs(p: &mut Popup, r: PResp) {
    for o in r.out {
        match o {
            Outward::CopyText(t) => {
                if !crate::daemon::paste::copy_to_clipboard(&t) {
                    crate::log_warn!("tray popup: could not copy to the clipboard");
                }
            }
            Outward::OpenPath(path) => {
                let target = HSTRING::from(path.as_os_str());
                // SAFETY: plain shell call with owned strings.
                unsafe {
                    ShellExecuteW(None, w!("open"), &target, PCWSTR::null(), PCWSTR::null(), SW_SHOWNORMAL);
                }
            }
            Outward::Post(a) => {
                page::mailbox::push(a);
                // SAFETY: posting to the daemon's window; a dead window just drops the message.
                unsafe {
                    let _ = PostMessageW(Some(p.owner), WM_TRAY_POPUP_ACT, WPARAM(0), LPARAM(0));
                }
            }
            Outward::BeginCapture => {
                p.capturing = true;
                capture_hook(true);
            }
            Outward::EndCapture => {
                if std::mem::take(&mut p.capturing) {
                    capture_hook(false);
                }
            }
            Outward::ToastTimer(arm) => {
                // SAFETY: our window; a one-shot timer that exists only while a toast shows.
                unsafe {
                    if arm {
                        SetTimer(Some(p.hwnd), TIMER_TOAST, page::pager::TOAST_MS, None);
                    } else {
                        let _ = KillTimer(Some(p.hwnd), TIMER_TOAST);
                    }
                }
            }
        }
    }
}

/// `apply_outs` plus the state changes and the redraw.
fn apply(p: &mut Popup, r: PResp) {
    let (home, saved, redraw) = (r.home, r.saved, r.redraw);
    apply_outs(p, r);
    if let (true, Some(pg)) = (saved, p.pager.as_mut()) {
        let dark = resolve_dark(pg.cfg.theme);
        pg.dark = dark;
        p.dark = dark;
    }
    if home {
        go_home(p);
    } else if redraw || saved {
        present(p);
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

/// Run the chosen Home row: a page row navigates inside the window, the others run and close.
fn choose(target: Target) {
    if let Some(route) = target.route() {
        with(|p| goto(p, route));
        return;
    }
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

fn key_down(vk: windows::Win32::UI::Input::KeyboardAndMouse::VIRTUAL_KEY) -> bool {
    // SAFETY: plain query.
    unsafe { GetKeyState(vk.0 as i32) < 0 }
}

/// Tell the IME where the caret is so its composition and candidate windows sit under the text.
fn place_ime(p: &Popup) {
    let Some(pg) = p.pager.as_ref() else { return };
    let Some((x, y)) = pg.caret_card_pos(&p.metrics) else { return };
    let pt = POINT { x: ((x + MARGIN) * p.scale).round() as i32, y: ((y + MARGIN) * p.scale).round() as i32 };
    // SAFETY: IMM calls on our own window; the context is released.
    unsafe {
        let himc = ImmGetContext(p.hwnd);
        if himc.is_invalid() {
            return;
        }
        let cf = COMPOSITIONFORM { dwStyle: CFS_POINT, ptCurrentPos: POINT { x: pt.x, y: pt.y - (page::edit::LINE_H * p.scale).round() as i32 }, rcArea: RECT::default() };
        let _ = ImmSetCompositionWindow(himc, &cf);
        let cand = CANDIDATEFORM { dwIndex: 0, dwStyle: CFS_CANDIDATEPOS, ptCurrentPos: pt, rcArea: RECT::default() };
        let _ = ImmSetCandidateWindow(himc, &cand);
        let _ = ImmReleaseContext(p.hwnd, himc);
    }
}

/// Run `f` on the pager of the open popup and apply the result.
fn on_pager(f: impl FnOnce(&mut Pager, &GdiMetrics, &mut WinClip) -> PResp) {
    with(|p| {
        let mut clip = WinClip;
        let Some(pg) = p.pager.as_mut() else { return };
        let r = f(pg, &p.metrics, &mut clip);
        apply(p, r);
    });
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
            let kind = with(|p| p.pager.as_ref().map_or(CursorKind::Arrow, Pager::cursor)).unwrap_or(CursorKind::Arrow);
            let id = match kind {
                CursorKind::Arrow => IDC_ARROW,
                CursorKind::IBeam => IDC_IBEAM,
                CursorKind::Hand => IDC_HAND,
            };
            // SAFETY: stock cursor.
            unsafe { SetCursor(LoadCursorW(None, id).ok()) };
            LRESULT(1)
        }
        WM_MOUSEMOVE => {
            with(|p| {
                let pt = to_card(p, lparam);
                track_leave(p);
                if p.pager.is_some() {
                    let buttons = wparam.0 & MK_LBUTTON != 0;
                    let mut clip = WinClip;
                    let r = p.pager.as_mut().expect("checked").pointer(Some(pt), buttons, &p.metrics, &mut clip);
                    apply(p, r);
                } else if p.nav.pointer(&p.layout, Some(pt)) {
                    present(p);
                }
            });
            LRESULT(0)
        }
        WM_MOUSELEAVE => {
            with(|p| {
                p.tracking = false;
                if p.pager.is_some() {
                    let mut clip = WinClip;
                    let r = p.pager.as_mut().expect("checked").leave(&p.metrics, &mut clip);
                    apply(p, r);
                } else if p.nav.pointer(&p.layout, None) {
                    present(p);
                }
            });
            LRESULT(0)
        }
        WM_MOUSEWHEEL => {
            let delta = ((wparam.0 >> 16) & 0xFFFF) as i16 as i32;
            on_pager(|pg, _, _| pg.wheel(delta));
            LRESULT(0)
        }
        WM_LBUTTONDOWN | WM_LBUTTONDBLCLK => {
            let outside = with(|p| {
                let (x, y) = to_card(p, lparam);
                let (cw, ch) = card_size(p);
                let inside = x >= 0.0 && y >= 0.0 && x < cw && y < ch;
                if inside {
                    if p.pager.is_some() {
                        let mut clip = WinClip;
                        let r = p.pager.as_mut().expect("checked").press((x, y), msg == WM_LBUTTONDBLCLK, &p.metrics, &mut clip);
                        apply(p, r);
                    } else if p.nav.press(&p.layout, (x, y)) {
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
                if p.pager.is_some() {
                    let mut clip = WinClip;
                    let r = p.pager.as_mut().expect("checked").release(pt, &p.metrics, &mut clip);
                    apply(p, r);
                    return None;
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
        WM_KEYDOWN | WM_SYSKEYDOWN => {
            let (shift, ctrl, alt) = (key_down(VK_SHIFT), key_down(VK_CONTROL), key_down(VK_MENU));
            let vk = wparam.0 as u32;
            if with(|p| p.pager.is_some()).unwrap_or(false) {
                let k = key_from_vk(vk, ctrl).or_else(|| (alt && vk == 0x25).then_some(K::Left));
                if let Some(k) = k {
                    on_pager(|pg, m, clip| pg.key(KeyEv { k, shift, ctrl, alt }, m, clip));
                }
                return LRESULT(0);
            }
            if let Some(key) = popup::key_from_vk(vk, shift) {
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
        WM_CHAR => {
            // Printable text, one UTF-16 unit per message; control chars (Ctrl+letters, Enter, Tab,
            // Esc, Backspace) arrive as keys above.
            let unit = wparam.0 as u16;
            if unit >= 0x20 && unit != 0x7F {
                with(|p| {
                    let Some(c) = p.surrogate.push(unit) else { return };
                    let mut clip = WinClip;
                    let mut buf = [0u8; 4];
                    let Some(pg) = p.pager.as_mut() else { return };
                    let r = pg.text(c.encode_utf8(&mut buf), &p.metrics, &mut clip);
                    apply(p, r);
                });
            }
            LRESULT(0)
        }
        WM_IME_STARTCOMPOSITION | WM_IME_COMPOSITION => {
            with(|p| place_ime(p));
            // SAFETY: default handling shows the composition window where we just put it.
            unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
        }
        WM_TIMER if wparam.0 == TIMER_TOAST => {
            on_pager(|pg, m, _| pg.toast_expired(m));
            LRESULT(0)
        }
        WM_POPUP_WAKE => {
            on_pager(|pg, m, _| pg.wake(m));
            LRESULT(0)
        }
        WM_POPUP_TEST => {
            let results: Vec<TestResult> = std::mem::take(&mut *TEST_RESULTS.lock().unwrap_or_else(|e| e.into_inner()));
            for result in results {
                on_pager(|pg, m, _| pg.test_done(result, m));
            }
            LRESULT(0)
        }
        // SAFETY: default handling of every other message.
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::tray::page::Section;

    fn request(route: Route) -> Request {
        Request { owner: 0, content: Content::default(), texts: Vec::new(), icon_id: 1, keyboard: false, dark: false, route }
    }

    #[test]
    fn the_wake_messages_live_in_the_private_app_range_and_do_not_clash() {
        const { assert!(WM_POPUP_WAKE >= WM_APP && WM_POPUP_TEST > WM_POPUP_WAKE) };
        assert_eq!(WM_TRAY_POPUP_ACT, WM_TRAY_POPUP_CMD + 1);
        const { assert!(WM_TRAY_POPUP_ACT > ipc::WM_OT_INTERNAL_BASE) };
    }

    #[test]
    fn nothing_is_open_in_a_unit_test_and_the_entry_points_are_inert() {
        assert!(!is_open());
        // These are no-ops without a popup (they must never panic or open a window).
        navigate(Route::History);
        set_config(Config::default());
        capture_update("Control".into());
        capture_done("Control+Super".into());
        deliver_test(TestResult { kind: page::TestKind::Stt, ok: true, badge: String::new(), text: String::new() });
        assert!(TEST_RESULTS.lock().unwrap().is_empty(), "a result is dropped when no popup is open");
        assert!(!is_open());
    }

    /// Real desktop: open the popup on a page, walk the stack and close it.
    #[test]
    #[ignore = "needs a real desktop (layered windows, foreground)"]
    fn desktop_popup_opens_a_page_navigates_and_closes() {
        open_route(request(Route::Settings(Section::About)));
        assert!(is_open());
        navigate(Route::History);
        assert!(is_open());
        navigate(Route::Home);
        assert!(is_open(), "Home is the menu, the popup stays");
        close();
        assert!(!is_open());
    }
}
