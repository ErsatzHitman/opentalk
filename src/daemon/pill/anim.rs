//! Animation primitives: CSS cubic-bezier easing, retargetable tracks (CSS-transition semantics),
//! the waveform sampler and the looping keyframe curves (pulse, signal spin/breathe).
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
pub const WAVE_SAMPLE_MS: f64 = 80.0;
pub const WAVE_BAR_MS: f64 = 75.0;
/// Resting silhouette (idle/unavailable compact pill).
pub const RESTING_WAVE: [f32; WAVE_BARS] = [6.0, 12.0, 5.0, 9.0, 7.0, 22.0, 18.0, 5.0, 20.0, 12.0, 17.0];

/// `resolveWaveformBarHeight`: `4 + min(1, (rms*8)^0.75) * 18`.
pub fn bar_height(rms: f32) -> f32 {
    let lvl = (rms.max(0.0) * 8.0).powf(0.75).min(1.0);
    WAVE_MIN_PX + lvl * (WAVE_MAX_PX - WAVE_MIN_PX)
}

#[derive(Clone, Copy, Debug)]
struct Bar {
    from: f32,
    to: f32,
    start: f64,
}

/// Scrolling level history: every 80 ms the bars shift left, the newest sample enters on the right
/// and every changed bar eases 75 ms (Tailwind ease-out).
#[derive(Clone, Debug)]
pub struct Waveform {
    levels: [f32; WAVE_BARS],
    bars: [Bar; WAVE_BARS],
    last_sample: f64,
}

impl Default for Waveform {
    fn default() -> Self {
        Self::new()
    }
}

impl Waveform {
    pub fn new() -> Self {
        Waveform {
            levels: [0.0; WAVE_BARS],
            bars: [Bar { from: WAVE_MIN_PX, to: WAVE_MIN_PX, start: f64::NEG_INFINITY }; WAVE_BARS],
            last_sample: f64::NEG_INFINITY,
        }
    }

    /// New recording: start from silence.
    pub fn reset(&mut self) {
        *self = Waveform::new();
    }

    /// Feed the live level; samples only when 80 ms elapsed since the previous sample.
    /// Returns `true` if a sample was taken.
    pub fn tick(&mut self, now: f64, rms: f32) -> bool {
        if now - self.last_sample < WAVE_SAMPLE_MS {
            return false;
        }
        self.last_sample = now;
        self.levels.rotate_left(1);
        self.levels[WAVE_BARS - 1] = if rms.is_finite() { rms } else { 0.0 };
        for i in 0..WAVE_BARS {
            let target = bar_height(self.levels[i]);
            let b = self.bars[i];
            if (b.to - target).abs() > f32::EPSILON {
                let cur = Self::eval(&b, now);
                self.bars[i] = Bar { from: cur, to: target, start: now };
            }
        }
        true
    }

    fn eval(b: &Bar, t: f64) -> f32 {
        let p = ((t - b.start) / WAVE_BAR_MS).clamp(0.0, 1.0) as f32;
        b.from + (b.to - b.from) * TW_EASE_OUT.ease(p)
    }

    pub fn height(&self, i: usize, t: f64) -> f32 {
        Self::eval(&self.bars[i], t)
    }

    pub fn heights(&self, t: f64) -> [f32; WAVE_BARS] {
        let mut out = [0.0; WAVE_BARS];
        for (i, o) in out.iter_mut().enumerate() {
            *o = self.height(i, t);
        }
        out
    }

    /// Any bar still easing at `t`.
    pub fn is_easing(&self, t: f64) -> bool {
        self.bars.iter().any(|b| t < b.start + WAVE_BAR_MS)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn level_mapping_follows_spec() {
        assert_eq!(bar_height(0.0), 4.0);
        assert_eq!(bar_height(1.0), 22.0);
        // rms 0.05 -> (0.4)^0.75 * 18 + 4
        let expect = 4.0 + (0.4f32).powf(0.75) * 18.0;
        assert!((bar_height(0.05) - expect).abs() < 1e-4);
        assert_eq!(bar_height(-1.0), 4.0);
    }

    #[test]
    fn waveform_samples_every_80ms_and_shifts() {
        let mut w = Waveform::new();
        assert!(w.tick(0.0, 0.1));
        assert!(!w.tick(40.0, 0.5));
        assert!(!w.tick(79.0, 0.5));
        assert!(w.tick(80.0, 0.0));
        // newest sample enters on the right and eases over 75 ms
        let mut w = Waveform::new();
        w.tick(0.0, 0.2);
        let target = bar_height(0.2);
        assert_eq!(w.height(WAVE_BARS - 1, 0.0), 4.0);
        assert!((w.height(WAVE_BARS - 1, 100.0) - target).abs() < 1e-4);
        let mid = w.height(WAVE_BARS - 1, 37.5);
        assert!(mid > 4.0 && mid < target);
        // next sample: the loud bar moves one slot left
        w.tick(80.0, 0.0);
        assert!((w.height(WAVE_BARS - 2, 200.0) - target).abs() < 1e-4);
        assert!((w.height(WAVE_BARS - 1, 200.0) - 4.0).abs() < 1e-4);
    }
}
