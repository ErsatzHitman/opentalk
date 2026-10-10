//! Timing diagnostics for one recording. The previous rounds fixed lag that could not be measured on
//! the development machines (no DWM under Wine), so the pill now measures itself while it records and
//! writes one summary line to the log when the recording ends: frame and tick intervals, how long a
//! tick waited for the UI thread, `UpdateLayeredWindow` time, `DwmFlush` blocking and guard trips,
//! `BitBlt` time and counts. Collecting costs a few `Instant` reads per frame and a few KB, and only
//! while the pill is in the Recording mode; nothing runs while idle.

use std::fmt;

/// Most samples kept for the percentile (the newest ones); mean, max and count cover everything.
const KEEP: usize = 2048;

/// A series of non-negative measurements (milliseconds).
#[derive(Clone, Debug, Default)]
pub struct Series {
    recent: Vec<f32>,
    next: usize,
    n: u64,
    sum: f64,
    max: f32,
}

/// What a [`Series`] reports.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Summary {
    pub n: u64,
    pub mean: f32,
    pub p95: f32,
    pub max: f32,
}

impl Series {
    pub fn push(&mut self, ms: f32) {
        if !ms.is_finite() || ms < 0.0 {
            return;
        }
        self.n += 1;
        self.sum += ms as f64;
        self.max = self.max.max(ms);
        if self.recent.len() < KEEP {
            self.recent.push(ms);
        } else {
            self.recent[self.next] = ms;
            self.next = (self.next + 1) % KEEP;
        }
    }

    pub fn clear(&mut self) {
        self.recent.clear();
        self.next = 0;
        self.n = 0;
        self.sum = 0.0;
        self.max = 0.0;
    }

    pub fn summary(&self) -> Summary {
        if self.n == 0 {
            return Summary::default();
        }
        let mut v = self.recent.clone();
        v.sort_by(f32::total_cmp);
        let p95 = v[((v.len() - 1) * 95).div_ceil(100)];
        Summary { n: self.n, mean: (self.sum / self.n as f64) as f32, p95, max: self.max }
    }
}

impl fmt::Display for Summary {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.n == 0 {
            write!(f, "n=0")
        } else {
            write!(f, "n={} mean {:.2} p95 {:.2} max {:.2} ms", self.n, self.mean, self.p95, self.max)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_series_reports_nothing() {
        let s = Series::default();
        assert_eq!(s.summary(), Summary::default());
        assert_eq!(format!("{}", s.summary()), "n=0");
    }

    #[test]
    fn summary_has_mean_p95_and_max() {
        let mut s = Series::default();
        for i in 1..=100 {
            s.push(i as f32);
        }
        let m = s.summary();
        assert_eq!(m.n, 100);
        assert!((m.mean - 50.5).abs() < 1e-3);
        assert!((m.p95 - 96.0).abs() <= 1.0, "p95 {}", m.p95);
        assert_eq!(m.max, 100.0);
    }

    #[test]
    fn garbage_is_ignored_and_memory_is_bounded() {
        let mut s = Series::default();
        s.push(f32::NAN);
        s.push(-1.0);
        s.push(f32::INFINITY);
        assert_eq!(s.summary().n, 0);
        for i in 0..10_000 {
            s.push((i % 20) as f32);
        }
        assert_eq!(s.recent.len(), KEEP);
        let m = s.summary();
        assert_eq!(m.n, 10_000);
        assert_eq!(m.max, 19.0);
        s.clear();
        assert_eq!(s.summary(), Summary::default());
    }
}
