//! Screen capture for the glass backdrop (Win32 only; everything else is in `glass.rs`).
//!
//! The pill is a layered window, so DWM backdrops do not apply: the screen under it is copied with
//! `BitBlt` from the screen DC. To keep the pill out of its own backdrop the window is excluded from
//! capture (`SetWindowDisplayAffinity(WDA_EXCLUDEFROMCAPTURE)`, Windows 10 2004+). That has a visible
//! side effect: the pill is also absent from screenshots, recordings and screen sharing.

use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BitBlt, CAPTUREBLT, CreateCompatibleDC, CreateDIBSection, DIB_RGB_COLORS, DeleteDC, DeleteObject, GdiFlush,
    GetDC, HBITMAP, HDC, HGDIOBJ, ROP_CODE, ReleaseDC, SRCCOPY, SelectObject,
};
use windows::Win32::UI::WindowsAndMessaging::{SetWindowDisplayAffinity, WDA_EXCLUDEFROMCAPTURE};

/// `OPENTALK_NO_GLASS_CAPTURE=1` (any non-empty value other than `0`) turns the screen capture off:
/// no display-affinity flag, no `BitBlt`, the neutral frosted glass. For A/B comparisons of waveform
/// smoothness on a real machine (if the lag goes away with it, the capture path was the cause).
pub const NO_GLASS_CAPTURE_ENV: &str = "OPENTALK_NO_GLASS_CAPTURE";

fn capture_disabled_from(v: Option<std::ffi::OsString>) -> bool {
    v.is_some_and(|s| !s.is_empty() && s != "0")
}

/// Exclude `hwnd` from screen capture. `false` when Windows refuses (before 10 2004 the flag is
/// unknown): the caller must then not capture, or the pill would see itself.
pub fn exclude_from_capture(hwnd: HWND) -> bool {
    // SAFETY: plain call on our own window handle.
    unsafe { SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE) }.is_ok()
}

/// Whether `hwnd` is currently excluded from capture (tests).
#[cfg(test)]
pub fn is_excluded(hwnd: HWND) -> bool {
    use windows::Win32::UI::WindowsAndMessaging::{GetWindowDisplayAffinity, WINDOW_DISPLAY_AFFINITY};
    let mut affinity = 0u32;
    // SAFETY: valid window handle and out-pointer.
    unsafe { GetWindowDisplayAffinity(hwnd, &mut affinity) }.is_ok() && WINDOW_DISPLAY_AFFINITY(affinity) == WDA_EXCLUDEFROMCAPTURE
}

/// A memory DC with a top-down 32-bit DIB that screen rects are copied into. The DIB is exactly the
/// size of the last request (recreated when it changes), so a grab is one `BitBlt` and no copy.
pub struct Capturer {
    dc: HDC,
    bmp: HBITMAP,
    old: HGDIOBJ,
    bits: *mut u8,
    w: i32,
    h: i32,
}

impl Capturer {
    pub fn new() -> Option<Capturer> {
        // SAFETY: creates a memory DC; released in Drop.
        let dc = unsafe { CreateCompatibleDC(None) };
        if dc.is_invalid() {
            return None;
        }
        Some(Capturer { dc, bmp: HBITMAP::default(), old: HGDIOBJ::default(), bits: std::ptr::null_mut(), w: 0, h: 0 })
    }

    fn release_bitmap(&mut self) {
        if !self.bmp.is_invalid() {
            // SAFETY: `bmp` was selected into `dc` by `resize`; deselect before deleting.
            unsafe {
                SelectObject(self.dc, self.old);
                let _ = DeleteObject(HGDIOBJ(self.bmp.0));
            }
        }
        self.bmp = HBITMAP::default();
        self.bits = std::ptr::null_mut();
        self.w = 0;
        self.h = 0;
    }

    fn resize(&mut self, w: i32, h: i32) -> bool {
        if self.w == w && self.h == h && !self.bits.is_null() {
            return true;
        }
        self.release_bitmap();
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
        // SAFETY: standard DIB section creation; `bits` is owned by the bitmap and valid until it is deleted.
        let Ok(bmp) = (unsafe { CreateDIBSection(Some(self.dc), &bmi, DIB_RGB_COLORS, &mut bits, None, 0) }) else { return false };
        if bits.is_null() {
            // SAFETY: the bitmap was just created and is not selected anywhere.
            unsafe {
                let _ = DeleteObject(HGDIOBJ(bmp.0));
            }
            return false;
        }
        // SAFETY: selecting our own bitmap into our own DC.
        self.old = unsafe { SelectObject(self.dc, HGDIOBJ(bmp.0)) };
        self.bmp = bmp;
        self.bits = bits as *mut u8;
        self.w = w;
        self.h = h;
        true
    }

    /// Copy the screen rect at `(x, y)` of size `w` x `h` (device px, virtual-screen coordinates) and
    /// return it as top-down BGRA (`w * h * 4` bytes). `None` when the copy fails (secure desktop,
    /// locked session, no GDI resources).
    pub fn grab(&mut self, x: i32, y: i32, w: i32, h: i32) -> Option<&[u8]> {
        if w <= 0 || h <= 0 || w > 16384 || h > 16384 || !self.resize(w, h) {
            return None;
        }
        // SAFETY: the screen DC is released before returning; the memory DC holds a w x h DIB.
        let ok = unsafe {
            let screen = GetDC(None);
            if screen.is_invalid() {
                return None;
            }
            let ok = BitBlt(self.dc, 0, 0, w, h, Some(screen), x, y, ROP_CODE(SRCCOPY.0 | CAPTUREBLT.0)).is_ok();
            ReleaseDC(None, screen);
            // make sure the DIB holds the finished copy before it is read
            let _ = GdiFlush();
            ok
        };
        if !ok {
            return None;
        }
        // SAFETY: `bits` points at w*h*4 bytes of the DIB created for exactly this size.
        Some(unsafe { std::slice::from_raw_parts(self.bits, (w * h * 4) as usize) })
    }
}

impl Drop for Capturer {
    fn drop(&mut self) {
        self.release_bitmap();
        // SAFETY: the DC was created in `new`.
        unsafe {
            let _ = DeleteDC(self.dc);
        }
    }
}

// --- the host: capture, hash, frost (on a worker thread) -------------------------------------------

use super::diag::Series;
use super::glass::{self, Backdrop, Cells, Frosted};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use windows::Win32::Foundation::{LPARAM, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_APP};

/// Posted to the pill window when the worker has a result for [`GlassHost::take`].
pub const WM_APP_GLASS: u32 = WM_APP + 0x51;

/// What a finished capture found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refresh {
    /// Capture is not possible (window not excluded, grab failed): paint over the neutral backdrop.
    Unavailable,
    /// Same picture in the same place: nothing to recompose.
    Unchanged,
    /// New picture or new placement: install [`GlassHost::backdrop`].
    Changed,
}

/// A request for the worker: grab this screen rect and frost it at this scale.
struct Request {
    region: (i32, i32, i32, i32),
    scale: f32,
    /// Keep the capture buffer for the next request (a periodic refresh is running); otherwise it is
    /// given back right after the copy, so the resting circle does not hold up to 2 MB between captures.
    keep: bool,
}

enum Msg {
    Grab(Request),
    /// The window hid: give the capture buffer back.
    Release,
    Quit,
}

/// A picture's identity: cell hash, captured region, scale bits.
type Identity = (u64, (i32, i32, i32, i32), u32);

enum Done {
    Unavailable,
    Unchanged { region: (i32, i32, i32, i32), scale: f32 },
    Changed { frosted: Frosted, region: (i32, i32, i32, i32), scale: f32 },
}

/// The worker: owns the [`Capturer`], blocks on its channel (no polling, no CPU while idle) and does
/// everything that waits or loops over pixels: `BitBlt` (it waits for the compositor's next frame, so
/// it must never run on the UI thread), the cell average + hash, and the frost when the hash changed.
fn worker(rx: Receiver<Msg>, out: Arc<Mutex<Option<Done>>>, hwnd: isize, alive: Arc<AtomicBool>, timing: Arc<Mutex<Series>>) {
    let mut cap: Option<Capturer> = None;
    // the last picture's identity: a grab with the same cell hash, region and scale changes nothing
    let mut last: Option<Identity> = None;
    while let Ok(msg) = rx.recv() {
        match msg {
            Msg::Quit => break,
            Msg::Release => {
                cap = None;
            }
            Msg::Grab(req) => {
                let t0 = std::time::Instant::now();
                let done = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| grab_one(&mut cap, &mut last, &req))).unwrap_or(Done::Unavailable);
                // wall time of the whole grab (the `BitBlt` waits for the compositor and dominates it)
                if let Ok(mut t) = timing.lock() {
                    t.push(t0.elapsed().as_secs_f32() * 1000.0);
                }
                if !req.keep {
                    cap = None;
                }
                if let Ok(mut slot) = out.lock() {
                    *slot = Some(done);
                }
                if alive.load(Ordering::Acquire) {
                    // SAFETY: posting to a window handle is safe from any thread; a stale handle just fails.
                    unsafe {
                        let _ = PostMessageW(Some(windows::Win32::Foundation::HWND(hwnd as *mut core::ffi::c_void)), WM_APP_GLASS, WPARAM(0), LPARAM(0));
                    }
                }
            }
        }
    }
}

fn grab_one(cap: &mut Option<Capturer>, last: &mut Option<Identity>, req: &Request) -> Done {
    if cap.is_none() {
        *cap = Capturer::new();
    }
    let Some(c) = cap.as_mut() else { return Done::Unavailable };
    let (w, h) = (req.region.2 - req.region.0, req.region.3 - req.region.1);
    let Some(raw) = c.grab(req.region.0, req.region.1, w, h) else {
        *last = None;
        return Done::Unavailable;
    };
    let Some(cells) = Cells::from_bgra(raw, w as usize, h as usize, glass::cell_for(req.scale)) else { return Done::Unavailable };
    let ident = (cells.id, req.region, req.scale.to_bits());
    if *last == Some(ident) {
        return Done::Unchanged { region: req.region, scale: req.scale };
    }
    *last = Some(ident);
    Done::Changed { frosted: cells.frost(glass::FROST_SIGMA, req.scale), region: req.region, scale: req.scale }
}

/// Owns the screen capture of one pill window. The window is excluded from capture so the copy never
/// contains the pill; the copy itself runs on a worker thread (spawned on the first request, parked
/// while nothing is asked, stopped on drop), and the UI thread only installs finished pictures.
pub struct GlassHost {
    /// The window is excluded from capture, so grabbing the screen cannot see the pill itself.
    excluded: bool,
    hwnd: isize,
    tx: Option<Sender<Msg>>,
    worker: Option<std::thread::JoinHandle<()>>,
    out: Arc<Mutex<Option<Done>>>,
    alive: Arc<AtomicBool>,
    /// A request is with the worker.
    pub in_flight: bool,
    frosted: Option<Arc<Frosted>>,
    /// Screen rect (device px) the kept picture was captured from, and the scale it was built at.
    region: Option<(i32, i32, i32, i32)>,
    scale_bits: u32,
    /// Time of the last request (ms, the window clock).
    pub last_ms: f64,
    /// Something changed that may have altered what is under the window; capture when allowed.
    pub dirty: bool,
    /// Finished captures, and how many of them found the picture unchanged (diagnostics and tests).
    pub grabs: u64,
    pub unchanged: u64,
    /// Wall time of each grab on the worker, ms (diagnostics; the worker writes, the window reads).
    pub timing: Arc<Mutex<Series>>,
}

impl GlassHost {
    /// Exclude `hwnd` from capture. When Windows refuses (before 10 2004) the host is permanently
    /// unavailable and the pill keeps its neutral frosted glass.
    pub fn new(hwnd: HWND) -> GlassHost {
        if capture_disabled_from(std::env::var_os(NO_GLASS_CAPTURE_ENV)) {
            crate::log_info!("glass: screen capture disabled by {NO_GLASS_CAPTURE_ENV}");
            return GlassHost::with_exclusion(hwnd.0 as isize, false);
        }
        GlassHost::with_exclusion(hwnd.0 as isize, exclude_from_capture(hwnd))
    }

    fn with_exclusion(hwnd: isize, excluded: bool) -> GlassHost {
        GlassHost {
            excluded,
            hwnd,
            tx: None,
            worker: None,
            out: Arc::new(Mutex::new(None)),
            alive: Arc::new(AtomicBool::new(true)),
            in_flight: false,
            frosted: None,
            region: None,
            scale_bits: 0,
            last_ms: f64::NEG_INFINITY,
            dirty: true,
            grabs: 0,
            unchanged: 0,
            timing: Arc::new(Mutex::new(Series::default())),
        }
    }

    /// Can this host capture at all?
    pub fn available(&self) -> bool {
        self.excluded
    }

    /// Ask the worker to grab the screen under the window (`win`, device px, screen coordinates) on
    /// `monitor`, clipped and extended by the refraction margin. `page` is the monitor in logical px.
    /// At most one request is in flight; returns whether one was sent.
    pub fn request(&mut self, win: (i32, i32, i32, i32), monitor: (i32, i32, i32, i32), scale: f32, page: (f32, f32), now: f64, keep: bool) -> bool {
        if !self.excluded || self.in_flight {
            return false;
        }
        let margin = (glass::margin_logical(page) * scale).ceil() as i32;
        let Some(region) = glass::capture_region(win, margin, monitor) else { return false };
        if self.tx.is_none() && !self.spawn() {
            self.excluded = false;
            return false;
        }
        let sent = self.tx.as_ref().is_some_and(|tx| tx.send(Msg::Grab(Request { region, scale, keep })).is_ok());
        if !sent {
            // the worker is gone: never capture again rather than wait for an answer that cannot come
            self.excluded = false;
            return false;
        }
        self.in_flight = true;
        self.last_ms = now;
        self.dirty = false;
        true
    }

    fn spawn(&mut self) -> bool {
        let (tx, rx) = channel();
        let (out, alive, hwnd, timing) = (self.out.clone(), self.alive.clone(), self.hwnd, self.timing.clone());
        match std::thread::Builder::new().name("pill-glass".into()).spawn(move || worker(rx, out, hwnd, alive, timing)) {
            Ok(j) => {
                self.tx = Some(tx);
                self.worker = Some(j);
                true
            }
            Err(_) => false,
        }
    }

    /// The worker has finished a request whose `WM_APP_GLASS` has not been handled yet (a post can be
    /// lost when the message queue is full; the next event or timer picks the result up).
    pub fn has_result(&self) -> bool {
        self.in_flight && self.out.lock().is_ok_and(|s| s.is_some())
    }

    /// Take the worker's finished result (on `WM_APP_GLASS`); `None` when there is nothing new.
    pub fn take(&mut self) -> Option<Refresh> {
        let done = self.out.lock().ok()?.take()?;
        self.in_flight = false;
        Some(match done {
            Done::Unavailable => {
                self.frosted = None;
                self.region = None;
                Refresh::Unavailable
            }
            Done::Unchanged { region, scale } => {
                self.grabs += 1;
                self.unchanged += 1;
                // the picture is the one we keep; make sure its placement is the requested one
                if self.region != Some(region) || self.scale_bits != scale.to_bits() {
                    return Some(Refresh::Unavailable);
                }
                Refresh::Unchanged
            }
            Done::Changed { frosted, region, scale } => {
                self.grabs += 1;
                self.frosted = Some(Arc::new(frosted));
                self.region = Some(region);
                self.scale_bits = scale.to_bits();
                Refresh::Changed
            }
        })
    }

    /// The kept frosted picture placed for a window whose top-left is at `origin` (screen device px).
    pub fn backdrop(&self, origin: (i32, i32), page: (f32, f32)) -> Option<Arc<Backdrop>> {
        let (f, r) = (self.frosted.as_ref()?, self.region?);
        Some(Arc::new(Backdrop::new(f.clone(), r.0 - origin.0, r.1 - origin.1, page)))
    }

    /// Was the kept picture built at this device scale? (A picture from another DPI has cells of the
    /// wrong size and must not be painted.)
    pub fn scale_matches(&self, scale: f32) -> bool {
        self.frosted.is_none() || self.scale_bits == scale.to_bits()
    }

    /// Does the kept picture cover everything the glass of a window at `win` can sample (its own rect,
    /// clipped to the monitor), at this scale? A window that grew, moved out of it or changed DPI needs
    /// a fresh capture.
    pub fn covers(&self, win: (i32, i32, i32, i32), monitor: (i32, i32, i32, i32), scale: f32) -> bool {
        let Some(r) = self.region else { return false };
        if self.scale_bits != scale.to_bits() {
            return false;
        }
        let need = (win.0.max(monitor.0), win.1.max(monitor.1), win.2.min(monitor.2), win.3.min(monitor.3));
        need.0 >= r.0 && need.1 >= r.1 && need.2 <= r.2 && need.3 <= r.3
    }

    /// The window hid: the worker drops its capture buffer. The small frosted picture is kept, so the
    /// next show has glass to paint at once (it is refreshed within a frame or two).
    pub fn release(&mut self) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(Msg::Release);
        }
        self.dirty = true;
        self.last_ms = f64::NEG_INFINITY;
    }
}

// --- screen events: refresh a resting pill's glass without a timer --------------------------------

use std::cell::RefCell;
use windows::Win32::UI::Accessibility::{HWINEVENTHOOK, SetWinEventHook, UnhookWinEvent};
use windows::Win32::UI::WindowsAndMessaging::{
    EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_MINIMIZEEND, EVENT_SYSTEM_MINIMIZESTART, EVENT_SYSTEM_MOVESIZEEND, OBJID_WINDOW, WINEVENT_OUTOFCONTEXT,
    WINEVENT_SKIPOWNPROCESS,
};

/// Posted to the pill window when another app's top-level window changed in a way that can change
/// what is behind the resting pill (see [`ScreenEvents`]).
pub const WM_APP_SCREEN_EVENT: u32 = WM_APP + 0x52;

/// The system events that usually change the screen under a resting pill: another window came to the
/// front, a move / resize of a window ended, a window was minimised or restored. All are rare (human
/// pace). `EVENT_OBJECT_LOCATIONCHANGE` / `SHOW` / `HIDE` are deliberately left out: they fire for
/// carets, cursors and tooltips many times a second and would turn the hook into polling.
const SCREEN_EVENTS: [(u32, u32); 3] = [
    (EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_FOREGROUND),
    (EVENT_SYSTEM_MOVESIZEEND, EVENT_SYSTEM_MOVESIZEEND),
    (EVENT_SYSTEM_MINIMIZESTART, EVENT_SYSTEM_MINIMIZEEND),
];

thread_local! {
    /// Which pill window each hook of this thread reports to (hook handle, window handle).
    static SCREEN_SINKS: RefCell<Vec<(isize, isize)>> = const { RefCell::new(Vec::new()) };
}

/// Out-of-context hook callback: runs on the thread that installed the hook while it retrieves
/// messages, so it only posts (the pill's state may be borrowed further up the stack).
unsafe extern "system" fn on_screen_event(hook: HWINEVENTHOOK, _event: u32, _hwnd: HWND, id_object: i32, _id_child: i32, _thread: u32, _time: u32) {
    if id_object != OBJID_WINDOW.0 {
        return;
    }
    let target = SCREEN_SINKS.with(|s| s.borrow().iter().find(|(h, _)| *h == hook.0 as isize).map(|(_, w)| *w));
    if let Some(w) = target {
        // SAFETY: posting to a window handle is always safe; a stale handle just fails.
        unsafe {
            let _ = PostMessageW(Some(HWND(w as *mut core::ffi::c_void)), WM_APP_SCREEN_EVENT, WPARAM(0), LPARAM(0));
        }
    }
}

/// Out-of-context WinEvent hooks (`WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS`, no DLL is
/// injected anywhere) that tell the pill window when other apps' windows were activated, moved,
/// resized, minimised or restored, so the resting circle can refresh its backdrop on events instead
/// of a timer (PLAN section 1: an idle daemon runs no timers). Installed while the window is shown,
/// removed (`UnhookWinEvent`) when it hides or is destroyed. Installed on the pill's UI thread.
pub struct ScreenEvents {
    hooks: Vec<HWINEVENTHOOK>,
}

impl ScreenEvents {
    /// `None` when no hook could be installed (the pill then refreshes on its own events only).
    pub fn watch(hwnd: HWND) -> Option<ScreenEvents> {
        let mut hooks = Vec::new();
        for (lo, hi) in SCREEN_EVENTS {
            // SAFETY: an out-of-context hook with a static callback; removed in Drop on this thread.
            let h = unsafe { SetWinEventHook(lo, hi, None, Some(on_screen_event), 0, 0, WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS) };
            if !h.is_invalid() {
                SCREEN_SINKS.with(|s| s.borrow_mut().push((h.0 as isize, hwnd.0 as isize)));
                hooks.push(h);
            }
        }
        (!hooks.is_empty()).then_some(ScreenEvents { hooks })
    }
}

impl Drop for ScreenEvents {
    fn drop(&mut self) {
        for h in self.hooks.drain(..) {
            SCREEN_SINKS.with(|s| s.borrow_mut().retain(|(k, _)| *k != h.0 as isize));
            // SAFETY: a hook this thread installed and has not removed yet.
            unsafe {
                let _ = UnhookWinEvent(h);
            }
        }
    }
}

impl Drop for GlassHost {
    fn drop(&mut self) {
        self.alive.store(false, Ordering::Release);
        if let Some(tx) = self.tx.take() {
            let _ = tx.send(Msg::Quit);
        }
        if let Some(j) = self.worker.take() {
            let _ = j.join();
        }
    }
}

#[cfg(test)]
impl GlassHost {
    /// A host that cannot capture (affinity refused): the fallback path.
    pub fn unavailable() -> GlassHost {
        GlassHost::with_exclusion(0, false)
    }

    /// Keep a picture as if it had been captured from `region` (placement tests).
    pub fn keep(&mut self, frosted: Frosted, region: (i32, i32, i32, i32), scale: f32) {
        self.frosted = Some(Arc::new(frosted));
        self.region = Some(region);
        self.scale_bits = scale.to_bits();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placement_and_coverage_of_the_kept_picture() {
        let mon = (0, 0, 1920, 1080);
        let mut h = GlassHost::unavailable();
        assert!(!h.available());
        assert!(h.backdrop((0, 0), (1920.0, 1080.0)).is_none(), "nothing captured yet");
        assert!(!h.covers((0, 0, 10, 10), mon, 1.0));
        h.keep(Frosted::flat([1, 2, 3]), (100, 50, 500, 350), 1.0);
        // the buffer starts 50 px left of and 30 px above a window at (150, 80)
        let b = h.backdrop((150, 80), (1920.0, 1080.0)).expect("backdrop");
        assert_eq!((b.ox, b.oy), (-50, -30));
        // moving the window moves the picture's placement but keeps its pixels
        let moved = h.backdrop((120, 60), (1920.0, 1080.0)).expect("backdrop");
        assert_eq!((moved.ox, moved.oy), (-20, -10));
        assert!(Arc::ptr_eq(&b.frosted, &moved.frosted));
        assert_ne!(b.id, moved.id);
        // the window must lie inside the captured region to be covered
        assert!(h.covers((150, 80, 358, 200), mon, 1.0));
        assert!(!h.covers((90, 80, 300, 200), mon, 1.0), "sticks out to the left");
        assert!(!h.covers((150, 80, 358, 400), mon, 1.0), "sticks out at the bottom");
        // a window hanging off the monitor only needs its visible part
        h.keep(Frosted::flat([1, 2, 3]), (1700, 900, 1920, 1080), 1.0);
        assert!(h.covers((1708, 956, 2000, 1100), mon, 1.0));
        // hiding keeps the small picture for the next show and asks for a fresh capture
        h.release();
        assert!(h.dirty && h.backdrop((1708, 956), (1.0, 1.0)).is_some());
    }

    #[test]
    fn the_capture_switch_reads_the_environment_value() {
        assert!(!capture_disabled_from(None));
        assert!(!capture_disabled_from(Some("".into())));
        assert!(!capture_disabled_from(Some("0".into())));
        assert!(capture_disabled_from(Some("1".into())));
        assert!(capture_disabled_from(Some("true".into())));
    }

    #[test]
    fn an_unavailable_host_never_requests_or_reports() {
        let mut h = GlassHost::unavailable();
        for _ in 0..3 {
            assert!(!h.request((100, 100, 300, 220), (0, 0, 1920, 1080), 1.0, (1920.0, 1080.0), 5.0, true));
        }
        assert!(!h.in_flight && h.take().is_none());
        assert_eq!((h.grabs, h.unchanged), (0, 0));
        assert!(h.backdrop((100, 100), (1920.0, 1080.0)).is_none());
    }

    /// CPU time of the calling thread so far, in ms (kernel + user), or `None` when unavailable.
    fn thread_cpu_ms() -> Option<f64> {
        use windows::Win32::Foundation::FILETIME;
        use windows::Win32::System::Threading::{GetCurrentThread, GetThreadTimes};
        let (mut c, mut e, mut k, mut u) = (FILETIME::default(), FILETIME::default(), FILETIME::default(), FILETIME::default());
        // SAFETY: valid pseudo handle and out-pointers.
        unsafe { GetThreadTimes(GetCurrentThread(), &mut c, &mut e, &mut k, &mut u) }.ok()?;
        let t = |f: FILETIME| ((f.dwHighDateTime as u64) << 32 | f.dwLowDateTime as u64) as f64 / 10_000.0;
        Some(t(k) + t(u))
    }

    fn median_ms(mut v: Vec<f64>) -> f64 {
        v.sort_by(|a, b| a.total_cmp(b));
        v[v.len() / 2]
    }

    /// A busy synthetic screen: text-like stripes, noise and flat blocks (BGRA).
    fn busy_bgra(w: usize, h: usize) -> Vec<u8> {
        (0..w * h)
            .flat_map(|i| {
                let (x, y) = (i % w, i / w);
                let v = if (x / 3 + y / 5) % 2 == 0 { 20 } else { 235 };
                let n = ((x * 31 + y * 17) ^ (x * y)) as u8;
                [v ^ (n & 15), v, v.wrapping_add(n & 31), 255]
            })
            .collect()
    }

    /// Bytes of the buffers one capture of the pill window touches, at a scale and monitor size: the
    /// DIB the screen is copied into plus the cell/frost working set and the kept picture.
    fn capture_bytes(scale: f32, mon: (i32, i32)) -> (usize, usize) {
        let win = (0, 0, (208.0 * scale).ceil() as i32, (120.0 * scale).ceil() as i32);
        let margin = (glass::margin_logical((mon.0 as f32 / scale, mon.1 as f32 / scale)) * scale).ceil() as i32;
        let (l, t, r, b) = glass::capture_region(win, margin, (-100_000, -100_000, 100_000, 100_000)).unwrap();
        let (w, h) = ((r - l) as usize, (b - t) as usize);
        let cell = glass::cell_for(scale) as usize;
        let cells = w.div_ceil(cell) * h.div_ceil(cell);
        // cells (u8 x3) + 3 f32 planes + the f32 scratch + the frosted copy (u8 x3)
        (w * h * 4, cells * (3 + 3 * 4 + 4 + 3))
    }

    #[test]
    fn capture_buffers_are_small_and_bounded() {
        // 1x, 1.5x, 2x on 1080p, 1440p, 4K; the DIB is the dominant buffer and exists only while shown
        for (scale, mon) in [(1.0, (1920, 1080)), (1.5, (2560, 1440)), (2.0, (3840, 2160)), (3.0, (7680, 4320))] {
            let (dib, work) = capture_bytes(scale, mon);
            // the margin is capped at 96 logical px, so the DIB is at most (208 + 192) x (120 + 192) logical px
            let cap = ((208.0 + 192.0) * scale).ceil() as usize * ((120.0 + 192.0) * scale).ceil() as usize * 4;
            assert!(dib <= cap + 4096, "{scale}x: DIB {dib} > {cap}");
            assert!(dib <= 12 << 20, "{scale}x on {mon:?}: DIB {dib} bytes");
            assert!(work <= 2 << 20, "{scale}x: frost working set {work} bytes");
            println!("capture buffers {scale}x on {}x{}: DIB {:.2} MB, frost working set {:.2} MB", mon.0, mon.1, dib as f64 / 1e6, work as f64 / 1e6);
        }
        // the kept picture (what stays resident while shown or hidden) is the cell grid only
        let frosted = Cells::from_bgra(&busy_bgra(400, 312), 400, 312, 3).unwrap().frost(glass::FROST_SIGMA, 1.0);
        assert!(frosted.w * frosted.h * 3 < 64 << 10, "kept picture {} bytes", frosted.w * frosted.h * 3);
        // Capturer::grab refuses absurd sizes instead of allocating them
        let mut c = Capturer::new().expect("memory DC");
        assert!(c.grab(0, 0, 20_000, 20_000).is_none() && c.grab(0, 0, 0, 10).is_none() && c.grab(0, 0, -5, 10).is_none());
    }

    /// Cost of one capture: the screen copy, the cell average + hash, and (when the hash changed) the
    /// frost, at 1x and 2x. Prints the numbers (`cargo test -- --nocapture capture_cost`); asserts only
    /// a very loose ceiling so a slow or busy machine cannot make it flaky.
    #[test]
    fn capture_cost_is_measured_and_bounded() {
        for scale in [1.0f32, 2.0] {
            let (w, h) = (((208.0 + 192.0) * scale) as usize, ((120.0 + 192.0) * scale) as usize);
            let raw = busy_bgra(w, h);
            let cell = glass::cell_for(scale);
            let (mut cells_ms, mut frost_ms, mut cpu) = (Vec::new(), Vec::new(), Vec::new());
            for _ in 0..21 {
                let c0 = thread_cpu_ms();
                let t0 = std::time::Instant::now();
                let cells = Cells::from_bgra(&raw, w, h, cell).unwrap();
                cells_ms.push(t0.elapsed().as_secs_f64() * 1000.0);
                let t1 = std::time::Instant::now();
                let f = cells.frost(glass::FROST_SIGMA, scale);
                frost_ms.push(t1.elapsed().as_secs_f64() * 1000.0);
                assert!(f.w * f.h > 0);
                if let (Some(a), Some(b)) = (c0, thread_cpu_ms()) {
                    cpu.push(b - a);
                }
            }
            let (c, f) = (median_ms(cells_ms), median_ms(frost_ms));
            println!("capture cost {scale}x ({w}x{h} px): cells+hash {c:.3} ms, frost {f:.3} ms, thread CPU (both) {:.3} ms", if cpu.is_empty() { f64::NAN } else { median_ms(cpu) });
            // the fastest capture cadence is 125 ms: a result slower than that (an unoptimised Wine build included) is a bug
            assert!(c + f < 120.0, "{scale}x: {c:.1} + {f:.1} ms");
        }
        // the real screen copy, when this machine has a screen to copy
        let Some(mut cap) = Capturer::new() else { return };
        for scale in [1.0f32, 2.0] {
            let (w, h) = (((208.0 + 192.0) * scale) as i32, ((120.0 + 192.0) * scale) as i32);
            if cap.grab(0, 0, 4, 4).is_none() {
                println!("SKIPPED: the screen cannot be copied here");
                return;
            }
            let (mut wall, mut cpu) = (Vec::new(), Vec::new());
            for _ in 0..11 {
                let c0 = thread_cpu_ms();
                let t0 = std::time::Instant::now();
                assert!(cap.grab(0, 0, w, h).is_some());
                wall.push(t0.elapsed().as_secs_f64() * 1000.0);
                if let (Some(a), Some(b)) = (c0, thread_cpu_ms()) {
                    cpu.push(b - a);
                }
            }
            println!("BitBlt {scale}x ({w}x{h} px): wall {:.3} ms, thread CPU {:.3} ms", median_ms(wall), if cpu.is_empty() { f64::NAN } else { median_ms(cpu) });
        }
    }

    #[test]
    fn the_worker_answers_off_thread_and_gates_on_the_hash() {
        // a host that is allowed to capture (no window: posts go nowhere) and a request for real pixels
        let mut h = GlassHost::with_exclusion(0, true);
        let (win, mon) = ((300, 300, 508, 420), (0, 0, 1920, 1080));
        let page = (1920.0, 1080.0);
        if Capturer::new().and_then(|mut c| c.grab(0, 0, 4, 4).map(|s| s.len())).is_none() {
            println!("SKIPPED: the screen cannot be copied here");
            return;
        }
        assert!(h.request(win, mon, 1.0, page, 1.0, true));
        assert!(h.in_flight && !h.request(win, mon, 1.0, page, 2.0, true), "one request in flight at a time");
        let wait = |h: &mut GlassHost| {
            let t0 = std::time::Instant::now();
            loop {
                if let Some(r) = h.take() {
                    return r;
                }
                assert!(t0.elapsed().as_secs() < 10, "the worker never answered");
                std::thread::yield_now();
            }
        };
        assert_eq!(wait(&mut h), Refresh::Changed);
        assert!(h.backdrop((win.0, win.1), page).is_some() && h.covers(win, mon, 1.0));
        // the same screen again: the hash says nothing changed (the desktop may tick between two
        // grabs, so accept a change but not an unavailable)
        assert!(h.request(win, mon, 1.0, page, 200.0, false));
        let again = wait(&mut h);
        assert!(matches!(again, Refresh::Unchanged | Refresh::Changed), "{again:?}");
        assert_eq!(h.grabs, 2);
        // a different scale is a different picture
        assert!(h.request(win, mon, 1.5, page, 400.0, true));
        assert_eq!(wait(&mut h), Refresh::Changed);
        drop(h); // joins the worker
    }
}
