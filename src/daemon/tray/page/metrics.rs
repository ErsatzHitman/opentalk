//! Text measuring for the page renderer, behind a trait so layout, wrapping and the editor are
//! unit-tested without fonts. All sizes are logical px at 1x; the real implementation measures at
//! the popup's device scale and divides back (so a page lays out identically at every DPI).

use crate::daemon::pill::kit::{Font, text_measure, text_prefix_widths};

/// Measures text at a logical font size and weight.
pub trait TextMetrics {
    /// Advance width of `s`.
    fn width(&self, s: &str, px: f32, weight: i32) -> f32;
    /// Cumulative advance before each char of `s`: `chars + 1` entries, `0.0` first, the width last.
    fn prefix(&self, s: &str, px: f32, weight: i32) -> Vec<f32>;
}

/// The GDI (Segoe UI) metrics of the live popup at a device scale.
pub struct GdiMetrics {
    pub scale: f32,
}

impl TextMetrics for GdiMetrics {
    fn width(&self, s: &str, px: f32, weight: i32) -> f32 {
        text_measure(s, Font::new(px, self.scale, weight)) / self.scale
    }

    fn prefix(&self, s: &str, px: f32, weight: i32) -> Vec<f32> {
        let mut v = text_prefix_widths(s, Font::new(px, self.scale, weight));
        v.iter_mut().for_each(|x| *x /= self.scale);
        v
    }
}

/// Deterministic metrics: every char is `0.52 * px` wide (every weight). Used by the unit tests and
/// by `--no-text` renders, so structural goldens do not depend on an installed font.
pub struct FixedMetrics;

impl FixedMetrics {
    pub const EM: f32 = 0.52;
}

impl TextMetrics for FixedMetrics {
    fn width(&self, s: &str, px: f32, _weight: i32) -> f32 {
        s.chars().count() as f32 * px * Self::EM
    }

    fn prefix(&self, s: &str, px: f32, _weight: i32) -> Vec<f32> {
        (0..=s.chars().count()).map(|i| i as f32 * px * Self::EM).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_metrics_are_linear() {
        let m = FixedMetrics;
        assert_eq!(m.width("abcd", 10.0, 400), 20.8);
        let p = m.prefix("abc", 10.0, 400);
        assert_eq!(p.len(), 4);
        assert_eq!(p[0], 0.0);
        assert!((p[3] - m.width("abc", 10.0, 400)).abs() < 1e-4);
        assert_eq!(m.prefix("", 10.0, 400), vec![0.0]);
    }

    #[test]
    fn gdi_metrics_scale_independent() {
        let (a, b) = (GdiMetrics { scale: 1.0 }, GdiMetrics { scale: 2.0 });
        let (w1, w2) = (a.width("Hello there", 13.0, 400), b.width("Hello there", 13.0, 400));
        assert!(w1 > 30.0 && (w1 - w2).abs() < 1.5, "{w1} {w2}");
        let p = b.prefix("Hello there", 13.0, 400);
        assert_eq!(p.len(), 12);
        assert!((p[11] - w2).abs() < 1.5);
    }
}
