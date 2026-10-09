//! The Win32 layer: one layered, top-most, no-activate tool window that shows the pre-rendered
//! premultiplied BGRA frame via `UpdateLayeredWindow`; alpha-based hit-testing, hover tracking,
//! click-vs-drag, the right-click menu, per-monitor DPI and the on-demand 60 fps animation timer.

use super::card::CardHover;
use super::model::{Hit, Pointer, RenderState, TickOutcome, window_size};
use super::render::{pixel_size, render_into};
use super::theme::{Palette, resolve_dark};
use super::{PillEvent, PillMode, PillOptions};
use crate::core::config::{PanelPosition, Theme};
use crate::core::timing::now_ms;
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use tiny_skia::Pixmap;
use windows::Win32::Foundation::{COLORREF, HANDLE, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WAIT_OBJECT_0, WPARAM, CloseHandle};
use windows::Win32::Graphics::Gdi::{
    AC_SRC_ALPHA, AC_SRC_OVER, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, BLENDFUNCTION, CreateCompatibleDC, CreateDIBSection, DIB_RGB_COLORS, DeleteDC,
    DeleteObject, GetMonitorInfoW, HBITMAP, HDC, HGDIOBJ, MONITOR_DEFAULTTONEAREST, MONITOR_DEFAULTTOPRIMARY, MONITORINFO, MonitorFromPoint,
    SelectObject, ScreenToClient, HMONITOR,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{
    CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, CancelWaitableTimer, CreateEventW, CreateWaitableTimerExW, INFINITE, SetEvent, SetWaitableTimer,
    TIMER_ALL_ACCESS, WaitForMultipleObjects,
};
use windows::Win32::UI::HiDpi::{DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForMonitor, MDT_EFFECTIVE_DPI, SetThreadDpiAwarenessContext};
use windows::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture, TME_LEAVE, TRACKMOUSEEVENT, TrackMouseEvent};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DestroyWindow, GWLP_USERDATA, GetCursorPos, GetWindowLongPtrW, HMENU,
    HTCLIENT, HTTRANSPARENT, HWND_TOPMOST, IDC_ARROW, IDC_HAND, IDC_NO, IDC_SIZEALL, LoadCursorW, MF_GRAYED, MF_STRING, MA_NOACTIVATE, PostMessageW,
    RegisterClassExW, SPI_SETWORKAREA, SW_HIDE, SW_SHOWNOACTIVATE, SWP_NOACTIVATE, SWP_NOSIZE, SWP_NOZORDER, SetCursor, SetForegroundWindow,
    SetWindowLongPtrW, SetWindowPos, ShowWindow, TPM_BOTTOMALIGN, TPM_RETURNCMD, TPM_RIGHTBUTTON, TrackPopupMenu, ULW_ALPHA, UpdateLayeredWindow,
    WM_APP, WM_DESTROY, WM_DISPLAYCHANGE, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEACTIVATE, WM_MOUSEMOVE, WM_NCHITTEST, WM_NULL,
    WM_RBUTTONUP, WM_SETCURSOR, WM_SETTINGCHANGE, WNDCLASSEXW, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
};
use windows::core::{PCWSTR, w};

pub const WM_APP_TICK: u32 = WM_APP + 0x50;
const WM_MOUSELEAVE: u32 = 0x02A3;
const CLASS_NAME: PCWSTR = w!("OpenTalkPill");
const DRAG_THRESHOLD_PX: f32 = 5.0;

// --- pure helpers (unit-tested) ------------------------------------------------------------

/// Device scale for an effective DPI (96 = 1.0).
pub fn scale_from_dpi(dpi: u32) -> f32 {
    (dpi.max(48) as f32 / 96.0).clamp(0.5, 8.0)
}

/// Top-left of a `w x h` window (device px) inside the work area `(l, t, r, b)`: 4 px margin to the
/// bottom, and to the docked side for corner placements; centred for `BottomCenter`.
pub fn place_window(work: (i32, i32, i32, i32), win: (i32, i32), pos: PanelPosition, scale: f32) -> (i32, i32) {
    let margin = (4.0 * scale).round() as i32;
    let (l, t, r, b) = work;
    let _ = t;
    let x = match pos {
        PanelPosition::BottomRight => r - margin - win.0,
        PanelPosition::BottomLeft => l + margin,
        PanelPosition::BottomCenter => l + (r - l - win.0) / 2,
    };
    (x, b - margin - win.1)
}

// --- animation timer -------------------------------------------------------------------------

struct SendHandle(isize);
// SAFETY: Win32 kernel handles may be used from any thread.
unsafe impl Send for SendHandle {}

/// A thread that owns a high-resolution waitable timer and posts `WM_APP_TICK` at ~62.5 Hz while
/// started; while stopped it blocks on an event (zero CPU).
pub struct AnimTimer {
    start_evt: HANDLE,
    stop_evt: HANDLE,
    quit_evt: HANDLE,
    active: AtomicBool,
    pending: Arc<AtomicBool>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl AnimTimer {
    pub fn new(hwnd: HWND) -> Result<AnimTimer, String> {
        // SAFETY: plain event creation.
        let (start_evt, stop_evt, quit_evt) = unsafe {
            (
                CreateEventW(None, false, false, None).map_err(|e| e.to_string())?,
                CreateEventW(None, false, false, None).map_err(|e| e.to_string())?,
                CreateEventW(None, false, false, None).map_err(|e| e.to_string())?,
            )
        };
        let pending = Arc::new(AtomicBool::new(false));
        let (h, s, st, q) = (SendHandle(hwnd.0 as isize), SendHandle(start_evt.0 as isize), SendHandle(stop_evt.0 as isize), SendHandle(quit_evt.0 as isize));
        let p = pending.clone();
        let join = std::thread::Builder::new()
            .name("pill-anim".into())
            .spawn(move || anim_thread(h, s, st, q, p))
            .map_err(|e| e.to_string())?;
        Ok(AnimTimer { start_evt, stop_evt, quit_evt, active: AtomicBool::new(false), pending, join: Some(join) })
    }

    pub fn start(&self) {
        if !self.active.swap(true, Ordering::AcqRel) {
            // SAFETY: valid event handle owned by self.
            unsafe {
                let _ = SetEvent(self.start_evt);
            }
        }
    }

    pub fn stop(&self) {
        if self.active.swap(false, Ordering::AcqRel) {
            // SAFETY: valid event handle owned by self.
            unsafe {
                let _ = SetEvent(self.stop_evt);
            }
        }
    }

    pub fn is_active(&self) -> bool {
        self.active.load(Ordering::Acquire)
    }

    /// Called by the window procedure when it consumed a tick.
    pub fn ack(&self) {
        self.pending.store(false, Ordering::Release);
    }
}

impl Drop for AnimTimer {
    fn drop(&mut self) {
        // SAFETY: handles are valid until closed below, after the thread exited.
        unsafe {
            let _ = SetEvent(self.quit_evt);
        }
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
        unsafe {
            let _ = CloseHandle(self.start_evt);
            let _ = CloseHandle(self.stop_evt);
            let _ = CloseHandle(self.quit_evt);
        }
    }
}

fn anim_thread(hwnd: SendHandle, start: SendHandle, stop: SendHandle, quit: SendHandle, pending: Arc<AtomicBool>) {
    let hwnd = HWND(hwnd.0 as *mut core::ffi::c_void);
    let (start, stop, quit) = (HANDLE(start.0 as *mut core::ffi::c_void), HANDLE(stop.0 as *mut core::ffi::c_void), HANDLE(quit.0 as *mut core::ffi::c_void));
    // SAFETY: handles stay valid until `AnimTimer::drop` joins this thread.
    unsafe {
        let timer = CreateWaitableTimerExW(None, PCWSTR::null(), CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, TIMER_ALL_ACCESS.0)
            .or_else(|_| CreateWaitableTimerExW(None, PCWSTR::null(), Default::default(), TIMER_ALL_ACCESS.0));
        let Ok(timer) = timer else { return };
        loop {
            let r = WaitForMultipleObjects(&[start, quit], false, INFINITE);
            if r.0 != WAIT_OBJECT_0.0 {
                break;
            }
            let due: i64 = -160_000; // 16 ms, relative
            if SetWaitableTimer(timer, &due, 16, None, None, false).is_err() {
                continue;
            }
            loop {
                let r = WaitForMultipleObjects(&[timer, stop, quit], false, INFINITE);
                match r.0.wrapping_sub(WAIT_OBJECT_0.0) {
                    0 => {
                        if !pending.swap(true, Ordering::AcqRel) {
                            let _ = PostMessageW(Some(hwnd), WM_APP_TICK, WPARAM(0), LPARAM(0));
                        }
                    }
                    1 => {
                        let _ = CancelWaitableTimer(timer);
                        break;
                    }
                    _ => {
                        let _ = CancelWaitableTimer(timer);
                        let _ = CloseHandle(timer);
                        return;
                    }
                }
            }
        }
        let _ = CloseHandle(timer);
    }
}

// --- DIB ---------------------------------------------------------------------------------------

struct Dib {
    dc: HDC,
    bmp: HBITMAP,
    old: HGDIOBJ,
    bits: *mut u8,
    w: i32,
    h: i32,
}

impl Dib {
    fn new(w: i32, h: i32) -> Option<Dib> {
        // SAFETY: standard memory-DC + DIB section creation, released in Drop.
        unsafe {
            let dc = CreateCompatibleDC(None);
            if dc.is_invalid() {
                return None;
            }
            let bmi = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: w,
                    biHeight: -h,
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
            let bmp = CreateDIBSection(Some(dc), &bmi, DIB_RGB_COLORS, &mut bits, None, 0).ok()?;
            if bits.is_null() {
                let _ = DeleteDC(dc);
                return None;
            }
            let old = SelectObject(dc, HGDIOBJ(bmp.0));
            Some(Dib { dc, bmp, old, bits: bits as *mut u8, w, h })
        }
    }

    /// Copy a premultiplied RGBA frame into the BGRA bitmap.
    fn upload(&mut self, rgba: &[u8]) {
        let n = (self.w * self.h * 4) as usize;
        if rgba.len() < n {
            return;
        }
        // SAFETY: `bits` points to w*h*4 bytes owned by the DIB section.
        let dst = unsafe { std::slice::from_raw_parts_mut(self.bits, n) };
        for (d, s) in dst.chunks_exact_mut(4).zip(rgba.chunks_exact(4)) {
            d[0] = s[2];
            d[1] = s[1];
            d[2] = s[0];
            d[3] = s[3];
        }
    }
}

impl Drop for Dib {
    fn drop(&mut self) {
        // SAFETY: handles created in `new`.
        unsafe {
            SelectObject(self.dc, self.old);
            let _ = DeleteObject(HGDIOBJ(self.bmp.0));
            let _ = DeleteDC(self.dc);
        }
    }
}

// --- window state ------------------------------------------------------------------------------

struct Down {
    hit: Hit,
    screen: POINT,
    win_pos: (i32, i32),
    dragging: bool,
}

pub(super) struct Inner {
    pub hwnd: HWND,
    pub st: RenderState,
    pub opts: PillOptions,
    pub level: Arc<AtomicU32>,
    pub on_event: Option<Box<dyn FnMut(PillEvent)>>,
    pm: Pixmap,
    dib: Option<Dib>,
    timer: Option<AnimTimer>,
    monitor: HMONITOR,
    work: (i32, i32, i32, i32),
    pos: (i32, i32),
    custom_pos: Option<(i32, i32)>,
    visible: bool,
    tracking: bool,
    down: Option<Down>,
    last_hit: Hit,
    /// Frames pushed to the screen (tests and diagnostics).
    pub frames: u64,
}

/// What the window procedure asks the outer layer to do after the borrow is released.
#[derive(Default)]
struct Outcome {
    result: Option<LRESULT>,
    events: Vec<PillEvent>,
    menu: bool,
}

/// Screen position of a mouse message (client coordinates + window origin; the cursor itself is never read
/// so synthetic messages behave like real ones).
fn screen_of(lp: LPARAM, win: (i32, i32)) -> POINT {
    POINT { x: win.0 + (lp.0 & 0xffff) as i16 as i32, y: win.1 + ((lp.0 >> 16) & 0xffff) as i16 as i32 }
}

fn monitor_info(hmon: HMONITOR) -> Option<(i32, i32, i32, i32)> {
    let mut mi = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
    // SAFETY: valid out-pointer with cbSize set.
    if unsafe { GetMonitorInfoW(hmon, &mut mi) }.as_bool() {
        let r = mi.rcWork;
        Some((r.left, r.top, r.right, r.bottom))
    } else {
        None
    }
}

fn monitor_scale(hmon: HMONITOR) -> f32 {
    let (mut x, mut y) = (96u32, 96u32);
    // SAFETY: valid out-pointers.
    unsafe {
        let _ = GetDpiForMonitor(hmon, MDT_EFFECTIVE_DPI, &mut x, &mut y);
    }
    scale_from_dpi(x)
}

fn cursor_monitor() -> HMONITOR {
    let mut p = POINT::default();
    // SAFETY: valid out-pointer.
    unsafe {
        let _ = GetCursorPos(&mut p);
        MonitorFromPoint(p, MONITOR_DEFAULTTONEAREST)
    }
}

impl Inner {
    fn now(&self) -> f64 {
        now_ms()
    }

    fn client_size(&self) -> (i32, i32) {
        let (w, h) = pixel_size(&self.st);
        (w as i32, h as i32)
    }

    fn ensure_buffers(&mut self) {
        let (w, h) = self.client_size();
        if self.pm.width() as i32 != w || self.pm.height() as i32 != h {
            if let Some(pm) = Pixmap::new(w as u32, h as u32) {
                self.pm = pm;
            }
        }
        if self.dib.as_ref().map(|d| d.w != w || d.h != h).unwrap_or(true) {
            self.dib = Dib::new(w, h);
        }
    }

    fn default_pos(&self) -> (i32, i32) {
        place_window(self.work, self.client_size(), self.opts.position, self.st.scale)
    }

    fn refresh_monitor(&mut self, hmon: HMONITOR) {
        self.monitor = hmon;
        if let Some(w) = monitor_info(hmon) {
            self.work = w;
        }
        let s = monitor_scale(hmon);
        self.st.set_scale(s);
    }

    /// Re-layout (window size may have changed) and push a frame.
    fn relayout(&mut self) {
        self.ensure_buffers();
        self.pos = match (self.custom_pos, self.st.error().is_some()) {
            (Some(p), false) => p,
            _ => self.default_pos(),
        };
        self.present();
    }

    fn present(&mut self) {
        let t = self.now();
        self.ensure_buffers();
        render_into(&self.st, t, &mut self.pm);
        let visible = self.st.wants_visible(t, self.opts.auto_hide) || self.st.is_animating(t);
        let Some(dib) = self.dib.as_mut() else { return };
        dib.upload(self.pm.data());
        if !visible {
            if self.visible {
                // SAFETY: valid window handle.
                unsafe {
                    let _ = ShowWindow(self.hwnd, SW_HIDE);
                }
                self.visible = false;
            }
            return;
        }
        let pos = POINT { x: self.pos.0, y: self.pos.1 };
        let size = SIZE { cx: dib.w, cy: dib.h };
        let src = POINT { x: 0, y: 0 };
        let blend = BLENDFUNCTION { BlendOp: AC_SRC_OVER as u8, BlendFlags: 0, SourceConstantAlpha: 255, AlphaFormat: AC_SRC_ALPHA as u8 };
        // SAFETY: all pointers reference live locals; the DIB DC stays selected.
        unsafe {
            if !self.visible {
                let _ = SetWindowPos(self.hwnd, Some(HWND_TOPMOST), pos.x, pos.y, size.cx, size.cy, SWP_NOACTIVATE);
                let _ = ShowWindow(self.hwnd, SW_SHOWNOACTIVATE);
                self.visible = true;
            }
            let _ = UpdateLayeredWindow(self.hwnd, None, Some(&pos), Some(&size), Some(dib.dc), Some(&src), COLORREF(0), Some(&blend), ULW_ALPHA);
        }
        self.frames += 1;
    }

    fn sync_timer(&mut self) {
        let t = self.now();
        if let Some(timer) = &self.timer {
            if self.st.is_animating(t) {
                timer.start();
            }
        }
    }

    /// State changed from outside the message loop: draw once and (re)arm the timer.
    pub fn refresh(&mut self) {
        self.relayout_if_resized();
        self.present();
        self.sync_timer();
    }

    fn relayout_if_resized(&mut self) {
        let (w, h) = self.client_size();
        let changed = self.dib.as_ref().map(|d| d.w != w || d.h != h).unwrap_or(true);
        if changed {
            self.relayout();
        }
    }

    fn set_pos(&mut self, p: (i32, i32)) {
        self.pos = p;
        // SAFETY: valid window handle.
        unsafe {
            let _ = SetWindowPos(self.hwnd, None, p.0, p.1, 0, 0, SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE);
        }
    }

    fn client_point(&self, lparam: LPARAM) -> (f32, f32) {
        let x = (lparam.0 & 0xffff) as i16 as f32;
        let y = ((lparam.0 >> 16) & 0xffff) as i16 as f32;
        (x / self.st.scale, y / self.st.scale)
    }

    fn hit_at_client(&self, lx: f32, ly: f32) -> Hit {
        self.st.hit_test(lx, ly, self.now())
    }

    fn update_pointer(&mut self, hit: Hit) {
        let now = self.now();
        self.last_hit = hit;
        let ptr = self.st.pointer_for(hit);
        if matches!(hit, Hit::ErrorCard | Hit::ErrorRetry) || self.st.error().is_some() {
            let h = match hit {
                Hit::ErrorRetry => CardHover::Retry,
                Hit::ErrorCard => CardHover::Card,
                _ => CardHover::None,
            };
            let before = self.st.error().map(|e| e.hover);
            if let Some(e) = self.st.error_mut() {
                e.set_hover(h, now);
            }
            if before != self.st.error().map(|e| e.hover) {
                self.present();
                self.sync_timer();
            }
            return;
        }
        if ptr != self.st.pointer() {
            self.st.set_pointer(ptr, now);
            self.present();
            self.sync_timer();
        }
    }

    fn can_click_pill(&self) -> bool {
        !matches!(self.st.mode(), PillMode::Thinking)
    }

    fn handle(&mut self, hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> Outcome {
        let mut out = Outcome::default();
        match msg {
            WM_NCHITTEST => {
                let mut p = POINT { x: (lp.0 & 0xffff) as i16 as i32, y: ((lp.0 >> 16) & 0xffff) as i16 as i32 };
                // SAFETY: valid window handle and out-pointer.
                unsafe {
                    let _ = ScreenToClient(hwnd, &mut p);
                }
                let hit = self.hit_at_client(p.x as f32 / self.st.scale, p.y as f32 / self.st.scale);
                out.result = Some(LRESULT(if hit == Hit::None { HTTRANSPARENT as isize } else { HTCLIENT as isize }));
            }
            WM_MOUSEACTIVATE => out.result = Some(LRESULT(MA_NOACTIVATE as isize)),
            WM_SETCURSOR => {
                let id = match self.last_hit {
                    Hit::Pill if self.down.as_ref().is_some_and(|d| d.dragging) => IDC_SIZEALL,
                    Hit::Pill if !self.can_click_pill() => IDC_NO,
                    Hit::Pill | Hit::Cancel | Hit::ErrorRetry => IDC_HAND,
                    _ => IDC_ARROW,
                };
                // SAFETY: stock cursor.
                unsafe {
                    if let Ok(c) = LoadCursorW(None, id) {
                        SetCursor(Some(c));
                    }
                }
                out.result = Some(LRESULT(1));
            }
            WM_MOUSEMOVE => {
                if !self.tracking {
                    let mut tme = TRACKMOUSEEVENT { cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32, dwFlags: TME_LEAVE, hwndTrack: hwnd, dwHoverTime: 0 };
                    // SAFETY: valid struct.
                    if unsafe { TrackMouseEvent(&mut tme) }.is_ok() {
                        self.tracking = true;
                    }
                }
                let (lx, ly) = self.client_point(lp);
                let pos_now = self.pos;
                if let Some(d) = self.down.as_mut() {
                    let cur = screen_of(lp, pos_now);
                    let dx = (cur.x - d.screen.x) as f32;
                    let dy = (cur.y - d.screen.y) as f32;
                    if !d.dragging && dx.hypot(dy) > DRAG_THRESHOLD_PX * self.st.scale && matches!(d.hit, Hit::Pill | Hit::Bubble | Hit::Gap) {
                        d.dragging = true;
                        self.st.set_dragging(true);
                    }
                    if d.dragging {
                        let np = (d.win_pos.0 + dx as i32, d.win_pos.1 + dy as i32);
                        self.custom_pos = Some(np);
                        self.set_pos(np);
                    }
                    out.result = Some(LRESULT(0));
                    return out;
                }
                let hit = self.hit_at_client(lx, ly);
                self.update_pointer(hit);
                out.result = Some(LRESULT(0));
            }
            WM_MOUSELEAVE => {
                self.tracking = false;
                if self.down.is_none() {
                    self.update_pointer(Hit::None);
                }
                out.result = Some(LRESULT(0));
            }
            WM_LBUTTONDOWN => {
                let (lx, ly) = self.client_point(lp);
                let hit = self.hit_at_client(lx, ly);
                let cur = screen_of(lp, self.pos);
                // SAFETY: capture on our own window.
                unsafe {
                    SetCapture(hwnd);
                }
                self.down = Some(Down { hit, screen: cur, win_pos: self.pos, dragging: false });
                out.result = Some(LRESULT(0));
            }
            WM_LBUTTONUP => {
                // SAFETY: releasing our own capture.
                unsafe {
                    let _ = ReleaseCapture();
                }
                let (lx, ly) = self.client_point(lp);
                let up = self.hit_at_client(lx, ly);
                if let Some(d) = self.down.take() {
                    self.st.set_dragging(false);
                    if !d.dragging && d.hit == up {
                        match up {
                            Hit::Pill if self.can_click_pill() => out.events.push(PillEvent::Clicked),
                            Hit::Cancel if self.st.cancel_interactive(self.now()) => out.events.push(PillEvent::CancelClicked),
                            Hit::ErrorRetry => {
                                let now = self.now();
                                if let Some(e) = self.st.error_mut() {
                                    e.dismiss_now(now);
                                }
                                out.events.push(PillEvent::ErrorRetry);
                                self.present();
                                self.sync_timer();
                            }
                            _ => {}
                        }
                    }
                    // pointer may have left while captured
                    self.update_pointer(up);
                }
                out.result = Some(LRESULT(0));
            }
            WM_RBUTTONUP => {
                let (lx, ly) = self.client_point(lp);
                if self.hit_at_client(lx, ly) == Hit::Pill && self.st.error().is_none() {
                    out.menu = true;
                }
                out.result = Some(LRESULT(0));
            }
            WM_APP_TICK => {
                if let Some(t) = &self.timer {
                    t.ack();
                }
                let now = self.now();
                let rms = f32::from_bits(self.level.load(Ordering::Relaxed));
                let o: TickOutcome = self.st.tick(now, rms);
                if o.error_expired {
                    out.events.push(PillEvent::ErrorDismissed);
                }
                if o.error_gone {
                    self.relayout();
                } else {
                    self.present();
                }
                if !self.st.is_animating(now) {
                    if let Some(t) = &self.timer {
                        t.stop();
                    }
                    // final frame at rest (also hides the window when idle + auto-hide)
                    self.present();
                }
                out.result = Some(LRESULT(0));
            }
            WM_SETTINGCHANGE => {
                if self.opts.theme == Theme::Auto {
                    let dark = resolve_dark(Theme::Auto);
                    if dark != self.st.pal.dark {
                        let now = self.now();
                        self.st.set_palette(Palette::get(dark), now);
                        self.present();
                    }
                }
                if wp.0 as u32 == SPI_SETWORKAREA.0 {
                    self.refresh_monitor(self.monitor);
                    self.relayout();
                }
            }
            WM_DISPLAYCHANGE => {
                self.refresh_monitor(cursor_monitor());
                self.custom_pos = None;
                self.relayout();
            }
            WM_DESTROY => {}
            _ => {}
        }
        out
    }
}

// --- public handle ------------------------------------------------------------------------------

pub struct Window {
    inner: Rc<RefCell<Inner>>,
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    // SAFETY: GWLP_USERDATA holds a pointer produced by `Rc::into_raw` in `Window::create`; the
    // matching strong count is released in `Window::drop` after the window is destroyed.
    let ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *const RefCell<Inner>;
    if ptr.is_null() {
        return unsafe { DefWindowProcW(hwnd, msg, wp, lp) };
    }
    let rc = std::mem::ManuallyDrop::new(unsafe { Rc::from_raw(ptr) });
    let outcome = match rc.try_borrow_mut() {
        Ok(mut inner) => inner.handle(hwnd, msg, wp, lp),
        Err(_) => Outcome::default(),
    };
    let mut events = outcome.events;
    if outcome.menu {
        if let Some(ev) = run_menu(&rc, hwnd) {
            events.push(ev);
        }
    }
    for ev in events {
        let cb = rc.try_borrow_mut().ok().and_then(|mut i| i.on_event.take());
        if let Some(mut cb) = cb {
            cb(ev);
            if let Ok(mut i) = rc.try_borrow_mut() {
                if i.on_event.is_none() {
                    i.on_event = Some(cb);
                }
            }
        }
    }
    match outcome.result {
        Some(r) => r,
        None => unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
}

/// Native popup menu: Start/Stop listening, Hide for now, Open OpenTalk.
fn run_menu(rc: &Rc<RefCell<Inner>>, hwnd: HWND) -> Option<PillEvent> {
    let (recording, busy) = {
        let i = rc.try_borrow().ok()?;
        (matches!(i.st.mode(), PillMode::Recording | PillMode::Unavailable), matches!(i.st.mode(), PillMode::Thinking))
    };
    // SAFETY: menu handle is created, used and destroyed here; strings are NUL-terminated wide literals.
    unsafe {
        let menu: HMENU = CreatePopupMenu().ok()?;
        let toggle = if recording { w!("Stop listening") } else { w!("Start listening") };
        let _ = AppendMenuW(menu, if busy { MF_STRING | MF_GRAYED } else { MF_STRING }, 1, toggle);
        let _ = AppendMenuW(menu, MF_STRING, 2, w!("Hide for now"));
        let _ = AppendMenuW(menu, MF_STRING, 3, w!("Open OpenTalk"));
        let mut p = POINT::default();
        let _ = GetCursorPos(&mut p);
        let _ = SetForegroundWindow(hwnd);
        let cmd = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_BOTTOMALIGN, p.x, p.y, Some(0), hwnd, None);
        let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
        let _ = DestroyMenu(menu);
        // the cursor may now be anywhere
        if let Ok(mut i) = rc.try_borrow_mut() {
            i.tracking = false;
            i.update_pointer(Hit::None);
        }
        match cmd.0 {
            1 => Some(PillEvent::MenuToggleListening),
            2 => Some(PillEvent::MenuHideForNow),
            3 => Some(PillEvent::MenuOpenSettings),
            _ => None,
        }
    }
}

impl Window {
    pub fn create(level: Arc<AtomicU32>, opts: PillOptions, on_event: Box<dyn FnMut(PillEvent)>) -> Result<Window, String> {
        // SAFETY: registration + creation of our own window class/window on this thread.
        unsafe {
            let _ = SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
            let hmod = GetModuleHandleW(None).map_err(|e| e.to_string())?;
            let hinst = HINSTANCE(hmod.0);
            let wc = WNDCLASSEXW {
                cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
                lpfnWndProc: Some(wndproc),
                hInstance: hinst,
                lpszClassName: CLASS_NAME,
                ..Default::default()
            };
            // a second Pill in the same process re-registers; that failure is harmless
            RegisterClassExW(&wc);
            let hwnd = CreateWindowExW(
                WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
                CLASS_NAME,
                w!("OpenTalk Pill"),
                WS_POPUP,
                0,
                0,
                1,
                1,
                None,
                None,
                Some(hinst),
                None,
            )
            .map_err(|e| e.to_string())?;
            let hmon = {
                let mut p = POINT::default();
                let _ = GetCursorPos(&mut p);
                MonitorFromPoint(p, MONITOR_DEFAULTTOPRIMARY)
            };
            let scale = monitor_scale(hmon);
            let dark = resolve_dark(opts.theme);
            let mut st = RenderState::new(Palette::get(dark), scale, opts.position);
            st.hotkey_label = opts.hotkey_label.clone();
            st.show_app_bubble = opts.show_app_bubble;
            let work = monitor_info(hmon).unwrap_or((0, 0, 1920, 1080));
            let (w, h) = pixel_size(&st);
            let inner = Inner {
                hwnd,
                pm: Pixmap::new(w, h).ok_or("pixmap")?,
                st,
                opts,
                level,
                on_event: Some(on_event),
                dib: None,
                timer: AnimTimer::new(hwnd).ok(),
                monitor: hmon,
                work,
                pos: (0, 0),
                custom_pos: None,
                visible: false,
                tracking: false,
                down: None,
                last_hit: Hit::None,
                frames: 0,
            };
            let rc = Rc::new(RefCell::new(inner));
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, Rc::into_raw(rc.clone()) as isize);
            // warm the glow sprites now so the first thinking state costs nothing
            let _ = super::glow::Sprites::get(scale);
            {
                let mut i = rc.borrow_mut();
                i.relayout();
            }
            Ok(Window { inner: rc })
        }
    }

    pub fn with<R>(&self, f: impl FnOnce(&mut Inner) -> R) -> R {
        let mut i = self.inner.borrow_mut();
        f(&mut i)
    }

    pub fn hwnd(&self) -> isize {
        self.inner.borrow().hwnd.0 as isize
    }
}

impl Inner {
    pub fn set_mode(&mut self, mode: PillMode) {
        let now = self.now();
        self.st.set_mode(mode, now);
        self.refresh();
    }

    pub fn set_app(&mut self, app: Option<super::model::AppBubble>) {
        let now = self.now();
        self.st.set_app(app, now);
        self.refresh();
    }

    pub fn show_error(&mut self, title: &str, desc: &str, retry: bool) {
        let now = self.now();
        self.st.error = Some(super::card::ErrorCard::new(title, desc, retry, now));
        self.st.set_pointer(Pointer::default(), now);
        self.custom_pos = None;
        self.relayout();
        self.sync_timer();
    }

    pub fn set_options(&mut self, opts: PillOptions) {
        let now = self.now();
        let dark = resolve_dark(opts.theme);
        self.st.set_position(opts.position);
        self.st.hotkey_label = opts.hotkey_label.clone();
        self.st.show_app_bubble = opts.show_app_bubble;
        if dark != self.st.pal.dark {
            self.st.set_palette(Palette::get(dark), now);
        }
        self.opts = opts;
        // re-evaluate bubble wanted/hidden state
        let app = self.st.app().cloned();
        self.st.set_app(app, now);
        self.custom_pos = None;
        self.relayout();
        self.sync_timer();
    }

    pub fn follow_cursor_display(&mut self) {
        let hmon = cursor_monitor();
        if hmon != self.monitor {
            self.refresh_monitor(hmon);
            self.custom_pos = None;
        } else if let Some(w) = monitor_info(hmon) {
            self.work = w;
        }
        self.relayout();
    }

    pub fn mode(&self) -> PillMode {
        self.st.mode()
    }
}

impl Drop for Window {
    fn drop(&mut self) {
        let hwnd = self.inner.borrow().hwnd;
        // SAFETY: destroys our own window, then releases the strong count handed to GWLP_USERDATA.
        unsafe {
            let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const RefCell<Inner>;
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            let _ = DestroyWindow(hwnd);
            if !ptr.is_null() {
                drop(Rc::from_raw(ptr));
            }
        }
    }
}

#[allow(dead_code)]
fn _unused(_: RECT, _: &mut Option<u8>, _: fn(&RenderState) -> (f32, f32)) {
    let _ = window_size;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dpi_scale_mapping() {
        assert_eq!(scale_from_dpi(96), 1.0);
        assert_eq!(scale_from_dpi(144), 1.5);
        assert_eq!(scale_from_dpi(192), 2.0);
        assert_eq!(scale_from_dpi(120), 1.25);
    }

    #[test]
    fn placement_per_panel_position() {
        let work = (0, 0, 1920, 1040);
        let win = (208, 120);
        assert_eq!(place_window(work, win, PanelPosition::BottomRight, 1.0), (1920 - 4 - 208, 1040 - 4 - 120));
        assert_eq!(place_window(work, win, PanelPosition::BottomLeft, 1.0), (4, 1040 - 4 - 120));
        assert_eq!(place_window(work, win, PanelPosition::BottomCenter, 1.0), ((1920 - 208) / 2, 1040 - 4 - 120));
        // secondary monitor with an offset origin and 150 % DPI: margin is 6 device px
        let work = (-2560, 0, 0, 1400);
        let win = (312, 180);
        assert_eq!(place_window(work, win, PanelPosition::BottomRight, 1.5), (0 - 6 - 312, 1400 - 6 - 180));
        assert_eq!(place_window(work, win, PanelPosition::BottomLeft, 1.5), (-2560 + 6, 1400 - 6 - 180));
    }

    #[test]
    fn pill_sits_16px_from_work_area_edge() {
        // window margin 4 + group inset 12 = 16 logical px from the work-area corner
        let st = RenderState::new(Palette::LIGHT, 1.0, PanelPosition::BottomRight);
        let (w, h) = pixel_size(&st);
        let (wx, wy) = place_window((0, 0, 1920, 1040), (w as i32, h as i32), PanelPosition::BottomRight, 1.0);
        let g = st.geometry(0.0);
        assert_eq!(1920.0 - (wx as f32 + g.pill.r), 16.0);
        assert_eq!(1040.0 - (wy as f32 + g.pill.b), 16.0);
    }
}

#[cfg(test)]
#[path = "desktop_tests.rs"]
mod desktop;
