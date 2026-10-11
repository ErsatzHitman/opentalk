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
/// About how long the cancel button's pop takes to settle (tests sample the motion over it).
#[cfg(test)]
pub const CANCEL_EMERGE_MS: f64 = 380.0;

// --- spring --------------------------------------------------------------------------------------

/// A damped spring: `zeta` < 1 overshoots and bounces, 1 settles as fast as possible without one.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SpringParams {
    /// Natural frequency (rad/s).
    pub omega: f64,
    pub zeta: f64,
}

/// The detached cancel button popping out: ~8 % overshoot (a few px past its spot and a slight
/// swell), the peak at ~230 ms, settled in ~400 ms.
pub const CANCEL_POP: SpringParams = SpringParams { omega: 17.0, zeta: 0.62 };
/// ... and tucking back in: firm, no bounce.
pub const CANCEL_TUCK: SpringParams = SpringParams { omega: 28.0, zeta: 1.0 };

/// A value driven by a damped spring toward a target, in closed form (exact at any frame rate). A new
/// target keeps the current position and velocity, so reversing mid-flight is seamless.
#[derive(Clone, Copy, Debug)]
pub struct Spring {
    target: f32,
    /// Offset from the target and velocity (units/s) at `t0`.
    x0: f64,
    v0: f64,
    t0: f64,
    p: SpringParams,
}

impl Spring {
    pub fn new(v: f32) -> Spring {
        Spring { target: v, x0: 0.0, v0: 0.0, t0: 0.0, p: CANCEL_TUCK }
    }

    /// Offset from the target and velocity `dt` ms after `t0`.
    fn state(&self, t: f64) -> (f64, f64) {
        if self.x0 == 0.0 && self.v0 == 0.0 {
            return (0.0, 0.0);
        }
        let dt = ((t - self.t0) / 1000.0).max(0.0);
        let (w, z) = (self.p.omega, self.p.zeta);
        let (a, v0) = (self.x0, self.v0);
        if z < 1.0 {
            let wd = w * (1.0 - z * z).sqrt();
            let b = (v0 + z * w * a) / wd;
            let (s, c) = (wd * dt).sin_cos();
            let e = (-z * w * dt).exp();
            let x = e * (a * c + b * s);
            let v = e * ((b * wd - z * w * a) * c - (a * wd + z * w * b) * s);
            (x, v)
        } else {
            // critically damped
            let b = v0 + w * a;
            let e = (-w * dt).exp();
            (e * (a + b * dt), e * (b - w * (a + b * dt)))
        }
    }

    /// Position at `t`; exactly the target once the spring has come to rest.
    pub fn value(&self, t: f64) -> f32 {
        if !self.is_active(t) {
            return self.target;
        }
        self.target + self.state(t).0 as f32
    }

    pub fn target(&self) -> f32 {
        self.target
    }

    /// Head for `to` from wherever the spring is at `now`, with `p`.
    pub fn set(&mut self, now: f64, to: f32, p: SpringParams) {
        let (x, v) = self.state(now);
        let pos = self.target as f64 + x;
        *self = Spring { target: to, x0: pos - to as f64, v0: v, t0: now, p };
    }

    /// Still visibly moving (more than 1/1000 away, or faster than 0.01 per second).
    pub fn is_active(&self, t: f64) -> bool {
        let (x, v) = self.state(t);
        x.abs() > 1e-3 || v.abs() > 1e-2
    }

    pub fn snap(&mut self, v: f32) {
        *self = Spring { target: v, x0: 0.0, v0: 0.0, t0: 0.0, p: self.p };
    }
}

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
/// A new sample enters every 80 ms and the bars scroll one place (880 ms across the lane).
pub const WAVE_STEP_MS: f64 = 80.0;
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

/// How long a bar takes to reach a new height (OpenWhispr: `transition-[height] duration-75 ease-out`).
pub const WAVE_EASE_MS: f64 = 75.0;

/// Idle ripple: while recording in silence the bars breathe in a slow wave that spreads from the
/// centre outwards, so a live microphone never looks frozen. It is deliberately unlike speech (which
/// scrolls right to left, jagged, up to 22 px): smooth, symmetric, at most `IDLE_PX` above the floor.
pub const IDLE_PERIOD_MS: f64 = 1600.0;
pub const IDLE_PX: f32 = 2.6;
/// Bar level at which the ripple has fully given way to speech.
pub const IDLE_GIVE_WAY: f32 = 0.2;
/// How long the ripple takes to fade in once the first sample arrives.
pub const IDLE_FADE_MS: f64 = 400.0;

/// Extra height (px) the idle ripple adds to bar `i` at `t` ms after the recording's first sample.
pub fn idle_px(i: usize, since_start: f64) -> f32 {
    if !since_start.is_finite() || since_start <= 0.0 {
        return 0.0;
    }
    let c = (WAVE_BARS - 1) as f64 / 2.0;
    let d = (i as f64 - c).abs() / c; // 0 at the centre, 1 at either end
    let phase = std::f64::consts::TAU * (since_start / IDLE_PERIOD_MS - d * 0.45);
    let breathe = 0.5 - 0.5 * phase.cos(); // starts at 0, so the ripple grows out of the floor
    let taper = 1.0 - 0.4 * d; // the ends move less than the middle
    let fade = (since_start / IDLE_FADE_MS).min(1.0);
    (IDLE_PX as f64 * breathe * taper * fade) as f32
}

/// The waveform, as OpenWhispr's `PillWaveform` draws it: every `WAVE_STEP_MS` (80 ms) the live level
/// is sampled, the bars shift one place to the left and the sample enters on the right; each bar then
/// eases to its new height over `WAVE_EASE_MS` (ease-out). 80 ms spacing lets neighbouring bars
/// straddle syllable onsets and gaps, so the signal itself supplies the bar-to-bar variance.
///
/// Silence is exactly zero (the meter's dead zone); the bars then show only the idle ripple
/// ([`idle_px`]), which fades out as a bar's speech level rises. `heights(t)` is a pure function of
/// the fed samples and `t`; after the last tick the picture holds (a recording that ended freezes).
#[derive(Clone, Debug)]
pub struct Waveform {
    /// Levels before and after the latest shift, oldest bar first.
    from: [f32; WAVE_BARS],
    to: [f32; WAVE_BARS],
    /// When the latest sample was taken (the eases start there).
    sampled_at: f64,
    last_t: f64,
    /// The idle ripple's clock: when this recording started (or its first sample, if nobody said)
    /// and when it ended (the ripple holds from there).
    started_at: f64,
    ended_at: f64,
}

impl Default for Waveform {
    fn default() -> Self {
        Self::new()
    }
}

/// CSS `ease-out` (cubic-bezier(0, 0, 0.58, 1)) at progress `x` in 0..=1.
fn ease_out(x: f64) -> f64 {
    let x = x.clamp(0.0, 1.0);
    // solve the bezier's x(s) = x by Newton, then return y(s)
    let bx = |s: f64| 3.0 * (1.0 - s) * s * s * 0.58 + s * s * s;
    let dbx = |s: f64| 6.0 * (1.0 - s) * s * 0.58 - 3.0 * s * s * 0.58 + 3.0 * s * s;
    let mut s = x;
    for _ in 0..6 {
        let d = dbx(s);
        if d.abs() < 1e-6 {
            break;
        }
        s = (s - (bx(s) - x) / d).clamp(0.0, 1.0);
    }
    3.0 * (1.0 - s) * (1.0 - s) * s + 3.0 * (1.0 - s) * s * s + s * s * s
}

impl Waveform {
    pub fn new() -> Self {
        Waveform { from: [0.0; WAVE_BARS], to: [0.0; WAVE_BARS], sampled_at: f64::NEG_INFINITY, last_t: f64::NEG_INFINITY, started_at: f64::NAN, ended_at: f64::INFINITY }
    }

    /// New recording: start from silence (never replay the previous recording's wave).
    pub fn reset(&mut self) {
        *self = Waveform::new();
    }

    /// New recording at `now`: start from silence, the idle ripple's clock starts here.
    pub fn begin(&mut self, now: f64) {
        self.reset();
        self.started_at = if now.is_finite() { now } else { f64::NAN };
    }

    /// The recording ended at `now`: the idle ripple holds its picture from here.
    pub fn end(&mut self, now: f64) {
        if now.is_finite() {
            self.ended_at = self.ended_at.min(now);
        }
    }

    /// Advance to `now` (ms): take a sample of `level` when 80 ms have passed since the last one.
    /// `level` is the live bar level 0..1 (anything else counts as 0); `peak` is not used (OpenWhispr
    /// samples the live value, which keeps the bars' contrast).
    pub fn tick(&mut self, now: f64, level: f32, _peak: f32) {
        if !now.is_finite() || now < self.last_t {
            return;
        }
        self.last_t = now;
        if self.started_at.is_nan() {
            self.started_at = now;
        }
        if now - self.sampled_at < WAVE_STEP_MS {
            return;
        }
        let v = if level.is_finite() { level.clamp(0.0, 1.0) } else { 0.0 };
        // every bar continues from where it is now
        let cur = self.levels_at(now);
        self.from = cur;
        self.to.copy_within(1.., 0);
        self.to[WAVE_BARS - 1] = v;
        // a late tick (slow cadence) does not owe the missed samples: the next one is 80 ms from now
        self.sampled_at = now;
    }

    fn levels_at(&self, t: f64) -> [f32; WAVE_BARS] {
        let k = ease_out((t - self.sampled_at) / WAVE_EASE_MS) as f32;
        let mut out = [0.0; WAVE_BARS];
        for (i, o) in out.iter_mut().enumerate() {
            *o = self.from[i] + (self.to[i] - self.from[i]) * k;
        }
        out
    }

    #[cfg(test)]
    pub fn height(&self, i: usize, t: f64) -> f32 {
        bar_height(self.levels_at(t.min(self.last_t))[i.min(WAVE_BARS - 1)])
    }

    /// Bar heights at `t`: the speech levels plus the idle ripple, which gives way as a bar's level
    /// rises. Frozen at the last tick like the levels.
    pub fn heights(&self, t: f64) -> [f32; WAVE_BARS] {
        let levels = self.levels_at(t.min(self.last_t));
        let since = t.min(self.ended_at) - self.started_at;
        let mut out = [0.0; WAVE_BARS];
        for (i, o) in out.iter_mut().enumerate() {
            let l = if levels[i].is_finite() { levels[i] } else { 0.0 };
            let give_way = 1.0 - (l / IDLE_GIVE_WAY).clamp(0.0, 1.0);
            *o = bar_height(l) + idle_px(i, since) * give_way * give_way;
        }
        out
    }

    /// The recording is live at `t` (the idle ripple keeps the bars moving even in silence).
    pub fn is_easing(&self, t: f64) -> bool {
        !self.started_at.is_nan() && t < self.ended_at
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

    #[test]
    fn a_sample_every_80_ms_scrolls_left_and_eases_in() {
        let mut w = Waveform::new();
        w.tick(1000.0, 0.5, 0.0);
        // the newest bar starts at the floor and eases to 4 + 0.5 * 18 over 75 ms
        assert_eq!(w.height(WAVE_BARS - 1, 1000.0), WAVE_MIN_PX);
        let mid = w.height(WAVE_BARS - 1, 1000.0);
        w.tick(1040.0, 0.9, 0.0);
        assert!(w.height(WAVE_BARS - 1, 1040.0) > mid, "eases up between samples");
        w.tick(1075.0, 0.9, 0.0);
        assert!((w.height(WAVE_BARS - 1, 1075.0) - 13.0).abs() < 0.01, "arrives in 75 ms");
        // no new sample before 80 ms
        assert!((w.height(WAVE_BARS - 1, 1075.0) - bar_height(0.5)).abs() < 0.01);
        w.tick(1080.0, 0.9, 0.0);
        w.tick(1160.0, 0.9, 0.0);
        w.tick(1235.0, 0.9, 0.0);
        assert!((w.height(WAVE_BARS - 2, 1235.0) - bar_height(0.9)).abs() < 0.01, "the old sample moved one bar left");
    }

    #[test]
    fn silence_breathes_gently_from_the_centre() {
        let mut w = Waveform::new();
        let mut t = 0.0;
        let mut seen_max = [WAVE_MIN_PX; WAVE_BARS];
        while t < 3200.0 {
            w.tick(t, 0.0, 0.0);
            for (m, h) in seen_max.iter_mut().zip(w.heights(t)) {
                assert!((WAVE_MIN_PX..=WAVE_MIN_PX + IDLE_PX + 1e-3).contains(&h), "idle stays small: {h}");
                *m = m.max(h);
            }
            t += 16.0;
        }
        assert!(seen_max.iter().all(|&m| m > WAVE_MIN_PX + 1.0), "every bar moves: {seen_max:?}");
        assert!(seen_max[WAVE_BARS / 2] > seen_max[0], "the centre moves more than the ends");
        assert!(w.is_easing(t), "a live recording keeps animating in silence");
        w.end(t);
        assert!(!w.is_easing(t + 1.0));
        assert_eq!(w.heights(t), w.heights(t + 500.0), "an ended recording holds its picture");
        // the ripple starts from the floor (no jump when recording begins)
        let mut fresh = Waveform::new();
        fresh.tick(0.0, 0.0, 0.0);
        assert!(fresh.heights(0.0).iter().all(|&h| h == WAVE_MIN_PX));
    }

    #[test]
    fn speech_replaces_the_idle_ripple() {
        let mut w = Waveform::new();
        let mut t = 0.0;
        while t < 2000.0 {
            w.tick(t, 0.6, 0.0);
            t += 16.0;
        }
        let hs = w.heights(t);
        assert!(hs.iter().all(|&h| (h - bar_height(0.6)).abs() < 0.01), "loud bars carry no ripple: {hs:?}");
    }

    #[test]
    fn reset_and_garbage() {
        let mut w = Waveform::new();
        assert!(w.heights(10.0).iter().all(|&h| h == WAVE_MIN_PX), "never fed: flat");
        w.tick(0.0, f32::NAN, 0.0);
        assert!(w.heights(10.0).iter().all(|&h| h.is_finite() && h <= WAVE_MIN_PX + IDLE_PX));
        w.tick(100.0, 1.0, 0.0);
        w.reset();
        assert!(w.heights(500.0).iter().all(|&h| h == WAVE_MIN_PX));
    }

    #[test]
    fn ease_out_matches_css() {
        assert!(ease_out(0.0).abs() < 1e-9 && (ease_out(1.0) - 1.0).abs() < 1e-9);
        assert!(ease_out(0.5) > 0.6, "ease-out is ahead of linear");
        let mut prev = 0.0;
        for k in 1..=100 {
            let v = ease_out(k as f64 / 100.0);
            assert!(v >= prev);
            prev = v;
        }
    }
}
