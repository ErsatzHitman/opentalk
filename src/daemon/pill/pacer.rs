//! Frame pacing for the pill animation: vsync-aligned ticks and presentation-time prediction.
//!
//! A free-running ~16 ms timer beats against the display's refresh (60 / 75 / 120 / 144 Hz): some
//! frames are shown twice and some never, which reads as judder. At full rate the animation thread
//! therefore waits for the compositor (`DwmFlush`, blocking and zero CPU) and tells the window the
//! time at which the frame it is about to draw will be *shown* (the next vertical blank), so the
//! animation state is sampled at presentation time, not at message-handling time. A late or
//! coalesced tick never slows the animation: the state is a pure function of that time.
//!
//! Everything that decides anything is a pure function here (unit-tested); only [`Vsync`] touches
//! Win32, and it degrades to the fixed-rate timer on a machine where the compositor does not block.

use std::sync::atomic::{AtomicU64, Ordering};

/// Highest tick rate the pill asks for. A 144 Hz display is ticked at every second vblank (72 fps),
/// a 240 Hz one at every fourth: the pill is small, 72 fps is as fluid as 144 to the eye, and the
/// recording CPU budget (PLAN section 1) stays flat on fast panels.
pub const MAX_FPS: f64 = 75.0;
/// Refresh rates up to this tick at every vblank.
const EVERY_VBLANK_UP_TO_HZ: f64 = 100.0;

/// How many vblanks pass between two ticks on a display refreshing at `hz`.
pub fn vsync_divisor(hz: f64) -> u32 {
    if !hz.is_finite() || hz <= EVERY_VBLANK_UP_TO_HZ {
        1
    } else {
        (hz / MAX_FPS).ceil().max(1.0) as u32
    }
}

/// Milliseconds from `now` to the next vertical blank, given any past or future vblank instant
/// (`vblank`, same clock) and the refresh `period`.
pub fn ms_to_next_vblank(now: f64, vblank: f64, period: f64) -> f64 {
    if !(period.is_finite() && period > 0.0 && now.is_finite() && vblank.is_finite()) {
        return 0.0;
    }
    let phase = (now - vblank).rem_euclid(period);
    if phase <= 1e-9 { 0.0 } else { period - phase }
}

/// Time at which a frame handled at `now` is shown, given the vblank the animation thread predicted
/// for it (`hint`, `period` apart): the first vblank at or after `now`. A tick that is handled late
/// slides to the vblank it can still make, so the picture is never sampled in the past.
/// `hint` NaN (no timing information) means "now".
pub fn present_time(hint: f64, period: f64, now: f64) -> f64 {
    if !hint.is_finite() || !(period.is_finite() && period > 0.0) {
        return now;
    }
    if hint >= now {
        // never promise a time more than a few frames ahead (a stale hint after a pause)
        return hint.min(now + 4.0 * period);
    }
    let late = ((now - hint) / period).ceil();
    (hint + late * period).min(now + 4.0 * period)
}

/// Detects a `DwmFlush` that does not pace: one that does not block (remote session, DWM off, a
/// wrapper layer: under [`Guard::MIN_BLOCK_MS`] four times in a row), or one that blocks far longer than
/// any refresh ([`Guard::MAX_BLOCK_MS`]: a compositor that idles when nothing changes).
#[derive(Clone, Copy, Debug, Default)]
pub struct Guard {
    fast: u32,
}

impl Guard {
    pub const MIN_BLOCK_MS: f64 = 1.0;
    pub const MAX_BLOCK_MS: f64 = 100.0;
    const STREAK: u32 = 4;

    /// Record how long one flush blocked; `false` once the flush cannot be trusted any more.
    pub fn observe(&mut self, blocked_ms: f64, ok: bool) -> bool {
        if blocked_ms > Self::MAX_BLOCK_MS {
            self.fast = Self::STREAK;
            return false;
        }
        if ok && blocked_ms >= Self::MIN_BLOCK_MS {
            self.fast = 0;
        } else {
            self.fast += 1;
        }
        self.fast < Self::STREAK
    }
}

/// The animation thread's forecast for the frame it just ticked, read by the window.
pub struct Hint {
    time: AtomicU64,
    period: AtomicU64,
}

impl Hint {
    pub const fn new() -> Hint {
        Hint { time: AtomicU64::new(f64::NAN.to_bits()), period: AtomicU64::new(0) }
    }

    /// Forecast: the next frame is shown at `time` (window clock, ms), vblanks `period` ms apart.
    pub fn set(&self, time: f64, period: f64) {
        self.period.store(period.to_bits(), Ordering::Relaxed);
        self.time.store(time.to_bits(), Ordering::Release);
    }

    pub fn clear(&self) {
        self.time.store(f64::NAN.to_bits(), Ordering::Release);
    }

    /// Time to sample the animation at for a frame drawn at `now`.
    pub fn present_time(&self, now: f64) -> f64 {
        let time = f64::from_bits(self.time.load(Ordering::Acquire));
        present_time(time, f64::from_bits(self.period.load(Ordering::Relaxed)), now)
    }
}

impl Default for Hint {
    fn default() -> Self {
        Hint::new()
    }
}

// --- Win32 -------------------------------------------------------------------------------------

use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Dwm::{DWM_TIMING_INFO, DwmFlush, DwmGetCompositionTimingInfo};
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};

/// The compositor's clock: blocks for vblanks and says when the next one is.
pub struct Vsync {
    guard: Guard,
    freq: f64,
    /// Vblanks per tick, from the measured refresh rate.
    divisor: u32,
    period_ms: f64,
}

impl Vsync {
    /// `None` when there is no usable compositor clock (the caller keeps its fixed-rate timer).
    pub fn new() -> Option<Vsync> {
        let mut f = 0i64;
        // SAFETY: valid out-pointer.
        unsafe { QueryPerformanceFrequency(&mut f) }.ok()?;
        if f <= 0 {
            return None;
        }
        let mut v = Vsync { guard: Guard::default(), freq: f as f64, divisor: 1, period_ms: 0.0 };
        let (_, period) = v.timing()?;
        v.period_ms = period;
        v.divisor = vsync_divisor(1000.0 / period);
        Some(v)
    }

    /// `(ms until the next vblank, refresh period in ms)`, or `None` when DWM cannot say.
    fn timing(&self) -> Option<(f64, f64)> {
        let mut ti = DWM_TIMING_INFO { cbSize: std::mem::size_of::<DWM_TIMING_INFO>() as u32, ..Default::default() };
        let mut now = 0i64;
        // SAFETY: valid out-pointers; a null window asks for the system-wide composition timing.
        unsafe {
            DwmGetCompositionTimingInfo(HWND::default(), &mut ti).ok()?;
            QueryPerformanceCounter(&mut now).ok()?;
        }
        let period = if ti.qpcRefreshPeriod > 0 {
            ti.qpcRefreshPeriod as f64 / self.freq * 1000.0
        } else if ti.rateRefresh.uiNumerator > 0 && ti.rateRefresh.uiDenominator > 0 {
            ti.rateRefresh.uiDenominator as f64 / ti.rateRefresh.uiNumerator as f64 * 1000.0
        } else {
            return None;
        };
        if !(2.0..=50.0).contains(&period) {
            return None;
        }
        let now_ms = now as f64 / self.freq * 1000.0;
        let vblank_ms = ti.qpcVBlank as f64 / self.freq * 1000.0;
        Some((ms_to_next_vblank(now_ms, vblank_ms, period), period))
    }

    /// Block until the next tick's vblank. Returns the forecast `(ms from now until the frame about
    /// to be drawn is shown, period)`, or `None` once the compositor stopped pacing (then the caller
    /// falls back to its timer).
    pub fn wait(&mut self) -> Option<(f64, f64)> {
        let t0 = std::time::Instant::now();
        let mut ok = true;
        for _ in 0..self.divisor {
            // SAFETY: plain call, no arguments.
            ok &= unsafe { DwmFlush() }.is_ok();
        }
        let blocked = t0.elapsed().as_secs_f64() * 1000.0;
        // each flush of a divisor run is judged on its own share
        if !self.guard.observe(blocked / self.divisor as f64, ok) {
            return None;
        }
        // the frame drawn now goes out at the first vblank to come
        let (to_next, period) = self.timing().unwrap_or((0.0, self.period_ms));
        self.period_ms = period;
        // a display mode change (another refresh rate) is picked up by the next wait
        self.divisor = vsync_divisor(1000.0 / period);
        Some((to_next, period))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn divisor_keeps_the_tick_rate_near_60_to_75() {
        let fps = |hz: f64| hz / vsync_divisor(hz) as f64;
        for (hz, want) in [(50.0, 1), (60.0, 1), (75.0, 1), (90.0, 1), (100.0, 1), (120.0, 2), (144.0, 2), (165.0, 3), (240.0, 4), (360.0, 5)] {
            assert_eq!(vsync_divisor(hz), want, "{hz} Hz");
            if hz >= 60.0 {
                assert!((50.0..=100.0).contains(&fps(hz)), "{hz} Hz -> {} fps", fps(hz));
            }
        }
        assert_eq!(vsync_divisor(f64::NAN), 1);
        assert_eq!(vsync_divisor(0.0), 1);
    }

    #[test]
    fn next_vblank_is_found_from_any_reference_instant() {
        let p = 1000.0 / 60.0;
        // reference in the past, in the future, exactly now
        assert!((ms_to_next_vblank(100.0, 100.0 - 10.0 * p - 4.0, p) - (p - 4.0)).abs() < 1e-9);
        assert!((ms_to_next_vblank(100.0, 100.0 + 3.0 * p + 4.0, p) - 4.0).abs() < 1e-9);
        assert_eq!(ms_to_next_vblank(100.0, 100.0, p), 0.0);
        // garbage in, "now" out
        assert_eq!(ms_to_next_vblank(100.0, f64::NAN, p), 0.0);
        assert_eq!(ms_to_next_vblank(100.0, 90.0, 0.0), 0.0);
    }

    #[test]
    fn present_time_never_samples_the_past_and_never_runs_far_ahead() {
        let p = 1000.0 / 144.0;
        assert_eq!(present_time(f64::NAN, p, 50.0), 50.0, "no forecast: now");
        assert_eq!(present_time(60.0, 0.0, 50.0), 50.0, "no period: now");
        assert_eq!(present_time(60.0, p, 50.0), 60.0, "on time: the forecast");
        // handled 2.5 frames late: slides to the first vblank still to come
        let t = present_time(60.0, p, 60.0 + 2.5 * p);
        assert!((t - (60.0 + 3.0 * p)).abs() < 1e-9, "{t}");
        assert!(t >= 60.0 + 2.5 * p);
        // a forecast from long ago (the thread paused) is capped
        assert!(present_time(60.0, p, 5000.0) <= 5000.0 + 4.0 * p + 1e-9);
        assert!(present_time(1.0e9, p, 50.0) <= 50.0 + 4.0 * p + 1e-9);
    }

    #[test]
    fn present_time_is_monotonic_for_an_even_vblank_train() {
        // frames handled a little after each vblank map to consecutive vblanks: even steps
        let p = 1000.0 / 60.0;
        let mut prev = f64::NEG_INFINITY;
        for k in 0..600 {
            let vb = 1000.0 + k as f64 * p;
            let handled = vb - p + 0.7 + (k % 5) as f64 * 0.4; // jittery handling, always before the vblank
            let t = present_time(vb, p, handled);
            assert!((t - vb).abs() < 1e-9, "frame {k}");
            assert!(t > prev);
            prev = t;
        }
    }

    #[test]
    fn guard_distrusts_a_flush_that_does_not_block() {
        let mut g = Guard::default();
        assert!(g.observe(16.0, true));
        assert!(g.observe(0.1, true));
        assert!(g.observe(0.1, true));
        assert!(g.observe(16.0, true), "a good flush resets the streak");
        for _ in 0..3 {
            assert!(g.observe(0.0, true));
        }
        assert!(!g.observe(0.0, true), "four instant flushes in a row");
        let mut g = Guard::default();
        for _ in 0..3 {
            assert!(g.observe(16.0, false));
        }
        assert!(!g.observe(16.0, false), "four failures in a row");
        let mut g = Guard::default();
        assert!(g.observe(16.0, true));
        assert!(!g.observe(400.0, true), "a flush that waits for ever is no vsync");
    }

    #[test]
    fn hint_round_trips_between_threads() {
        let h = Hint::new();
        assert_eq!(h.present_time(42.0), 42.0, "empty hint: now");
        h.set(60.0, 16.0);
        assert_eq!(h.present_time(50.0), 60.0);
        h.clear();
        assert_eq!(h.present_time(50.0), 50.0);
    }
}
