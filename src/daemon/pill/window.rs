//! The Win32 layer: one layered, top-most, no-activate tool window that shows the pre-rendered
//! premultiplied BGRA frame via `UpdateLayeredWindow`; alpha-based hit-testing, hover tracking,
//! click-vs-drag, the right-click menu, per-monitor DPI and the on-demand 60 fps animation timer.

use super::anim::{QuietRun, SLOW_TICK_MS, TICK_MS, level_band};
use super::capture::{GlassHost, Refresh, ScreenEvents, WM_APP_GLASS, WM_APP_SCREEN_EVENT};
use super::card::CardHover;
use super::model::{Hit, Pointer, RenderState, TickOutcome};
use super::pacer::{Hint, Vsync};
use super::raster::DevRect;
use super::render::{BAR_EPS, FrameCache, FrameOutcome, pixel_size};
use super::theme::{Palette, resolve_dark};
use super::{PillEvent, PillMode, PillOptions};
use crate::core::config::{PanelPosition, Theme};
use crate::core::timing::now_ms;
use crate::daemon::audio::{arm_level_wake, disarm_level_wake};
use std::sync::atomic::AtomicU64;
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use tiny_skia::Pixmap;
use windows::Win32::Foundation::{COLORREF, HANDLE, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WAIT_OBJECT_0, WAIT_TIMEOUT, WPARAM, CloseHandle};
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
    SetWindowLongPtrW, SetWindowPos, ShowWindow, TPM_BOTTOMALIGN, TPM_RETURNCMD, TPM_RIGHTBUTTON, TrackPopupMenu, ULW_ALPHA, UPDATELAYEREDWINDOWINFO,
    UpdateLayeredWindow, UpdateLayeredWindowIndirect, KillTimer, SetTimer, WM_TIMER,
    WM_APP, WM_DESTROY, WM_DISPLAYCHANGE, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEACTIVATE, WM_MOUSEMOVE, WM_NCHITTEST, WM_NULL,
    WM_RBUTTONUP, WM_SETCURSOR, WM_SETTINGCHANGE, WNDCLASSEXW, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
};
use windows::core::{PCWSTR, w};

pub const WM_APP_TICK: u32 = WM_APP + 0x50;
const WM_MOUSELEAVE: u32 = 0x02A3;
const WM_CAPTURECHANGED: u32 = 0x0215;
const WM_DPICHANGED: u32 = 0x02E0;
const CLASS_NAME: PCWSTR = w!("OpenTalkPill");
const DRAG_THRESHOLD_PX: f32 = 5.0;
/// Timer id of the periodic backdrop refresh (a USER timer on the pill window). It runs only while
/// something is going on (hover, drag, recording, thinking, an animation), never while the pill rests.
const GLASS_TIMER_ID: usize = 0x6c67;
/// Timer id of the one-shot deferred capture: armed by an event (another window activated, moved or
/// minimised; a rate-limited request), it fires once and is killed.
const GLASS_SETTLE_TIMER_ID: usize = 0x6c68;
/// Delay between a screen event and the capture it causes, so the window that changed has finished
/// its open / minimise animation. Also coalesces bursts of events into one capture.
const GLASS_SETTLE_MS: u32 = 400;
/// Backdrop refresh period while the pointer is on the pill or it is being dragged: 8 Hz (a
/// dragged window must keep its picture under it).
const GLASS_FAST_MS: u32 = 125;
/// Backdrop refresh period while recording, thinking or animating: 4 Hz. The glass
/// frosts ~10 px of the screen, so it changes slowly; every capture costs ~1 ms of CPU at 2x (PLAN
/// section 1 caps recording at 1.5 %), and a capture of an unchanged screen repaints nothing.
const GLASS_ACTIVE_MS: u32 = 250;
/// Two captures are never closer than this, whatever asks for one.
const GLASS_MIN_GAP_MS: f64 = 100.0;

// --- pure helpers (unit-tested) ------------------------------------------------------------

/// Period of the backdrop timer: the fast rate for hover and drag, the active rate while something is
/// going on, and 0 (no timer) otherwise: while the window is hidden or cannot capture, and while the
/// pill rests (PLAN section 1: an idle daemon runs no timers). The resting circle's backdrop is
/// refreshed on events instead (show, state / size / scale / dock / position change, hover, and the
/// [`ScreenEvents`] hooks), so its frost can be slightly stale.
fn glass_period_ms(shown_and_capturing: bool, fast: bool, busy: bool) -> u32 {
    if !shown_and_capturing {
        0
    } else if fast {
        GLASS_FAST_MS
    } else if busy {
        GLASS_ACTIVE_MS
    } else {
        0
    }
}

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

fn handle(raw: isize) -> HANDLE {
    HANDLE(raw as *mut core::ffi::c_void)
}

/// State shared by the window, the timer thread and the capture-side wake callback. The event
/// handles are closed when the last reference goes, so a late wake can never signal a closed handle.
struct TimerCore {
    start_evt: isize,
    stop_evt: isize,
    quit_evt: isize,
    /// Signalled whenever `slow` flips; the thread re-arms its timer to match.
    rate_evt: isize,
    active: AtomicBool,
    /// The recording is visually static: tick at `SLOW_TICK_MS` instead of `TICK_MS`.
    slow: AtomicBool,
    /// Something asked for full rate since the window last looked (it then restarts its quiet count).
    woken: AtomicBool,
    /// Token of the capture-side level wake armed for the current slow spell (0: none).
    wake_token: AtomicU64,
    /// When the frame of the tick being handled is shown (set by the vsync-paced thread).
    hint: Hint,
}

impl TimerCore {
    fn signal(evt: isize) {
        // SAFETY: `evt` is an event handle owned by this core and still open (closed only in `drop`).
        unsafe {
            let _ = SetEvent(handle(evt));
        }
    }

    /// Back to the full-rate cadence; the timer thread posts a tick right away. Idempotent and cheap
    /// (two atomic operations) when already at full rate.
    fn wake(&self) {
        self.woken.store(true, Ordering::Release);
        if self.slow.swap(false, Ordering::AcqRel) {
            let token = self.wake_token.swap(0, Ordering::AcqRel);
            if token != 0 {
                disarm_level_wake(token);
            }
            Self::signal(self.rate_evt);
        }
    }
}

impl Drop for TimerCore {
    fn drop(&mut self) {
        // SAFETY: the last owner closes the handles; the timer thread (an owner) has exited.
        unsafe {
            for h in [self.start_evt, self.stop_evt, self.quit_evt, self.rate_evt] {
                let _ = CloseHandle(handle(h));
            }
        }
    }
}

/// A thread that owns a high-resolution waitable timer and calls its sink at ~62.5 Hz while started
/// (window: it posts `WM_APP_TICK`); while stopped it blocks on an event (zero CPU). A static
/// recording asks for the slow cadence (~15 Hz) and is brought back to full rate within one frame by
/// [`AnimTimer::wake`] - from the capture thread when the level changes, or from the window.
pub struct AnimTimer {
    core: Arc<TimerCore>,
    /// Set while a tick message is queued; the window clears it when it consumes one.
    pending: Arc<AtomicBool>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl AnimTimer {
    /// Timer that posts `WM_APP_TICK` to `hwnd` (coalescing ticks the window has not consumed yet).
    pub fn new(hwnd: HWND) -> Result<AnimTimer, String> {
        let pending = Arc::new(AtomicBool::new(false));
        let (h, p) = (SendHandle(hwnd.0 as isize), pending.clone());
        let sink = move || {
            if !p.swap(true, Ordering::AcqRel) {
                let hwnd = HWND(h.0 as *mut core::ffi::c_void);
                // SAFETY: posting to a window handle is safe from any thread; a stale handle just fails.
                unsafe {
                    let _ = PostMessageW(Some(hwnd), WM_APP_TICK, WPARAM(0), LPARAM(0));
                }
            }
        };
        Self::with_sink(Box::new(sink), pending)
    }

    /// Timer that calls `sink` on every tick (from the timer thread). `pending` is only what
    /// [`AnimTimer::ack`] clears.
    fn with_sink(sink: Box<dyn Fn() + Send>, pending: Arc<AtomicBool>) -> Result<AnimTimer, String> {
        let event = || -> Result<isize, String> {
            // SAFETY: plain auto-reset event creation.
            unsafe { CreateEventW(None, false, false, None) }.map(|h| h.0 as isize).map_err(|e| e.to_string())
        };
        let core = Arc::new(TimerCore {
            start_evt: event()?,
            stop_evt: event()?,
            quit_evt: event()?,
            rate_evt: event()?,
            active: AtomicBool::new(false),
            slow: AtomicBool::new(false),
            woken: AtomicBool::new(false),
            wake_token: AtomicU64::new(0),
            hint: Hint::new(),
        });
        let c = core.clone();
        let join = std::thread::Builder::new().name("pill-anim".into()).spawn(move || anim_thread(c, sink)).map_err(|e| e.to_string())?;
        Ok(AnimTimer { core, pending, join: Some(join) })
    }

    /// Run at full rate (also wakes a slowed timer).
    pub fn start(&self) {
        self.core.wake();
        if !self.core.active.swap(true, Ordering::AcqRel) {
            TimerCore::signal(self.core.start_evt);
        }
    }

    pub fn stop(&self) {
        self.core.wake();
        if self.core.active.swap(false, Ordering::AcqRel) {
            TimerCore::signal(self.core.stop_evt);
        }
    }

    #[cfg(test)]
    pub fn is_active(&self) -> bool {
        self.core.active.load(Ordering::Acquire)
    }

    pub fn is_slow(&self) -> bool {
        self.core.slow.load(Ordering::Acquire)
    }

    /// Ticking at the full cadence: the next frame is at most one refresh away.
    pub fn is_full_rate(&self) -> bool {
        self.core.active.load(Ordering::Acquire) && !self.core.slow.load(Ordering::Acquire)
    }

    /// Switch to the slow cadence. Nothing brings the timer back unless the caller arranges a
    /// [`AnimTimer::wake`] (see `Inner::go_slow`).
    fn slow_down(&self) {
        if !self.core.slow.swap(true, Ordering::AcqRel) {
            TimerCore::signal(self.core.rate_evt);
        }
    }

    /// Back to full rate; the first tick is posted at once.
    pub fn wake(&self) {
        self.core.wake();
    }

    /// Ask the capture thread to wake this timer once the mic level leaves `lo..=hi`.
    fn wake_on_level_outside(&self, lo: f32, hi: f32) {
        let core = self.core.clone();
        let token = arm_level_wake(lo, hi, Box::new(move || core.wake()));
        let old = self.core.wake_token.swap(token, Ordering::AcqRel);
        if old != 0 {
            disarm_level_wake(old);
        }
    }

    /// Whether anything asked for full rate since the last call (clears the flag).
    pub fn take_woken(&self) -> bool {
        self.core.woken.swap(false, Ordering::AcqRel)
    }

    /// Time to sample the animation at for a frame drawn at `now`: when it will be shown (the next
    /// vertical blank) while the thread is vsync-paced, else `now`.
    pub fn present_time(&self, now: f64) -> f64 {
        self.core.hint.present_time(now)
    }

    /// Called by the window procedure when it consumed a tick.
    pub fn ack(&self) {
        self.pending.store(false, Ordering::Release);
    }
}

impl Drop for AnimTimer {
    fn drop(&mut self) {
        let token = self.core.wake_token.swap(0, Ordering::AcqRel);
        if token != 0 {
            disarm_level_wake(token);
        }
        TimerCore::signal(self.core.quit_evt);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

fn anim_thread(core: Arc<TimerCore>, sink: Box<dyn Fn() + Send>) {
    let (start, stop, quit, rate) = (handle(core.start_evt), handle(core.stop_evt), handle(core.quit_evt), handle(core.rate_evt));
    // SAFETY: the handles stay open while `core` is alive, and this thread holds a reference.
    unsafe {
        let timer = CreateWaitableTimerExW(None, PCWSTR::null(), CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, TIMER_ALL_ACCESS.0)
            .or_else(|_| CreateWaitableTimerExW(None, PCWSTR::null(), Default::default(), TIMER_ALL_ACCESS.0));
        let Ok(timer) = timer else { return };
        // The compositor's clock paces the full-rate cadence (see `pacer`); the fixed-rate timer
        // remains for the slow cadence and for a machine whose compositor does not block.
        let mut vsync = Vsync::new();
        // first tick one period from now, then periodic
        let arm = |slow: bool| {
            let ms = if slow { SLOW_TICK_MS } else { TICK_MS };
            let due = -(ms as i64) * 10_000;
            SetWaitableTimer(timer, &due, ms as i32, None, None, false).is_ok()
        };
        // Pick the pacing for a cadence: vsync at full rate when available (the timer is disarmed),
        // else the timer. Returns whether vsync paces.
        let pace = |slow: bool, vsync: &Option<Vsync>| -> Option<bool> {
            if !slow && vsync.is_some() {
                let _ = CancelWaitableTimer(timer);
                core.hint.clear();
                Some(true)
            } else {
                core.hint.clear();
                arm(slow).then_some(false)
            }
        };
        loop {
            let r = WaitForMultipleObjects(&[start, quit], false, INFINITE);
            if r.0 != WAIT_OBJECT_0.0 {
                break;
            }
            // Auto-reset events coalesce: a start/stop/start burst leaves a stale start signal, so
            // the shared flag, not the event, decides whether to run.
            if !core.active.load(Ordering::Acquire) {
                continue;
            }
            let mut slow = core.slow.load(Ordering::Acquire);
            let Some(mut paced) = pace(slow, &vsync) else { continue };
            loop {
                // 0: timer, 1: stop, 2: quit, 3: rate. Vsync-paced, the control events are polled
                // between frames (a frame is at most one refresh long).
                let idx = if paced {
                    let r = WaitForMultipleObjects(&[stop, quit, rate], false, 0);
                    if r == WAIT_TIMEOUT {
                        match vsync.as_mut().and_then(|v| v.wait()) {
                            Some((to_next, period)) => {
                                core.hint.set(crate::core::timing::now_ms() + to_next, period);
                                sink();
                            }
                            None => {
                                // the compositor stopped pacing (remote session, DWM off): timer from now on
                                vsync = None;
                                match pace(slow, &vsync) {
                                    Some(p) => paced = p,
                                    None => break,
                                }
                            }
                        }
                        continue;
                    }
                    r.0.wrapping_sub(WAIT_OBJECT_0.0) + 1
                } else {
                    WaitForMultipleObjects(&[timer, stop, quit, rate], false, INFINITE).0.wrapping_sub(WAIT_OBJECT_0.0)
                };
                match idx {
                    0 => sink(),
                    1 => {
                        let _ = CancelWaitableTimer(timer);
                        core.hint.clear();
                        // stop followed by start before this thread ran: keep going
                        if core.active.load(Ordering::Acquire) {
                            slow = core.slow.load(Ordering::Acquire);
                            if let Some(p) = pace(slow, &vsync) {
                                paced = p;
                                continue;
                            }
                        }
                        break;
                    }
                    3 => {
                        // the cadence flipped (events coalesce, so compare with what is armed)
                        let want = core.slow.load(Ordering::Acquire);
                        if want != slow {
                            slow = want;
                            match pace(slow, &vsync) {
                                Some(p) => {
                                    paced = p;
                                    if !slow {
                                        // woken: the first full-rate frame must not wait a period
                                        sink();
                                    }
                                }
                                None => {
                                    let _ = CancelWaitableTimer(timer);
                                }
                            }
                        }
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

    /// Copy the `r` part of a premultiplied RGBA frame into the BGRA bitmap (the whole frame for the
    /// full rect). `r` is clipped to the bitmap.
    fn upload(&mut self, rgba: &[u8], r: DevRect) {
        let n = (self.w * self.h * 4) as usize;
        if rgba.len() < n {
            return;
        }
        let r = DevRect { x0: r.x0.max(0), y0: r.y0.max(0), x1: r.x1.min(self.w), y1: r.y1.min(self.h) };
        if r.is_empty() {
            return;
        }
        // SAFETY: `bits` points to w*h*4 bytes owned by the DIB section.
        let dst = unsafe { std::slice::from_raw_parts_mut(self.bits, n) };
        let (stride, x0, x1) = (self.w as usize * 4, r.x0 as usize * 4, r.x1 as usize * 4);
        for y in r.y0 as usize..r.y1 as usize {
            let row = y * stride;
            for (d, s) in dst[row + x0..row + x1].chunks_exact_mut(4).zip(rgba[row + x0..row + x1].chunks_exact(4)) {
                d[0] = s[2];
                d[1] = s[1];
                d[2] = s[0];
                d[3] = s[3];
            }
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
    /// Renders only what moves once an animation has settled (see `render::FrameCache`).
    cache: FrameCache,
    /// Frames handled: every present call that had something to show, whether it was drawn or
    /// skipped because nothing visible changed (tests and diagnostics).
    pub frames: u64,
    /// Frames skipped because no pixel changed.
    pub skipped: u64,
    /// Frames that updated only part of the window (the dirty rect).
    pub partial: u64,
    /// Consecutive recording frames that changed nothing; a long run slows the animation timer.
    quiet: QuietRun,
    /// Time of the newest frame handed to the compositor (ms). Frames are sampled at the time they
    /// will be shown, which can be a vblank ahead of the clock; every later frame and every state
    /// change is stamped no earlier than this, so the animation never steps back.
    last_t: f64,
    /// Presentation time of the tick being handled (set only around that tick's frame).
    frame_t: Option<f64>,
    /// Screen capture under the window (the glass backdrop); see `capture.rs`.
    glass: GlassHost,
    /// Full rect of the monitor the window is on (device px).
    mon: (i32, i32, i32, i32),
    /// Period of the running backdrop timer, 0 while it is off.
    glass_ms: u32,
    /// Window origin the installed backdrop was placed for.
    bd_origin: (i32, i32),
    /// The one-shot deferred capture timer is armed.
    glass_settle: bool,
    /// WinEvent hooks that report other windows' activation / move / minimise while the pill is shown.
    screen_events: Option<ScreenEvents>,
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

/// Full monitor rect (`rcMonitor`), device px.
fn monitor_rect(hmon: HMONITOR) -> Option<(i32, i32, i32, i32)> {
    let mut mi = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
    // SAFETY: valid out-pointer with cbSize set.
    if unsafe { GetMonitorInfoW(hmon, &mut mi) }.as_bool() {
        let r = mi.rcMonitor;
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
        now_ms().max(self.last_t)
    }

    fn client_size(&self) -> (i32, i32) {
        let (w, h) = pixel_size(&self.st);
        (w as i32, h as i32)
    }

    fn ensure_buffers(&mut self) {
        let (w, h) = self.client_size();
        if (self.pm.width() as i32 != w || self.pm.height() as i32 != h)
            && let Some(pm) = Pixmap::new(w as u32, h as u32) {
                self.pm = pm;
                self.cache.reset();
            }
        if self.dib.as_ref().map(|d| d.w != w || d.h != h).unwrap_or(true) {
            self.dib = Dib::new(w, h);
            self.cache.reset();
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
        if let Some(m) = monitor_rect(hmon) {
            self.mon = m;
        }
        let s = monitor_scale(hmon);
        let before = self.st.scale;
        self.st.set_scale(s);
        if (self.st.scale - before).abs() > f32::EPSILON {
            // another DPI: build the glow sprites for it off the UI thread now, so the first stop on
            // this monitor does not pay ~25 ms of blur in the middle of the shrink
            let scale = self.st.scale;
            let _ = std::thread::Builder::new().name("pill-sprites".into()).spawn(move || {
                let _ = super::glow::Sprites::get(scale);
            });
        }
    }

    /// Re-layout (window size may have changed) and push a frame.
    fn relayout(&mut self) {
        self.ensure_buffers();
        // size, scale or dock changed: what is under the window is a different rect, capture at once
        self.glass.dirty = true;
        self.glass.last_ms = f64::NEG_INFINITY;
        // the window may have moved or been re-docked: push a full frame so the position is applied
        self.cache.reset();
        self.pos = match (self.custom_pos, self.st.error().is_some()) {
            (Some(p), false) => p,
            _ => self.default_pos(),
        };
        self.present();
    }

    /// Draw (or hide) the window, then keep the backdrop timer in step with what is shown.
    fn present(&mut self) {
        self.present_frame();
        self.glass_retune();
    }

    // --- glass backdrop -----------------------------------------------------------------

    /// The window's screen rect (device px).
    fn win_rect(&self) -> (i32, i32, i32, i32) {
        let (w, h) = self.client_size();
        (self.pos.0, self.pos.1, self.pos.0 + w, self.pos.1 + h)
    }

    /// Follow the monitor the window is on (it can be dragged onto another one, which changes the area
    /// that may be captured and the field's page size).
    fn sync_monitor_rect(&mut self) {
        let (l, t, r, b) = self.win_rect();
        let p = POINT { x: (l + r) / 2, y: (t + b) / 2 };
        // SAFETY: plain query.
        let hmon = unsafe { MonitorFromPoint(p, MONITOR_DEFAULTTONEAREST) };
        if let Some(m) = monitor_rect(hmon).filter(|m| *m != self.mon) {
            self.mon = m;
            self.glass.dirty = true;
        }
    }

    /// The monitor in logical px: the refraction field's `pageW` / `pageH`.
    fn page(&self) -> (f32, f32) {
        let s = self.st.scale;
        (((self.mon.2 - self.mon.0) as f32 / s).max(1.0), ((self.mon.3 - self.mon.1) as f32 / s).max(1.0))
    }

    /// The pill is changing size (expand, shrink, the cancel bud, the app bubble). The backdrop is
    /// frozen while it does: a capture mid-motion costs the compositor and the worker CPU exactly when
    /// every frame counts, and the ~10 px of screen the glass frosts does not change in 300 ms. The
    /// capture the motion postponed is taken as soon as it ends (the periodic timer, or a deferral).
    fn backdrop_frozen(&self, now: f64) -> bool {
        self.st.is_resizing(now)
    }

    /// Ask the worker for the screen under the window (no-op while one request is in flight; the
    /// result arrives as `WM_APP_GLASS`).
    fn request_backdrop(&mut self, now: f64) -> bool {
        self.sync_monitor_rect();
        let (win, page) = (self.win_rect(), self.page());
        // a periodic refresh keeps the capture buffer; an event-driven one gives it back after the copy
        let keep = self.glass_ms != 0;
        self.glass.request(win, self.mon, self.st.scale, page, now, keep)
    }

    /// Install the kept picture for the window's current origin; returns whether the glass changed.
    fn place_backdrop(&mut self) -> bool {
        self.bd_origin = self.pos;
        let b = self.glass.backdrop((self.pos.0, self.pos.1), self.page());
        self.st.set_backdrop(b)
    }

    /// `WM_APP_GLASS`: the worker finished. Install the picture and repaint only when it changed,
    /// then chain the next capture if something asked for one meanwhile.
    fn glass_done(&mut self) {
        let Some(result) = self.glass.take() else { return };
        if !self.visible {
            // hid while the capture ran: the next show places the kept picture and asks again
            return;
        }
        let changed = match result {
            Refresh::Unavailable => self.st.set_backdrop(None),
            Refresh::Unchanged => self.bd_origin != self.pos && self.place_backdrop(),
            Refresh::Changed => self.place_backdrop(),
        };
        if !self.glass.covers(self.win_rect(), self.mon, self.st.scale) {
            self.glass.dirty = true;
        }
        let now = self.now();
        if self.glass.dirty {
            let wait = GLASS_MIN_GAP_MS - (now - self.glass.last_ms);
            if wait <= 0.0 && !self.backdrop_frozen(now) {
                self.request_backdrop(now);
            } else if self.glass_ms == 0 {
                // no periodic timer to pick it up: capture once the rate limit allows
                self.glass_defer(wait.ceil() as u32);
            }
        }
        // a full-rate animation draws the new picture with its next frame, a vblank away: drawing it
        // here as well would cost a second frame inside the same refresh
        if changed && !self.timer.as_ref().is_some_and(|t| t.is_full_rate()) {
            self.present();
        }
    }

    /// Before a frame: place the kept picture when the window is about to appear and ask for a fresh
    /// one; when the window outgrew the picture or something marked it dirty (rate-limited), ask
    /// again; just re-place the kept picture when the window only moved.
    fn glass_before_frame(&mut self, was_visible: bool, now: f64) {
        if !self.glass.available() {
            return;
        }
        let due = now - self.glass.last_ms >= GLASS_MIN_GAP_MS;
        if !was_visible || self.bd_origin != self.pos {
            self.sync_monitor_rect();
        }
        if !was_visible {
            if self.glass.covers(self.win_rect(), self.mon, self.st.scale) {
                self.place_backdrop();
            } else {
                self.st.set_backdrop(None);
            }
            self.request_backdrop(now);
        } else if !self.glass.covers(self.win_rect(), self.mon, self.st.scale) || (self.glass.dirty && due && !self.backdrop_frozen(now)) {
            if !self.glass.scale_matches(self.st.scale) {
                // another DPI: the picture's cells are the wrong size, paint the neutral glass until the new one lands
                self.st.set_backdrop(None);
            } else if self.bd_origin != self.pos {
                self.place_backdrop();
            }
            self.request_backdrop(now);
        } else if self.bd_origin != self.pos {
            self.place_backdrop();
        }
    }

    /// How often the backdrop is refreshed in the current state (ms); 0 while the window is hidden,
    /// cannot capture, or rests.
    fn glass_period(&self, now: f64) -> u32 {
        let fast = self.st.pointer().group || self.down.as_ref().is_some_and(|d| d.dragging);
        // the error card sits still for 3-5 s (its countdown counts as an animation): the capture taken
        // as it appears is enough (rebuilding its glass costs up to ~30 ms at 2x), so it refreshes on
        // events only, like the resting circle
        let busy = !matches!(self.st.mode(), PillMode::Idle | PillMode::Hidden) || (self.st.error().is_none() && self.st.is_animating(now));
        glass_period_ms(self.visible && self.glass.available(), fast, busy)
    }

    /// Run the backdrop timer only while something is going on, at the rate the state calls for. With
    /// no periodic timer, a capture that is still owed (rate-limited) is deferred once instead.
    fn glass_retune(&mut self) {
        let now = self.now();
        let want = self.glass_period(now);
        if want == 0 && self.glass.dirty && self.visible && self.glass.available() && !self.glass.in_flight {
            let wait = (GLASS_MIN_GAP_MS - (now - self.glass.last_ms)).max(0.0);
            self.glass_defer(wait.ceil() as u32);
        }
        if want == self.glass_ms {
            return;
        }
        // SAFETY: a timer on our own window; replaced by id, killed when the window hides.
        unsafe {
            if want == 0 {
                let _ = KillTimer(Some(self.hwnd), GLASS_TIMER_ID);
            } else {
                SetTimer(Some(self.hwnd), GLASS_TIMER_ID, want, None);
            }
        }
        self.glass_ms = want;
    }

    /// Capture once, `ms` from now (a one-shot timer; an armed one is kept, so bursts coalesce).
    fn glass_defer(&mut self, ms: u32) {
        if self.glass_settle || !self.visible || !self.glass.available() {
            return;
        }
        // SAFETY: a timer on our own window; killed when it fires or the window hides.
        unsafe {
            SetTimer(Some(self.hwnd), GLASS_SETTLE_TIMER_ID, ms.max(10), None);
        }
        self.glass_settle = true;
    }

    fn glass_settle_cancel(&mut self) {
        if self.glass_settle {
            // SAFETY: killing our own timer.
            unsafe {
                let _ = KillTimer(Some(self.hwnd), GLASS_SETTLE_TIMER_ID);
            }
            self.glass_settle = false;
        }
    }

    /// The one-shot deferred capture is due.
    fn glass_settle_fire(&mut self) {
        self.glass_settle_cancel();
        if self.glass.has_result() {
            // its `WM_APP_GLASS` was lost: install it now (this also chains the owed capture)
            self.glass_done();
        }
        if !self.visible || !self.glass.available() || self.glass.in_flight {
            return;
        }
        self.glass.dirty = true;
        let now = self.now();
        if self.backdrop_frozen(now) {
            // the pill is still moving: look again shortly after it settles
            self.glass_defer(60);
            return;
        }
        let wait = GLASS_MIN_GAP_MS - (now - self.glass.last_ms);
        if wait <= 0.0 {
            self.request_backdrop(now);
        } else {
            self.glass_defer(wait.ceil() as u32);
        }
    }

    /// `WM_APP_SCREEN_EVENT`: another window was activated, moved, resized, minimised or restored.
    /// While the periodic timer runs it picks this up; a resting pill captures once, after a settle delay.
    fn glass_screen_event(&mut self) {
        if !self.visible || !self.glass.available() {
            return;
        }
        if self.glass.has_result() {
            self.glass_done();
        }
        self.glass.dirty = true;
        if self.glass_ms == 0 {
            self.glass_defer(GLASS_SETTLE_MS);
        }
    }

    /// The window hid: stop capturing, unhook the screen events and give the buffers back.
    fn glass_stop(&mut self) {
        if self.glass_ms != 0 {
            // SAFETY: killing our own timer.
            unsafe {
                let _ = KillTimer(Some(self.hwnd), GLASS_TIMER_ID);
            }
            self.glass_ms = 0;
        }
        self.glass_settle_cancel();
        self.screen_events = None;
        self.glass.release();
        self.st.set_backdrop(None);
    }

    /// `WM_TIMER`: ask for a new capture when the rate limit allows; the answer repaints.
    fn glass_tick(&mut self) {
        // a result whose `WM_APP_GLASS` post was dropped (full message queue) is picked up here, so a
        // lost message can never leave a request in flight forever
        if self.glass.has_result() {
            self.glass_done();
        }
        let now = self.now();
        if self.visible && self.glass.available() && now - self.glass.last_ms >= GLASS_MIN_GAP_MS && !self.backdrop_frozen(now) {
            self.request_backdrop(now);
        }
        self.glass_retune();
    }

    fn present_frame(&mut self) {
        let t = self.frame_t.take().unwrap_or_else(|| self.now()).max(self.last_t);
        self.last_t = t;
        let visible = self.st.wants_visible(t, self.opts.auto_hide) || self.st.is_animating(t);
        if !visible {
            // nothing to draw while hidden; the next visible frame is rendered from scratch
            if self.visible {
                // SAFETY: valid window handle.
                unsafe {
                    let _ = ShowWindow(self.hwnd, SW_HIDE);
                }
                self.visible = false;
                self.glass_stop();
            }
            self.cache.reset();
            self.quiet.reset();
            return;
        }
        self.ensure_buffers();
        if !self.visible {
            self.cache.reset();
        }
        self.glass_before_frame(self.visible, t);
        let outcome = self.cache.render(&self.st, t, &mut self.pm);
        self.frames += 1;
        let FrameOutcome::Dirty(dirty) = outcome else {
            // the pixmap and the DIB already show this frame
            self.skipped += 1;
            self.quiet.frame(true);
            return;
        };
        // something moved: no slow cadence until the picture is static again
        self.quiet.frame(false);
        if let Some(timer) = &self.timer {
            timer.wake();
        }
        let Some(dib) = self.dib.as_mut() else {
            self.cache.reset();
            return;
        };
        let full = dirty.is_full(dib.w, dib.h);
        dib.upload(self.pm.data(), dirty);
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
                if self.glass.available() && self.screen_events.is_none() {
                    self.screen_events = ScreenEvents::watch(self.hwnd);
                }
            }
            let mut pushed = false;
            if !full {
                // only `prcDirty` is recomposited; position and size stay as they are (a drag moves the
                // window with SetWindowPos)
                let rc = RECT { left: dirty.x0, top: dirty.y0, right: dirty.x1, bottom: dirty.y1 };
                let info = UPDATELAYEREDWINDOWINFO {
                    cbSize: std::mem::size_of::<UPDATELAYEREDWINDOWINFO>() as u32,
                    hdcSrc: dib.dc,
                    pptSrc: &src,
                    pblend: &blend,
                    dwFlags: ULW_ALPHA,
                    prcDirty: &rc,
                    ..Default::default()
                };
                pushed = UpdateLayeredWindowIndirect(self.hwnd, &info).as_bool();
                if pushed {
                    self.partial += 1;
                }
            }
            if !pushed {
                let _ = UpdateLayeredWindow(self.hwnd, None, Some(&pos), Some(&size), Some(dib.dc), Some(&src), COLORREF(0), Some(&blend), ULW_ALPHA);
            }
        }
    }

    fn sync_timer(&mut self) {
        let t = self.now();
        if let Some(timer) = &self.timer
            && self.st.is_animating(t) {
                // an outside change (mode, pointer, options) always gets full rate
                self.quiet.reset();
                timer.start();
            }
    }

    /// The recording is visually static: tick slowly and ask the capture thread to wake the timer
    /// as soon as the mic level leaves the band that would change a bar. Nothing polls.
    fn go_slow(&mut self) {
        let Some(timer) = &self.timer else { return };
        if timer.is_slow() || timer.take_woken() {
            return;
        }
        let baseline = f32::from_bits(self.level.load(Ordering::Relaxed));
        let (lo, hi) = level_band(baseline, BAR_EPS);
        // Order matters: flag first, then arm, then re-check, so a rise at any point in between is
        // either announced by the capture thread or caught here.
        timer.slow_down();
        timer.wake_on_level_outside(lo, hi);
        let now_level = f32::from_bits(self.level.load(Ordering::Relaxed));
        if now_level < lo || now_level > hi {
            timer.wake();
        }
    }

    /// State changed from outside the message loop: draw once and (re)arm the timer.
    pub fn refresh(&mut self) {
        self.glass.dirty = true;
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
        // the screen under the window changed: the timer (now at the fast rate while dragging)
        // recaptures, or a deferred capture once it rests; between captures the kept picture stays
        // where it was on the screen
        self.glass.dirty = true;
        self.glass_retune();
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
            WM_CAPTURECHANGED => {
                // capture taken away mid-press (Alt+Tab, a modal, ...): end the press so a later
                // hover is not treated as a drag. Our own ReleaseCapture re-enters while borrowed
                // and never reaches here.
                if self.down.take().is_some() {
                    self.st.set_dragging(false);
                    self.update_pointer(Hit::None);
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
                    if t.take_woken() {
                        self.quiet.reset();
                    }
                }
                // The frame is sampled when it will be shown (the next vblank), not when this
                // message happens to be handled: a late tick cannot slow the animation down.
                let now = self.now();
                let now = self.timer.as_ref().map_or(now, |t| t.present_time(now));
                let rms = f32::from_bits(self.level.load(Ordering::Relaxed));
                let o: TickOutcome = self.st.tick(now, rms);
                if o.error_expired {
                    out.events.push(PillEvent::ErrorDismissed);
                }
                self.frame_t = Some(now);
                if o.error_gone {
                    self.relayout();
                } else {
                    self.present();
                }
                self.frame_t = None;
                // the frame above was the final one at rest (it also hid an idle auto-hide pill)
                if !self.st.is_animating(now) {
                    if let Some(t) = &self.timer {
                        t.stop();
                    }
                } else if self.quiet.is_quiet() && self.st.mode() == PillMode::Recording {
                    self.go_slow();
                }
                out.result = Some(LRESULT(0));
            }
            WM_APP_GLASS => {
                self.glass_done();
                out.result = Some(LRESULT(0));
            }
            WM_TIMER if wp.0 == GLASS_TIMER_ID => {
                self.glass_tick();
                out.result = Some(LRESULT(0));
            }
            WM_TIMER if wp.0 == GLASS_SETTLE_TIMER_ID => {
                self.glass_settle_fire();
                out.result = Some(LRESULT(0));
            }
            WM_APP_SCREEN_EVENT => {
                self.glass_screen_event();
                out.result = Some(LRESULT(0));
            }
            WM_SETTINGCHANGE => {
                // wallpaper, colours, accent: what is behind the glass may have changed
                self.glass_screen_event();
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
            WM_DPICHANGED => {
                // scaling changed on our monitor; layered windows ignore the suggested rect
                self.refresh_monitor(self.monitor);
                self.relayout();
                out.result = Some(LRESULT(0));
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
    if outcome.menu
        && let Some(ev) = run_menu(&rc, hwnd) {
            events.push(ev);
        }
    for ev in events {
        let cb = rc.try_borrow_mut().ok().and_then(|mut i| i.on_event.take());
        if let Some(mut cb) = cb {
            cb(ev);
            if let Ok(mut i) = rc.try_borrow_mut()
                && i.on_event.is_none() {
                    i.on_event = Some(cb);
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
                cache: FrameCache::new(),
                frames: 0,
                skipped: 0,
                partial: 0,
                quiet: QuietRun::default(),
                last_t: f64::NEG_INFINITY,
                frame_t: None,
                glass: GlassHost::new(hwnd),
                mon: monitor_rect(hmon).unwrap_or((0, 0, 1920, 1080)),
                glass_ms: 0,
                bd_origin: (0, 0),
                glass_settle: false,
                screen_events: None,
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

    #[cfg(test)]
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
        // the card is anchored at the default spot (see `relayout`); a dragged position comes back after it
        self.relayout();
        self.sync_timer();
    }

    pub fn set_options(&mut self, opts: PillOptions) {
        let now = self.now();
        let dark = resolve_dark(opts.theme);
        if opts.position != self.opts.position {
            self.custom_pos = None;
        }
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
        self.relayout();
        self.sync_timer();
    }

    pub fn follow_cursor_display(&mut self) {
        let hmon = cursor_monitor();
        if hmon != self.monitor {
            self.custom_pos = None;
        }
        // also picks up a scaling change on the same monitor
        self.refresh_monitor(hmon);
        self.relayout();
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backdrop_timer_runs_only_while_shown_and_at_the_rate_the_state_needs() {
        // hidden or unable to capture: no timer at all (idle CPU stays flat)
        assert_eq!(glass_period_ms(false, true, true), 0);
        assert_eq!(glass_period_ms(false, false, false), 0);
        // resting circle (and a still error card): no timer either (PLAN section 1), it refreshes on
        // events; recording / thinking: 4 Hz; hover / drag: 8 Hz
        assert_eq!(glass_period_ms(true, false, false), 0);
        assert_eq!(glass_period_ms(true, false, true), GLASS_ACTIVE_MS);
        assert_eq!(glass_period_ms(true, true, false), GLASS_FAST_MS);
        assert_eq!(glass_period_ms(true, true, true), GLASS_FAST_MS);
        // the rates are ordered and keep the rate limit meaningful
        const { assert!(GLASS_FAST_MS < GLASS_ACTIVE_MS && GLASS_SETTLE_MS as f64 > GLASS_MIN_GAP_MS) };
        const { assert!(GLASS_FAST_MS as f64 >= GLASS_MIN_GAP_MS) };
    }

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

    /// Timer whose ticks are only recorded (no window involved).
    fn recording_timer() -> (AnimTimer, Arc<std::sync::Mutex<Vec<std::time::Instant>>>) {
        let stamps = Arc::new(std::sync::Mutex::new(Vec::new()));
        let s = stamps.clone();
        let sink = move || s.lock().unwrap_or_else(|e| e.into_inner()).push(std::time::Instant::now());
        (AnimTimer::with_sink(Box::new(sink), Arc::new(AtomicBool::new(false))).expect("timer"), stamps)
    }

    fn tick_count(stamps: &std::sync::Mutex<Vec<std::time::Instant>>) -> usize {
        stamps.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    /// Wait (spinning, no sleep granularity) until a tick after `before` arrives; its time since `t0`.
    fn first_tick_after(stamps: &std::sync::Mutex<Vec<std::time::Instant>>, before: usize, t0: std::time::Instant) -> Option<std::time::Duration> {
        while t0.elapsed() < std::time::Duration::from_millis(250) {
            if let Some(at) = stamps.lock().unwrap_or_else(|e| e.into_inner()).get(before) {
                return Some(at.saturating_duration_since(t0));
            }
            std::thread::yield_now();
        }
        None
    }

    #[test]
    fn slow_cadence_wakes_the_thread_under_20_times_a_second() {
        use std::time::Duration;
        let (timer, stamps) = recording_timer();
        timer.start();
        std::thread::sleep(Duration::from_millis(250));
        let fast = tick_count(&stamps);
        assert!(fast >= 6, "full rate ticks ~62 Hz, got {fast} in 250 ms");
        timer.slow_down();
        assert!(timer.is_slow());
        std::thread::sleep(Duration::from_millis(150)); // the thread re-arms; a tick already due may still land
        let n0 = tick_count(&stamps);
        std::thread::sleep(Duration::from_millis(1000));
        let slow = tick_count(&stamps) - n0;
        assert!((8..20).contains(&slow), "{slow} timer wakeups in one silent second");
        timer.stop();
        std::thread::sleep(Duration::from_millis(50));
        let n1 = tick_count(&stamps);
        std::thread::sleep(Duration::from_millis(150));
        assert_eq!(tick_count(&stamps), n1, "a stopped timer is silent");
    }

    #[test]
    fn wake_brings_back_a_full_rate_tick_within_one_frame() {
        use std::time::{Duration, Instant};
        let (timer, stamps) = recording_timer();
        timer.start();
        timer.slow_down();
        let mut lat = Vec::new();
        for _ in 0..9 {
            std::thread::sleep(Duration::from_millis(100)); // settled in slow mode, off any tick boundary
            let before = tick_count(&stamps);
            let t0 = Instant::now();
            timer.wake();
            lat.push(first_tick_after(&stamps, before, t0).expect("a tick after the wake"));
            assert!(!timer.is_slow());
            assert!(timer.take_woken());
            timer.slow_down();
        }
        lat.sort();
        let median = lat[lat.len() / 2];
        assert!(median <= Duration::from_millis(17), "median wake-to-tick {median:?} (all: {lat:?})");
        // and the cadence after a wake is the full one again
        timer.wake();
        std::thread::sleep(Duration::from_millis(100));
        let n0 = tick_count(&stamps);
        std::thread::sleep(Duration::from_millis(320));
        let n = tick_count(&stamps) - n0;
        assert!(n >= 12, "full rate after a wake: {n} ticks in 320 ms");
    }

    /// The capture side signals the timer: a level outside the armed band wakes it, no polling.
    #[test]
    fn capture_level_change_wakes_a_slow_timer() {
        use crate::daemon::audio::{publish_level, serial_guard};
        use std::time::{Duration, Instant};
        let _serial = serial_guard();
        let level = AtomicU32::new(0);
        let (timer, stamps) = recording_timer();
        timer.start();
        timer.slow_down();
        let (lo, hi) = level_band(0.0, BAR_EPS);
        timer.wake_on_level_outside(lo, hi);
        std::thread::sleep(Duration::from_millis(150));
        // inside the band (digital silence): the timer stays slow
        publish_level(&level, 0.0);
        assert!(timer.is_slow());
        let before = tick_count(&stamps);
        let t0 = Instant::now();
        publish_level(&level, 0.05);
        let lat = first_tick_after(&stamps, before, t0).expect("a tick after the level rise");
        assert!(lat <= Duration::from_millis(40), "level rise to tick: {lat:?}");
        assert!(!timer.is_slow());
        assert!(timer.take_woken());
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

/// Desktop tests of the incremental present path (real layered window). Run with
/// `cargo test desktop_ -- --ignored --test-threads=1`.
#[cfg(test)]
mod steady {
    use super::*;
    use std::time::{Duration, Instant};
    use windows::Win32::UI::WindowsAndMessaging::{DispatchMessageW, MSG, PM_REMOVE, PeekMessageW, TranslateMessage};

    /// Pump messages for `ms`, feeding `level` a speech-like RMS (or silence) every few ms.
    fn pump(level: &AtomicU32, ms: u64, speaking: bool) {
        let t0 = Instant::now();
        while t0.elapsed() < Duration::from_millis(ms) {
            let rms = if speaking { 0.02 + 0.2 * ((t0.elapsed().as_secs_f32() * 7.3).sin().abs()) } else { 0.0 };
            // what the capture thread does with each packet (store the level, wake a slowed timer)
            crate::daemon::audio::publish_level(level, rms);
            // SAFETY: standard message pump on the creating thread.
            unsafe {
                let mut msg = MSG::default();
                while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    fn make(level: Arc<AtomicU32>) -> Window {
        let opts = PillOptions { position: PanelPosition::BottomRight, auto_hide: false, show_app_bubble: false, theme: Theme::Dark, hotkey_label: String::new() };
        Window::create(level, opts, Box::new(|_| {})).expect("create pill window")
    }

    /// The DIB the window shows must equal the pixmap the cache maintains.
    fn dib_matches_pixmap(i: &Inner) -> bool {
        let Some(d) = i.dib.as_ref() else { return false };
        let n = (d.w * d.h * 4) as usize;
        // SAFETY: `bits` points to w*h*4 bytes owned by the DIB section.
        let bgra = unsafe { std::slice::from_raw_parts(d.bits, n) };
        i.pm.data().chunks_exact(4).zip(bgra.chunks_exact(4)).all(|(p, b)| p[0] == b[2] && p[1] == b[1] && p[2] == b[0] && p[3] == b[3])
    }

    #[test]
    #[ignore]
    fn desktop_pill_steady_recording_presents_only_the_dirty_lane() {
        let _serial = crate::daemon::audio::serial_guard();
        let level = Arc::new(AtomicU32::new(0));
        let w = make(level.clone());
        w.with(|i| i.set_mode(PillMode::Recording));
        pump(&level, 1500, true); // entrance (full frames) is over
        let (f0, s0, p0) = w.with(|i| (i.frames, i.skipped, i.partial));
        pump(&level, 1000, true);
        let (f1, s1, p1) = w.with(|i| (i.frames, i.skipped, i.partial));
        let (frames, skipped, partial) = (f1 - f0, s1 - s0, p1 - p0);
        println!("speaking: {frames} frames, {partial} dirty-rect presents, {skipped} skipped");
        assert!((40..=80).contains(&frames), "~60 fps timer, got {frames}");
        assert!(partial >= frames / 2, "most speaking frames are dirty-rect updates ({partial}/{frames})");
        assert!(frames - skipped - partial <= 2, "no full repaints in steady state ({} of {frames})", frames - skipped - partial);
        assert!(w.with(|i| dib_matches_pixmap(i)), "DIB equals the pixmap after dirty-rect uploads");

        // silence: the bars settle at 4 px, every later tick is skipped and the timer slows down
        pump(&level, 1300, false); // the 11-sample history (880 ms) flushes out
        assert!(w.with(|i| i.timer.as_ref().is_some_and(|t| t.is_slow())), "a silent recording runs the slow cadence");
        let (f2, s2, p2) = w.with(|i| (i.frames, i.skipped, i.partial));
        pump(&level, 1000, false);
        let (f3, s3, p3) = w.with(|i| (i.frames, i.skipped, i.partial));
        println!("silent: {} frames/s, {} dirty, {} skipped", f3 - f2, p3 - p2, s3 - s2);
        assert!(f3 - f2 < 20, "under 20 timer wakeups in a silent second, got {}", f3 - f2);
        assert_eq!(p3 - p2, 0, "silence presents nothing");
        assert_eq!(s3 - s2, f3 - f2, "every silent tick is skipped");
        assert!(w.with(|i| dib_matches_pixmap(i)));

        // speech starts: the capture side wakes the timer and the next tick is a full-rate one
        let frames_before = w.with(|i| i.frames);
        let t_rise = Instant::now();
        crate::daemon::audio::publish_level(&level, 0.2);
        let mut first_frame = None;
        while t_rise.elapsed() < Duration::from_millis(200) {
            // SAFETY: standard message pump on the creating thread.
            unsafe {
                let mut msg = MSG::default();
                while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
            if w.with(|i| i.frames) > frames_before {
                first_frame = Some(t_rise.elapsed());
                break;
            }
        }
        let first_frame = first_frame.expect("a frame after the level rise");
        println!("level rise to first frame: {first_frame:?}");
        assert!(first_frame <= Duration::from_millis(17), "first full-rate tick {first_frame:?} after the rise");
        assert!(w.with(|i| i.timer.as_ref().is_some_and(|t| !t.is_slow())), "full rate again");

        // cost of one lane present vs a forced full repaint, timed around `present()` alone: stop the
        // timer thread's ticks from interfering by pumping nothing while a bar eases
        pump(&level, 200, true);
        let cost = |force_full: bool| {
            let (mut total, n) = (Duration::ZERO, 12);
            for k in 0..n {
                std::thread::sleep(Duration::from_millis(81));
                total += w.with(|i| {
                    let rms = if k % 2 == 0 { 0.2f32 } else { 0.05 };
                    let now = i.now();
                    i.st.tick(now, rms);
                    std::thread::sleep(Duration::from_millis(25)); // bars are mid-ease
                    if force_full {
                        i.cache.reset();
                    }
                    let t0 = Instant::now();
                    i.present();
                    t0.elapsed()
                });
            }
            total.as_secs_f64() * 1000.0 / n as f64
        };
        let (lane, full) = (cost(false), cost(true));
        println!("present cost: lane update {lane:.3} ms vs full repaint {full:.3} ms");
        assert!(lane < full, "the lane update is the cheaper one");
    }

    #[test]
    #[ignore]
    fn desktop_pill_thinking_glow_presents_partial_frames() {
        let level = Arc::new(AtomicU32::new(0));
        let w = make(level.clone());
        w.with(|i| i.set_mode(PillMode::Recording));
        pump(&level, 900, true);
        w.with(|i| i.set_mode(PillMode::Thinking));
        pump(&level, 1200, false); // shrink + glow fade-in are full frames
        let (f0, p0) = w.with(|i| (i.frames, i.partial));
        pump(&level, 800, false);
        let (f1, p1) = w.with(|i| (i.frames, i.partial));
        println!("thinking: {} frames, {} dirty-rect presents", f1 - f0, p1 - p0);
        assert!(f1 - f0 > 30 && p1 - p0 >= (f1 - f0) * 9 / 10, "glow frames are dirty-rect updates");
        assert!(w.with(|i| dib_matches_pixmap(i)));
        // back to idle: the animation stops and the frame counter freezes
        w.with(|i| i.set_mode(PillMode::Idle));
        pump(&level, 900, false);
        let f2 = w.with(|i| i.frames);
        pump(&level, 400, false);
        assert_eq!(w.with(|i| i.frames), f2, "no frames while idle");
    }
}

#[cfg(test)]
#[path = "desktop_tests.rs"]
mod desktop;
