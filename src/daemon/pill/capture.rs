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

// --- the host: capture, hash, frost ------------------------------------------------------------

use super::glass::{self, Backdrop, Cells, Frosted};
use std::sync::Arc;

/// What a [`GlassHost::refresh`] found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refresh {
    /// Capture is not possible (window not excluded, grab failed): paint over the neutral backdrop.
    Unavailable,
    /// Same picture in the same place: nothing to recompose.
    Unchanged,
    /// New picture or new placement: install [`GlassHost::backdrop`].
    Changed,
}

/// Owns the screen capture of one pill window. Costs nothing until [`GlassHost::refresh`] is called,
/// and [`GlassHost::release`] returns its buffers (the pill calls it when the window hides).
pub struct GlassHost {
    capturer: Option<Capturer>,
    /// The window is excluded from capture, so grabbing the screen cannot see the pill itself.
    excluded: bool,
    cells_id: u64,
    frosted: Option<Arc<Frosted>>,
    /// Screen rect (device px) the frosted buffer was captured from, and the scale it was built at.
    region: Option<(i32, i32, i32, i32)>,
    scale_bits: u32,
    /// Time of the last grab (ms, the window clock).
    pub last_ms: f64,
    /// Something changed that may have altered what is under the window; capture when allowed.
    pub dirty: bool,
    /// Grabs, and how many of them found the picture unchanged (diagnostics and tests).
    pub grabs: u64,
    pub unchanged: u64,
}

impl GlassHost {
    /// Exclude `hwnd` from capture and prepare the capturer. When Windows refuses (before 10 2004) the
    /// host is permanently unavailable and the pill keeps its neutral frosted glass.
    pub fn new(hwnd: HWND) -> GlassHost {
        let excluded = exclude_from_capture(hwnd);
        let capturer = if excluded { Capturer::new() } else { None };
        GlassHost {
            capturer,
            excluded,
            cells_id: 0,
            frosted: None,
            region: None,
            scale_bits: 0,
            last_ms: f64::NEG_INFINITY,
            dirty: true,
            grabs: 0,
            unchanged: 0,
        }
    }

    /// Can this host capture at all?
    pub fn available(&self) -> bool {
        self.excluded && self.capturer.is_some()
    }

    /// Grab the screen under the window (`win`, device px, screen coordinates) on `monitor`, frost it
    /// and keep it. `page` is the monitor in logical px (the field's page size).
    pub fn refresh(&mut self, win: (i32, i32, i32, i32), monitor: (i32, i32, i32, i32), scale: f32, page: (f32, f32), now: f64) -> Refresh {
        self.last_ms = now;
        self.dirty = false;
        let Some(cap) = self.capturer.as_mut().filter(|_| self.excluded) else { return Refresh::Unavailable };
        let margin = (glass::margin_logical(page) * scale).ceil() as i32;
        let Some(region) = glass::capture_region(win, margin, monitor) else {
            self.frosted = None;
            self.region = None;
            return Refresh::Unavailable;
        };
        let (w, h) = (region.2 - region.0, region.3 - region.1);
        let Some(raw) = cap.grab(region.0, region.1, w, h) else {
            self.frosted = None;
            self.region = None;
            return Refresh::Unavailable;
        };
        self.grabs += 1;
        let Some(cells) = Cells::from_bgra(raw, w as usize, h as usize, glass::cell_for(scale)) else { return Refresh::Unavailable };
        if self.frosted.is_some() && cells.id == self.cells_id && self.region == Some(region) && self.scale_bits == scale.to_bits() {
            self.unchanged += 1;
            return Refresh::Unchanged;
        }
        self.cells_id = cells.id;
        self.frosted = Some(Arc::new(cells.frost(glass::FROST_SIGMA, scale)));
        self.region = Some(region);
        self.scale_bits = scale.to_bits();
        Refresh::Changed
    }

    /// The last frosted picture placed for a window whose top-left is at `origin` (screen device px).
    pub fn backdrop(&self, origin: (i32, i32), page: (f32, f32)) -> Option<Arc<Backdrop>> {
        let (f, r) = (self.frosted.as_ref()?, self.region?);
        Some(Arc::new(Backdrop::new(f.clone(), r.0 - origin.0, r.1 - origin.1, page)))
    }

    /// Does the kept picture cover everything the glass of a window at `win` can sample (its own rect,
    /// clipped to the monitor)? A window that grew or moved out of it needs a fresh capture.
    pub fn covers(&self, win: (i32, i32, i32, i32), monitor: (i32, i32, i32, i32)) -> bool {
        let Some(r) = self.region else { return false };
        let need = (win.0.max(monitor.0), win.1.max(monitor.1), win.2.min(monitor.2), win.3.min(monitor.3));
        need.0 >= r.0 && need.1 >= r.1 && need.2 <= r.2 && need.3 <= r.3
    }

    /// The window hid: drop the picture and the capture buffer (nothing is captured while hidden).
    pub fn release(&mut self) {
        self.frosted = None;
        self.region = None;
        self.cells_id = 0;
        if self.excluded {
            self.capturer = Capturer::new();
        }
        self.dirty = true;
        self.last_ms = f64::NEG_INFINITY;
    }
}
