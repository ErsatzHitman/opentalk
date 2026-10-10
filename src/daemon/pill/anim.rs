//! Animation primitives: CSS cubic-bezier easing, retargetable tracks (CSS-transition semantics),
//! the waveform envelope / delay line and the looping keyframe curves (pulse, signal spin/breathe).
//!
//! Everything here is a pure function of time so `render(state, t)` is deterministic.

/// CSS `cubic-bezier(x1, y1, x2, y2)`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Bezier {
    x1: f32,
    y1: f32,
    x2: f32,
    y2: f32,
}

impl Bezier {
    pub const fn new(x1: f32, y1: f32, x2: f32, y2: f32) -> Self {
        Bezier { x1, y1, x2, y2 }
    }

    #[inline]
    fn sample(a: f32, b: f32, t: f32) -> f32 {
        // 3(1-t)^2 t a + 3(1-t) t^2 b + t^3
        let it = 1.0 - t;
        3.0 * it * it * t * a + 3.0 * it * t * t * b + t * t * t
    }

    #[inline]
    fn slope(a: f32, b: f32, t: f32) -> f32 {
        let it = 1.0 - t;
        3.0 * it * it * a + 6.0 * it * t * (b - a) + 3.0 * t * t * (1.0 - b)
    }

    /// Progress `x` in 0..1 -> eased value (Newton, bisection fallback).
    pub fn ease(&self, x: f32) -> f32 {
        if x <= 0.0 {
            return 0.0;
        }
        if x >= 1.0 {
            return 1.0;
        }
        if self.x1 == self.y1 && self.x2 == self.y2 {
            return x;
        }
        let mut t = x;
        for _ in 0..6 {
            let err = Self::sample(self.x1, self.x2, t) - x;
            if err.abs() < 1e-6 {
                return Self::sample(self.y1, self.y2, t);
            }
            let d = Self::slope(self.x1, self.x2, t);
            if d.abs() < 1e-6 {
                break;
            }
            t -= err / d;
        }
        let (mut lo, mut hi) = (0.0f32, 1.0f32);
        t = x;
        for _ in 0..24 {
            let v = Self::sample(self.x1, self.x2, t);
            if (v - x).abs() < 1e-6 {
                break;
            }
            if v < x {
                lo = t;
            } else {
                hi = t;
            }
            t = (lo + hi) * 0.5;
        }
        Self::sample(self.y1, self.y2, t)
    }
}

/// The house motion curve `cubic-bezier(0.2, 0, 0, 1)` (VOICE_PILL_GROW_EASING).
pub const HOUSE: Bezier = Bezier::new(0.2, 0.0, 0.0, 1.0);
/// CSS keyword `ease-out` (inline `transition: ... ease-out`).
pub const CSS_EASE_OUT: Bezier = Bezier::new(0.0, 0.0, 0.58, 1.0);
/// Tailwind `ease-out` utility.
pub const TW_EASE_OUT: Bezier = Bezier::new(0.0, 0.0, 0.2, 1.0);
/// CSS keyword `ease-in-out` (signal breathe).
pub const EASE_IN_OUT: Bezier = Bezier::new(0.42, 0.0, 0.58, 1.0);
/// Tailwind `animate-pulse` timing function.
pub const PULSE_EASE: Bezier = Bezier::new(0.4, 0.0, 0.6, 1.0);
#[cfg(test)]
pub const LINEAR: Bezier = Bezier::new(0.0, 0.0, 1.0, 1.0);

pub const EXPANSION_MS: f64 = 300.0;
/// Hold before the control changes shape (LISTENING_ENTRANCE_TIMING.thinkingMs).
pub const ENTRANCE_HOLD_MS: f64 = 260.0;
/// Waveform reveal: hold + expansion + 100 ms.
pub const WAVEFORM_AT_MS: f64 = 660.0;
pub const WAVEFORM_FADE_MS: f64 = 200.0;
pub const CHROME_MS: f64 = 220.0;
pub const CANCEL_EMERGE_MS: f64 = 280.0;

#[derive(Clone, Copy, Debug)]
struct Seg {
    start: f64,
    dur: f64,
    from: f32,
    to: f32,
    ease: Bezier,
}

/// A scalar that follows CSS-transition semantics: retargeting starts a new transition from the
/// current value. Segments may be scheduled in the future; scheduling earlier than a pending segment
/// drops it.
#[derive(Clone, Debug)]
pub struct Track {
    init: f32,
    segs: Vec<Seg>,
}

impl Track {
    pub fn new(v: f32) -> Self {
        Track { init: v, segs: Vec::with_capacity(2) }
    }

    pub fn value(&self, t: f64) -> f32 {
        let mut v = self.init;
        for s in &self.segs {
            if s.start > t {
                break;
            }
            v = if s.dur <= 0.0 {
                s.to
            } else {
                let p = ((t - s.start) / s.dur).clamp(0.0, 1.0) as f32;
                s.from + (s.to - s.from) * s.ease.ease(p)
            };
        }
        v
    }

    /// Final value once every segment finished.
    pub fn target(&self) -> f32 {
        self.segs.last().map(|s| s.to).unwrap_or(self.init)
    }

    /// Time at which the last segment ends.
    pub fn end_time(&self) -> f64 {
        self.segs.last().map(|s| s.start + s.dur).unwrap_or(f64::NEG_INFINITY)
    }

    pub fn is_active(&self, t: f64) -> bool {
        t < self.end_time()
    }

    /// Start a transition at `start` toward `to`.
    pub fn set_at(&mut self, start: f64, to: f32, dur: f64, ease: Bezier) {
        let from = self.value(start);
        self.segs.retain(|s| s.start < start);
        // fold everything but the segment still in flight into `init`
        if self.segs.len() > 1 {
            let keep = self.segs.len() - 1;
            self.init = self.value(self.segs[keep].start);
            self.segs.drain(..keep);
        }
        self.segs.push(Seg { start, dur, from, to, ease });
    }

    /// Jump straight to `v`.
    pub fn snap(&mut self, v: f32) {
        self.init = v;
        self.segs.clear();
    }
}

/// Colour-valued transition (premultiplied lerp), one segment.
#[derive(Clone, Debug)]
pub struct Blend<T: Copy> {
    from: T,
    to: T,
    start: f64,
    dur: f64,
    ease: Bezier,
}

impl<T: Copy> Blend<T> {
    pub fn new(v: T) -> Self {
        Blend { from: v, to: v, start: f64::NEG_INFINITY, dur: 0.0, ease: CSS_EASE_OUT }
    }
    pub fn progress(&self, t: f64) -> f32 {
        if self.dur <= 0.0 {
            return 1.0;
        }
        self.ease.ease(((t - self.start) / self.dur).clamp(0.0, 1.0) as f32)
    }
    pub fn from(&self) -> T {
        self.from
    }
    pub fn to(&self) -> T {
        self.to
    }
    pub fn is_active(&self, t: f64) -> bool {
        t < self.start + self.dur
    }
    /// Retarget: caller supplies the value at `start` as the new `from`.
    pub fn retarget(&mut self, from_now: T, to: T, start: f64, dur: f64, ease: Bezier) {
        self.from = from_now;
        self.to = to;
        self.start = start;
        self.dur = dur;
        self.ease = ease;
    }
}

// --- looping keyframes -------------------------------------------------------------------

/// Tailwind `animate-pulse`: `50% { opacity: .5 }`, 2 s, per-segment `cubic-bezier(.4,0,.6,1)`.
pub fn pulse_opacity(elapsed_ms: f64) -> f32 {
    let phase = (elapsed_ms.rem_euclid(2000.0) / 2000.0) as f32;
    if phase < 0.5 {
        1.0 - 0.5 * PULSE_EASE.ease(phase * 2.0)
    } else {
        0.5 + 0.5 * PULSE_EASE.ease((phase - 0.5) * 2.0)
    }
}

pub const SIGNAL_SPIN_MS: f64 = 1600.0;
pub const SIGNAL_BREATHE_MS: f64 = 1900.0;

/// Signal glow orbit angle in degrees (0 = top, clockwise).
pub fn signal_angle_deg(elapsed_ms: f64) -> f32 {
    (elapsed_ms.rem_euclid(SIGNAL_SPIN_MS) / SIGNAL_SPIN_MS * 360.0) as f32
}

/// Halo breathe: returns `(opacity factor 0.55..1, scale 0.96..1.05)`.
pub fn signal_breathe(elapsed_ms: f64) -> (f32, f32) {
    let phase = (elapsed_ms.rem_euclid(SIGNAL_BREATHE_MS) / SIGNAL_BREATHE_MS) as f32;
    // 0% -> 50% rises, 50% -> 100% falls; ease-in-out per segment
    let e = if phase < 0.5 {
        EASE_IN_OUT.ease(phase * 2.0)
    } else {
        1.0 - EASE_IN_OUT.ease((phase - 0.5) * 2.0)
    };
    (0.55 + 0.45 * e, 0.96 + 0.09 * e)
}

// --- waveform ----------------------------------------------------------------------------

pub const WAVE_BARS: usize = 11;
pub const WAVE_MIN_PX: f32 = 4.0;
pub const WAVE_MAX_PX: f32 = 22.0;
/// Time one bar slot stands for: the history scrolls one bar every 80 ms (880 ms across the lane).
pub const WAVE_STEP_MS: f64 = 80.0;
/// Envelope follower time constants: a rising level is followed within a frame or two (12 ms), a
/// falling one lets go over ~120 ms so syllables read as separate beats instead of flicker.
pub const ENV_ATTACK_MS: f64 = 12.0;
pub const ENV_RELEASE_MS: f64 = 120.0;
/// Below this (with nothing pushing it up) the envelope is exactly zero, so silence is exactly flat.
const ENV_SNAP: f32 = 0.005;
/// Longest gap one envelope update integrates over; a longer silence between ticks (the slow
/// cadence) is a hold at the old value followed by one normal step.
const ENV_MAX_DT_MS: f64 = 50.0;
/// History kept behind the newest sample: the oldest bar looks this far back.
const WAVE_HISTORY_MS: f64 = (WAVE_BARS as f64 - 1.0) * WAVE_STEP_MS;
/// Resting silhouette (idle/unavailable compact pill).
pub const RESTING_WAVE: [f32; WAVE_BARS] = [6.0, 12.0, 5.0, 9.0, 7.0, 22.0, 18.0, 5.0, 20.0, 12.0, 17.0];

/// Bar height for a display level in 0..1 (the meter's output, see `audio::meter`): `4 + level * 18`.
/// Non-finite input draws the resting height.
pub fn bar_height(level: f32) -> f32 {
    let l = if level.is_finite() { level.clamp(0.0, 1.0) } else { 0.0 };
    WAVE_MIN_PX + l * (WAVE_MAX_PX - WAVE_MIN_PX)
}

// --- timer cadence -------------------------------------------------------------------------------

/// Animation timer period while frames change (~62.5 Hz).
pub const TICK_MS: u32 = 16;
/// Timer period once a recording has gone quiet (~15 Hz): enough to notice a change the capture
/// thread did not already announce, and to keep Windows' timer coalescing out of the picture.
pub const SLOW_TICK_MS: u32 = 66;
/// Consecutive recording frames in which nothing visible changed before the timer slows down.
pub const QUIET_FRAMES: u32 = 8;

/// Counts consecutive frames that changed nothing; decides when the animation timer may slow down.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct QuietRun(u32);

impl QuietRun {
    /// Record one frame (`skipped`: the renderer found nothing to repaint).
    pub fn frame(&mut self, skipped: bool) {
        self.0 = if skipped { self.0.saturating_add(1) } else { 0 };
    }

    pub fn reset(&mut self) {
        self.0 = 0;
    }

    /// Long enough without a visible change to run the timer slowly.
    pub fn is_quiet(&self) -> bool {
        self.0 >= QUIET_FRAMES
    }
}

/// The waveform: a continuous envelope E(t) of the microphone's display level, and the bars as a
/// tapped delay line over it. Bar 10 (right-most) is the live envelope; bar `j` shows what the
/// envelope was `(10 - j) * 80 ms` earlier, interpolated between frames, so the picture scrolls
/// smoothly right to left instead of stepping every 80 ms.
///
/// The envelope follows `max(level, peak)` per tick (peak = the highest level published since the
/// previous tick, so nothing between two frames is lost) with a 12 ms attack and 120 ms release,
/// integrated exactly, so the result does not depend on the frame rate. Silence is exactly zero and
/// then every bar is exactly 4 px, which lets the renderer skip frames and the timer slow down.
///
/// `heights(t)` is a pure function of the fed samples and `t`. After the last tick the picture holds
/// (a recording that ended freezes its bars while the pill shrinks).
#[derive(Clone, Debug)]
pub struct Waveform {
    /// `(time, envelope)` samples, oldest first, enough to look `WAVE_HISTORY_MS` back from the newest.
    ring: Vec<(f64, f32)>,
    e: f32,
    last_t: f64,
}

impl Default for Waveform {
    fn default() -> Self {
        Self::new()
    }
}

impl Waveform {
    pub fn new() -> Self {
        Waveform { ring: Vec::with_capacity(160), e: 0.0, last_t: f64::NEG_INFINITY }
    }

    /// New recording: start from silence.
    pub fn reset(&mut self) {
        self.ring.clear();
        self.e = 0.0;
        self.last_t = f64::NEG_INFINITY;
    }

    /// Advance the envelope to `now` (ms). `level` is the live display level, `peak` the highest one
    /// since the previous tick; both 0..1, anything else counts as 0.
    pub fn tick(&mut self, now: f64, level: f32, peak: f32) {
        let clean = |v: f32| if v.is_finite() { v.clamp(0.0, 1.0) } else { 0.0 };
        let target = clean(level).max(clean(peak));
        if !now.is_finite() {
            return;
        }
        if self.ring.is_empty() {
            // one frame of silence before the first sample, so the first sample ramps instead of jumping
            self.ring.push((now - 16.0, 0.0));
            self.last_t = now - 16.0;
        }
        if now < self.last_t {
            return;
        }
        let gap = now - self.last_t;
        if gap > ENV_MAX_DT_MS {
            // nothing was sampled for a while (slow cadence): the old value held until just before now
            self.ring.push((now - ENV_MAX_DT_MS, self.e));
        }
        let dt = gap.min(ENV_MAX_DT_MS);
        let tau = if target > self.e { ENV_ATTACK_MS } else { ENV_RELEASE_MS };
        self.e += (target - self.e) * (1.0 - (-dt / tau).exp() as f32);
        if target == 0.0 && self.e < ENV_SNAP {
            self.e = 0.0;
        }
        self.e = self.e.clamp(0.0, 1.0);
        if gap == 0.0 {
            if let Some(last) = self.ring.last_mut() {
                *last = (now, self.e);
            }
        } else {
            self.ring.push((now, self.e));
        }
        self.last_t = now;
        // keep one sample older than the horizon as the interpolation anchor
        let horizon = now - WAVE_HISTORY_MS - 2.0 * ENV_MAX_DT_MS;
        let drop = self.ring.partition_point(|&(t, _)| t < horizon).saturating_sub(1);
        if drop > 0 {
            self.ring.drain(..drop);
        }
    }

    /// Envelope at time `t` by linear interpolation; zero before any sample, held after the newest.
    fn envelope_at(&self, t: f64) -> f32 {
        let (first, last) = match (self.ring.first(), self.ring.last()) {
            (Some(f), Some(l)) => (f, l),
            _ => return 0.0,
        };
        if t <= first.0 {
            return first.1;
        }
        if t >= last.0 {
            return last.1;
        }
        let k = self.ring.partition_point(|&(rt, _)| rt <= t);
        let (a, b) = (self.ring[k - 1], self.ring[k]);
        let span = b.0 - a.0;
        if span <= 0.0 {
            return b.1;
        }
        a.1 + (b.1 - a.1) * ((t - a.0) / span) as f32
    }

    pub fn height(&self, i: usize, t: f64) -> f32 {
        let t = t.min(self.last_t);
        let age = (WAVE_BARS - 1 - i.min(WAVE_BARS - 1)) as f64 * WAVE_STEP_MS;
        bar_height(self.envelope_at(t - age))
    }

    pub fn heights(&self, t: f64) -> [f32; WAVE_BARS] {
        let mut out = [0.0; WAVE_BARS];
        for (i, o) in out.iter_mut().enumerate() {
            *o = self.height(i, t);
        }
        out
    }

    /// Anything above the floor still in the picture, and the animation is being fed (within one
    /// maximum tick gap of the newest sample). Once the line has been flat for 880 ms this is false.
    pub fn is_easing(&self, t: f64) -> bool {
        t <= self.last_t + ENV_MAX_DT_MS && (self.e > 0.0 || self.ring.iter().any(|&(_, e)| e > 0.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quiet_run_needs_consecutive_skipped_frames() {
        let mut q = QuietRun::default();
        for _ in 0..QUIET_FRAMES - 1 {
            q.frame(true);
        }
        assert!(!q.is_quiet());
        q.frame(false);
        assert!(!q.is_quiet(), "a changed frame starts the count over");
        for _ in 0..QUIET_FRAMES {
            q.frame(true);
        }
        assert!(q.is_quiet());
        q.reset();
        assert!(!q.is_quiet());
        for _ in 0..1000 {
            q.frame(true);
        }
        assert!(q.is_quiet(), "the counter saturates instead of wrapping");
    }

    #[test]
    fn bezier_endpoints_and_monotonic() {
        for b in [HOUSE, CSS_EASE_OUT, TW_EASE_OUT, EASE_IN_OUT, PULSE_EASE] {
            assert_eq!(b.ease(0.0), 0.0);
            assert_eq!(b.ease(1.0), 1.0);
            let mut prev = 0.0;
            for i in 1..=100 {
                let v = b.ease(i as f32 / 100.0);
                assert!(v + 1e-5 >= prev, "{b:?} not monotonic at {i}");
                prev = v;
            }
        }
        // house curve is front-loaded: past 50% progress at 20% time
        assert!(HOUSE.ease(0.2) > 0.5);
        // linear
        assert!((LINEAR.ease(0.37) - 0.37).abs() < 1e-5);
        // known value: ease-in-out(0.5) = 0.5
        assert!((EASE_IN_OUT.ease(0.5) - 0.5).abs() < 1e-3);
    }

    #[test]
    fn track_retargets_from_current_value() {
        let mut t = Track::new(0.0);
        t.set_at(0.0, 1.0, 300.0, LINEAR);
        assert!((t.value(150.0) - 0.5).abs() < 1e-5);
        t.set_at(150.0, 0.0, 300.0, LINEAR);
        assert!((t.value(150.0) - 0.5).abs() < 1e-5);
        assert!((t.value(300.0) - 0.25).abs() < 1e-5);
        assert!(t.value(450.0).abs() < 1e-5);
        assert!(!t.is_active(450.0) && t.is_active(449.0));
    }

    #[test]
    fn track_future_schedule_is_cancelled_by_earlier_retarget() {
        let mut t = Track::new(0.0);
        t.set_at(260.0, 1.0, 300.0, HOUSE);
        assert_eq!(t.value(100.0), 0.0);
        // stop before the expansion starts: scheduled expansion must vanish
        t.set_at(100.0, 0.0, 300.0, HOUSE);
        assert_eq!(t.value(400.0), 0.0);
        assert_eq!(t.target(), 0.0);
    }

    #[test]
    fn pulse_matches_tailwind() {
        assert!((pulse_opacity(0.0) - 1.0).abs() < 1e-5);
        assert!((pulse_opacity(1000.0) - 0.5).abs() < 1e-3);
        assert!((pulse_opacity(2000.0) - 1.0).abs() < 1e-5);
        assert!(pulse_opacity(500.0) < 1.0 && pulse_opacity(500.0) > 0.5);
    }

    #[test]
    fn signal_loops() {
        assert_eq!(signal_angle_deg(0.0), 0.0);
        assert!((signal_angle_deg(800.0) - 180.0).abs() < 1e-3);
        assert!((signal_angle_deg(1600.0)).abs() < 1e-3);
        let (o, s) = signal_breathe(0.0);
        assert!((o - 0.55).abs() < 1e-5 && (s - 0.96).abs() < 1e-5);
        let (o, s) = signal_breathe(950.0);
        assert!((o - 1.0).abs() < 1e-3 && (s - 1.05).abs() < 1e-3);
    }

    #[test]
    fn level_mapping_is_linear_in_the_display_level() {
        assert_eq!(bar_height(0.0), 4.0);
        assert_eq!(bar_height(1.0), 22.0);
        assert_eq!(bar_height(0.5), 13.0);
        // out of range and garbage never leave the 4..22 px band
        assert_eq!(bar_height(-1.0), 4.0);
        assert_eq!(bar_height(7.0), 22.0);
        assert_eq!(bar_height(f32::NAN), 4.0);
        assert_eq!(bar_height(f32::INFINITY), 4.0);
    }

    // --- the envelope and the delay line --------------------------------------------------------

    /// Feed `level` for `ms` of frame time at `hz`, returning the time after the last frame.
    fn feed(w: &mut Waveform, t: &mut f64, hz: f64, ms: f64, level: f32) {
        let dt = 1000.0 / hz;
        let end = *t + ms;
        while *t < end - 1e-9 {
            w.tick(*t, level, level);
            *t += dt;
        }
    }

    const CADENCES: [f64; 6] = [30.0, 60.0, 72.0, 75.0, 100.0, 144.0];

    /// A step reaches half height within 15 ms of frame time at any cadence, and never overshoots.
    #[test]
    fn envelope_attack_is_fast_and_frame_rate_independent() {
        for hz in CADENCES {
            let mut w = Waveform::new();
            let mut t = 1000.0;
            feed(&mut w, &mut t, hz, 500.0, 0.0);
            let t_step = t;
            let dt = 1000.0 / hz;
            let mut reached = None;
            for k in 0..40 {
                let now = t_step + k as f64 * dt;
                w.tick(now, 1.0, 1.0);
                let h = w.height(WAVE_BARS - 1, now);
                assert!(h <= WAVE_MAX_PX + 1e-4, "{hz} Hz: overshoot {h}");
                if reached.is_none() && h >= 13.0 {
                    reached = Some(now - t_step);
                }
            }
            let r = reached.unwrap_or(f64::INFINITY);
            assert!(r <= 15.0, "{hz} Hz: half height after {r:.1} ms of frame time");
            // exact integration: after 100 ms the same fraction whatever the cadence
            let e = (w.height(WAVE_BARS - 1, t_step + 40.0 * dt) - 4.0) / 18.0;
            assert!(e > 0.99, "{hz} Hz: settled at {e}");
        }
    }

    /// The release is exponential with 120 ms whatever the cadence (it is integrated exactly).
    #[test]
    fn envelope_release_follows_its_time_constant() {
        for hz in CADENCES {
            let mut w = Waveform::new();
            let mut t = 0.0;
            feed(&mut w, &mut t, hz, 400.0, 1.0);
            let dt = 1000.0 / hz;
            let t_drop = t;
            let mut prev = 1.0f32;
            for k in 0..30 {
                let now = t_drop + k as f64 * dt;
                w.tick(now, 0.0, 0.0);
                let e = (w.height(WAVE_BARS - 1, now) - 4.0) / 18.0;
                assert!(e <= prev + 1e-6, "{hz} Hz: release must be monotonic");
                prev = e;
                // the first dropped frame already integrates one frame of release
                let want = (-(((k + 1) as f64) * dt) / ENV_RELEASE_MS).exp() as f32;
                if want > 0.1 {
                    assert!((e - want).abs() < 0.01, "{hz} Hz frame {k}: {e} vs {want}");
                }
            }
        }
    }

    /// Bar `j` is exactly the live envelope `(10 - j) * 80 ms` ago: the newest bar is the envelope
    /// itself, older bars are delayed copies, and everything in between is interpolated.
    #[test]
    fn bars_are_a_delay_line_over_the_envelope() {
        for hz in CADENCES {
            let mut w = Waveform::new();
            let mut t = 0.0;
            // a short burst, then silence: a bump travelling along the lane
            feed(&mut w, &mut t, hz, 300.0, 0.0);
            feed(&mut w, &mut t, hz, 40.0, 0.9);
            feed(&mut w, &mut t, hz, 900.0, 0.0);
            let now = t - 1000.0 / hz;
            for k in 0..=6 {
                let at = now - 20.0 * k as f64;
                let h = w.heights(at);
                for (j, &hj) in h.iter().enumerate() {
                    let age = (WAVE_BARS - 1 - j) as f64 * WAVE_STEP_MS;
                    let want = w.height(WAVE_BARS - 1, at - age);
                    assert!((hj - want).abs() < 1e-4, "{hz} Hz bar {j} at {at}: {hj} vs the envelope {age} ms ago {want}");
                }
            }
            // the bump is in bar 10 first and in bar 10 - k about k * 80 ms later
            let t_peak = {
                let mut best = (0.0f32, 0.0f64);
                let mut x = 300.0;
                while x < 600.0 {
                    let h = w.height(WAVE_BARS - 1, x);
                    if h > best.0 {
                        best = (h, x);
                    }
                    x += 1.0;
                }
                best.1
            };
            for k in 1..WAVE_BARS {
                let when = t_peak + k as f64 * WAVE_STEP_MS;
                if when > now {
                    break;
                }
                let h = w.heights(when);
                let top = (0..WAVE_BARS).max_by(|&a, &b| h[a].total_cmp(&h[b])).unwrap();
                assert_eq!(top, WAVE_BARS - 1 - k, "{hz} Hz: {k} slots later the bump is in bar {top}");
            }
        }
    }

    /// Between two frames no bar moves by more than the envelope can: the picture scrolls, it never
    /// steps (the old design changed every bar at 12.5 Hz).
    #[test]
    fn bars_move_continuously_at_any_cadence() {
        for hz in CADENCES {
            let dt = 1000.0 / hz;
            // the largest one-frame rise: a full-scale step through the 12 ms attack
            let bound = 18.0 * (1.0 - (-dt / ENV_ATTACK_MS).exp() as f32) + 1e-3;
            let mut w = Waveform::new();
            let (mut t, mut k) = (0.0f64, 0u32);
            feed(&mut w, &mut t, hz, 200.0, 0.0);
            let mut prev = w.heights(t);
            while t < 4000.0 {
                // loud / quiet / medium syllables with pauses, so bars move both ways
                let lvl = [0.9, 0.2, 0.6, 0.0, 0.0, 1.0, 0.35][((t / 90.0) as u32 % 7) as usize];
                w.tick(t, lvl, lvl);
                let now = w.heights(t);
                for i in 0..WAVE_BARS {
                    assert!((WAVE_MIN_PX..=WAVE_MAX_PX).contains(&now[i]), "{hz} Hz bar {i}: {} out of range", now[i]);
                    assert!((now[i] - prev[i]).abs() <= bound, "{hz} Hz bar {i}: moved {} px in one frame at {t:.1} ms (bound {bound:.2})", now[i] - prev[i]);
                }
                prev = now;
                t += dt;
                k += 1;
            }
            assert!(k > 100);
        }
    }

    /// A peak between two frames is not lost: the envelope follows the highest level of the interval.
    #[test]
    fn a_peak_between_frames_still_moves_the_bars() {
        let mut w = Waveform::new();
        let mut t = 0.0;
        feed(&mut w, &mut t, 60.0, 200.0, 0.0);
        w.tick(t, 0.0, 0.8); // the live level is already 0 again, but 0.8 was published since the last frame
        assert!(w.height(WAVE_BARS - 1, t) > 4.0 + 0.5 * 18.0 * 0.8 * 0.5, "{}", w.height(WAVE_BARS - 1, t));
    }

    /// Silence is exactly flat: after the history has gone by every bar is 4.0 px and the waveform
    /// no longer asks for frames.
    #[test]
    fn silence_is_exactly_flat_and_stops_the_animation() {
        for hz in CADENCES {
            let mut w = Waveform::new();
            let mut t = 0.0;
            feed(&mut w, &mut t, hz, 600.0, 0.8);
            assert!(w.is_easing(t - 1.0));
            feed(&mut w, &mut t, hz, 3000.0, 0.0);
            let now = t - 1000.0 / hz;
            assert!(w.heights(now).iter().all(|&h| h == WAVE_MIN_PX), "{hz} Hz: {:?}", w.heights(now));
            assert!(!w.is_easing(now), "{hz} Hz: flat for 880 ms must not animate");
        }
    }

    /// Between the end of the sound and the flat line the picture only ever falls.
    #[test]
    fn the_tail_after_speech_decays_without_wiggling() {
        let mut w = Waveform::new();
        let mut t = 0.0;
        feed(&mut w, &mut t, 60.0, 500.0, 0.7);
        let mut last_newest = w.height(WAVE_BARS - 1, t);
        for _ in 0..90 {
            w.tick(t, 0.0, 0.0);
            let h = w.height(WAVE_BARS - 1, t);
            assert!(h <= last_newest + 1e-5, "the newest bar rose in silence");
            last_newest = h;
            t += 1000.0 / 60.0;
        }
        assert_eq!(last_newest, WAVE_MIN_PX, "and it reaches exactly 4 px (snap)");
    }

    #[test]
    fn reset_starts_from_silence_and_garbage_is_ignored() {
        let mut w = Waveform::new();
        let mut t = 0.0;
        feed(&mut w, &mut t, 60.0, 500.0, 1.0);
        assert!(w.height(WAVE_BARS - 1, t) > 20.0);
        w.reset();
        assert!(w.heights(t).iter().all(|&h| h == WAVE_MIN_PX));
        assert!(!w.is_easing(t));
        w.tick(t, f32::NAN, f32::INFINITY);
        w.tick(t + 16.0, -3.0, f32::NEG_INFINITY);
        w.tick(f64::NAN, 1.0, 1.0);
        assert!(w.heights(t + 16.0).iter().all(|&h| h == WAVE_MIN_PX), "non-finite input draws silence");
    }

    /// `heights(t)` is a pure function of what was fed and `t`; after the newest tick it holds.
    #[test]
    fn heights_are_pure_and_hold_after_the_last_tick() {
        let mut w = Waveform::new();
        let mut t = 0.0;
        feed(&mut w, &mut t, 60.0, 700.0, 0.6);
        let last = t - 1000.0 / 60.0;
        assert_eq!(w.heights(last), w.heights(last));
        assert_eq!(w.heights(last + 500.0), w.heights(last), "a recording that ended freezes its bars");
        assert!(!w.is_easing(last + 500.0), "and asks for no frames");
        let c = w.clone();
        assert_eq!(c.heights(last - 123.0), w.heights(last - 123.0));
    }

    /// A long gap between ticks (the slow cadence of a silent recording) is a hold at the old value,
    /// then one normal step, not a lurch.
    #[test]
    fn a_slow_cadence_gap_holds_then_attacks_normally() {
        let mut w = Waveform::new();
        let mut t = 0.0;
        feed(&mut w, &mut t, 60.0, 300.0, 0.0);
        // 66 ms ticks at silence, then a sound
        for k in 0..20 {
            w.tick(t + k as f64 * 66.0, 0.0, 0.0);
        }
        let t_sound = t + 20.0 * 66.0;
        w.tick(t_sound, 0.5, 0.5);
        let h = w.height(WAVE_BARS - 1, t_sound);
        let one_step = 4.0 + 18.0 * 0.5 * (1.0 - (-ENV_MAX_DT_MS / ENV_ATTACK_MS).exp() as f32);
        assert!(h > 4.0 && h <= one_step + 1e-3, "{h} vs {one_step}");
        // just before the tick it was still flat
        assert_eq!(w.height(WAVE_BARS - 1, t_sound - ENV_MAX_DT_MS - 1.0), WAVE_MIN_PX);
        // and the bars to its left are still 4 px: the sound has not scrolled yet
        assert_eq!(w.height(WAVE_BARS - 2, t_sound), WAVE_MIN_PX);
    }

    // --- the whole pipeline, simulated ----------------------------------------------------------

    /// Pure Rust model of capture -> meter -> published level -> frame ticks -> envelope, with the
    /// audio packets and the frames on unrelated clocks. Returns the bar-10 height at each frame.
    struct Pipe {
        meter: crate::daemon::audio::meter::Meter,
        wave: Waveform,
        level: f32,
        peak: f32,
        /// Frame times (ms) and the height of the newest bar at each.
        frames: Vec<(f64, f32)>,
    }

    /// Run `pcm` (16 kHz) through the pipeline: 10 ms packets delivered when complete, frames every
    /// `frame_ms` starting at `phase`.
    fn simulate(pcm: &[i16], frame_ms: f64, phase: f64) -> Pipe {
        let mut p = Pipe { meter: crate::daemon::audio::meter::Meter::new(), wave: Waveform::new(), level: 0.0, peak: 0.0, frames: Vec::new() };
        let total_ms = pcm.len() as f64 / 16.0;
        let mut next_frame = phase;
        for (k, packet) in pcm.chunks(160).enumerate() {
            let delivered = (k + 1) as f64 * 10.0;
            // frames due before this packet arrives see the previous state
            while next_frame < delivered && next_frame < total_ms {
                p.wave.tick(next_frame, p.level, p.peak);
                p.peak = 0.0;
                p.frames.push((next_frame, p.wave.height(WAVE_BARS - 1, next_frame)));
                next_frame += frame_ms;
            }
            let (level, peak) = (&mut p.level, &mut p.peak);
            p.meter.push(packet, |h| {
                *level = h.level;
                *peak = peak.max(h.level);
            });
        }
        p
    }

    fn noise(db: f32, ms: usize, seed: &mut u64) -> Vec<i16> {
        let a = 10f32.powf(db / 20.0) * 3f32.sqrt() * 32768.0;
        (0..ms * 16)
            .map(|_| {
                *seed ^= *seed << 13;
                *seed ^= *seed >> 7;
                *seed ^= *seed << 17;
                let u = ((*seed >> 11) as f64 / (1u64 << 53) as f64) as f32 * 2.0 - 1.0;
                (u * a) as i16
            })
            .collect()
    }

    fn voiced(db: f32, ms: usize) -> Vec<i16> {
        let a = 10f32.powf(db / 20.0) * 2f32.sqrt() * 32768.0;
        (0..ms * 16).map(|n| (a * (n as f32 * 2.0 * std::f32::consts::PI * 190.0 / 16_000.0).sin()) as i16).collect()
    }

    /// Onset to half height: the old design took 77 ms on average and 116 ms at worst (before the OS's
    /// own capture and compositor latency). Here the audio-to-bar part must stay under 30 ms mean and
    /// 45 ms worst across every relative phase of the packet clock and the frame clock.
    #[test]
    fn onset_reaches_half_height_within_the_latency_budget() {
        let mut seed = 0x1234_5678_9ABC_DEF1u64;
        for (frame_ms, name) in [(1000.0 / 60.0, "60 Hz"), (1000.0 / 144.0, "144 Hz"), (1000.0 / 75.0, "75 Hz")] {
            let mut lat = Vec::new();
            for step in 0..24 {
                // the onset lands at a different offset inside the 10 ms packet each time
                let onset_ms = 800.0 + step as f64 * 0.7 + 0.37;
                let mut pcm = noise(-62.0, 800, &mut seed);
                pcm.extend(vec![0i16; (0.7 * step as f64 * 16.0 + 6.0) as usize]);
                let onset_idx = pcm.len();
                assert!((onset_idx as f64 / 16.0 - onset_ms).abs() < 1.0);
                pcm.extend(voiced(-24.0, 600));
                let p = simulate(&pcm, frame_ms, 3.1 * step as f64 % frame_ms);
                let t_on = onset_idx as f64 / 16.0;
                let (t_half, _) = p.frames.iter().copied().find(|&(t, h)| t >= t_on && h >= 13.0).expect("the bar reaches half height");
                lat.push(t_half - t_on);
            }
            let mean = lat.iter().sum::<f64>() / lat.len() as f64;
            let max = lat.iter().cloned().fold(0.0, f64::max);
            println!("{name}: onset to half height mean {mean:.1} ms, max {max:.1} ms");
            assert!(mean <= 30.0, "{name}: mean {mean:.1} ms");
            assert!(max <= 45.0, "{name}: max {max:.1} ms");
        }
    }

    /// A noisy but silent microphone draws a perfectly flat line for 10 s, so the renderer can skip
    /// every frame and the timer can slow down.
    #[test]
    fn a_noisy_silent_room_draws_nothing_for_ten_seconds() {
        let mut seed = 0xDEAD_BEEF_1234_5678u64;
        for db in [-65.0f32, -50.0, -45.0] {
            let pcm = noise(db, 10_000, &mut seed);
            let p = simulate(&pcm, 1000.0 / 60.0, 5.0);
            assert!(p.frames.len() > 590);
            let moved = p.frames.iter().filter(|&&(_, h)| h != WAVE_MIN_PX).count();
            assert_eq!(moved, 0, "{db} dBFS room noise moved the newest bar on {moved} frames");
            assert!(p.wave.heights(9990.0).iter().all(|&h| h == WAVE_MIN_PX));
            assert!(!p.wave.is_easing(9990.0));
        }
    }

    /// A recorded phrase (a TTS fixture, 16 kHz) between room noise: the newest bar follows the
    /// phrase's short-time energy with a lag of at most 40 ms, and the noise tail that follows is
    /// perfectly flat (the whole lane once the history has gone by).
    #[test]
    fn a_recorded_phrase_is_followed_closely_and_the_noise_tail_is_flat() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/tts_fillers.wav");
        let wav = crate::stt::Audio::from_wav(&std::fs::read(&path).expect("fixture")).expect("wav");
        assert_eq!(wav.sample_rate, 16_000);
        let mut seed = 0xA5A5_5A5A_1234_4321u64;
        let mut pcm = noise(-62.0, 500, &mut seed);
        let start_ms = 500.0;
        pcm.extend_from_slice(&wav.samples);
        let end_ms = pcm.len() as f64 / 16.0;
        pcm.extend(noise(-62.0, 3500, &mut seed));
        let p = simulate(&pcm, 1000.0 / 60.0, 4.0);
        // short-time energy (dB over the last 20 ms, floored at -52) at each frame, and the bar at it
        let energy = |t: f64| -> f64 {
            let end = ((t * 16.0) as usize).min(pcm.len());
            let from = end.saturating_sub(320);
            let w = &pcm[from..end];
            if w.is_empty() {
                return 0.0;
            }
            let ms = w.iter().map(|&s| (s as f64 / 32768.0).powi(2)).sum::<f64>() / w.len() as f64;
            (10.0 * (ms + 1e-12).log10() + 52.0).max(0.0)
        };
        let frames: Vec<(f64, f32)> = p.frames.iter().copied().filter(|&(t, _)| t >= start_ms + 100.0 && t <= end_ms).collect();
        let corr = |lag: f64| -> f64 {
            let xs: Vec<f64> = frames.iter().map(|&(_, h)| h as f64).collect();
            let ys: Vec<f64> = frames.iter().map(|&(t, _)| energy(t - lag)).collect();
            let (mx, my) = (xs.iter().sum::<f64>() / xs.len() as f64, ys.iter().sum::<f64>() / ys.len() as f64);
            let (mut sxy, mut sxx, mut syy) = (0.0, 0.0, 0.0);
            for (x, y) in xs.iter().zip(&ys) {
                sxy += (x - mx) * (y - my);
                sxx += (x - mx).powi(2);
                syy += (y - my).powi(2);
            }
            sxy / (sxx * syy).sqrt()
        };
        let best = (0..=100).step_by(5).map(|l| (l as f64, corr(l as f64))).max_by(|a, b| a.1.total_cmp(&b.1)).unwrap();
        println!("phrase follows the energy with a lag of {} ms (correlation {:.3})", best.0, best.1);
        assert!(best.1 > 0.75, "the bar must follow the phrase's energy, correlation {:.3}", best.1);
        assert!(best.0 <= 40.0, "lag {} ms", best.0);
        // the tail: noise only
        for &(t, h) in p.frames.iter().filter(|&&(t, _)| t > end_ms + 800.0) {
            assert_eq!(h, WAVE_MIN_PX, "newest bar not flat at {t:.0} ms (phrase ended at {end_ms:.0})");
        }
        let last = p.frames.last().unwrap().0;
        assert!(last > end_ms + 2000.0);
        assert!(p.wave.heights(last).iter().all(|&h| h == WAVE_MIN_PX), "the whole lane is flat in the noise tail");
        assert!(!p.wave.is_easing(last));
    }

    /// The speech train in the middle looks like speech (bars rise and fall) and the room before and
    /// after it is exactly flat again within the envelope's release and the delay line's length.
    #[test]
    fn speech_between_silences_rises_and_returns_to_a_flat_line() {
        let mut seed = 77u64;
        let mut pcm = noise(-60.0, 1000, &mut seed);
        for _ in 0..5 {
            pcm.extend(voiced(-26.0, 180));
            pcm.extend(noise(-60.0, 90, &mut seed));
        }
        pcm.extend(noise(-60.0, 3000, &mut seed));
        let p = simulate(&pcm, 1000.0 / 60.0, 2.0);
        let before = p.frames.iter().filter(|&&(t, _)| t < 1000.0).all(|&(_, h)| h == WAVE_MIN_PX);
        assert!(before);
        let peak = p.frames.iter().map(|&(_, h)| h).fold(0.0, f32::max);
        assert!(peak > 18.0, "peak {peak}");
        // dips between syllables
        let mid: Vec<f32> = p.frames.iter().filter(|&&(t, _)| (1400.0..2300.0).contains(&t)).map(|&(_, h)| h).collect();
        assert!(mid.iter().cloned().fold(30.0, f32::min) < 14.0, "syllable gaps show");
        let end = p.frames.last().unwrap().0;
        assert!(p.wave.heights(end).iter().all(|&h| h == WAVE_MIN_PX));
    }
}
