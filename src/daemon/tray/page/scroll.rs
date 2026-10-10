//! Scrolling state of a page body: offset clamp, wheel steps (48 px per 120 delta), PgUp / PgDn,
//! Home / End, scroll-into-view of the focused item, and the thin scrollbar (thumb geometry and
//! drag). Pure; the unit is logical px.

use crate::daemon::tray::popup::R;

/// Logical px scrolled per wheel notch (`WHEEL_DELTA` = 120).
pub const WHEEL_PX: f32 = 48.0;
pub const WHEEL_DELTA: f32 = 120.0;
/// Width of the scrollbar thumb and of its hit track.
pub const BAR_W: f32 = 6.0;
pub const BAR_HIT_W: f32 = 14.0;
pub const THUMB_MIN: f32 = 28.0;
/// Margin kept around an item scrolled into view.
pub const VIEW_MARGIN: f32 = 8.0;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Scroll {
    pub offset: f32,
    pub view_h: f32,
    pub content_h: f32,
    /// Grab offset (px from the thumb top) while the thumb is dragged.
    drag: Option<f32>,
}

impl Scroll {
    pub fn new(view_h: f32) -> Scroll {
        Scroll { offset: 0.0, view_h, content_h: 0.0, drag: None }
    }

    /// Largest offset.
    pub fn max(&self) -> f32 {
        (self.content_h - self.view_h).max(0.0)
    }

    pub fn scrollable(&self) -> bool {
        self.max() > 0.5
    }

    fn set(&mut self, v: f32) -> bool {
        let v = v.clamp(0.0, self.max());
        let changed = (v - self.offset).abs() > 0.001;
        self.offset = v;
        changed
    }

    /// New content or viewport size; the offset is clamped. True when the offset moved.
    pub fn resize(&mut self, view_h: f32, content_h: f32) -> bool {
        self.view_h = view_h;
        self.content_h = content_h;
        self.set(self.offset)
    }

    /// Wheel by `delta` (positive = away from the user = up, like `WM_MOUSEWHEEL`).
    pub fn wheel(&mut self, delta: i32) -> bool {
        self.set(self.offset - delta as f32 / WHEEL_DELTA * WHEEL_PX)
    }

    pub fn by(&mut self, dy: f32) -> bool {
        self.set(self.offset + dy)
    }

    /// PgDn (`forward`) / PgUp: one viewport minus a line of overlap.
    pub fn page(&mut self, forward: bool) -> bool {
        let step = (self.view_h - 24.0).max(24.0);
        self.by(if forward { step } else { -step })
    }

    pub fn home(&mut self) -> bool {
        self.set(0.0)
    }

    pub fn end(&mut self) -> bool {
        self.set(self.max())
    }

    /// Scroll the least amount that shows `[top, bottom]` (content space) with a margin. An item
    /// taller than the viewport shows its top.
    pub fn reveal(&mut self, top: f32, bottom: f32) -> bool {
        let (t, b) = (top - VIEW_MARGIN, bottom + VIEW_MARGIN);
        if b - t >= self.view_h || t < self.offset {
            self.set(t)
        } else if b > self.offset + self.view_h {
            self.set(b - self.view_h)
        } else {
            false
        }
    }

    // -----------------------------------------------------------------------------------------
    // Scrollbar
    // -----------------------------------------------------------------------------------------

    /// Thumb `(top, len)` in viewport-local px (`0` = viewport top), `None` when nothing scrolls.
    pub fn thumb(&self) -> Option<(f32, f32)> {
        if !self.scrollable() || self.view_h <= 0.0 {
            return None;
        }
        let len = (self.view_h * self.view_h / self.content_h).clamp(THUMB_MIN.min(self.view_h), self.view_h);
        let top = (self.view_h - len) * self.offset / self.max();
        Some((top, len))
    }

    /// The thumb as a rect in viewport-local coordinates for a bar whose right edge is at `right`.
    pub fn thumb_rect(&self, right: f32) -> Option<R> {
        self.thumb().map(|(t, l)| R::new(right - BAR_W, t, right, t + l))
    }

    /// Press at viewport-local `y` on the bar: grab the thumb, or page toward the click on the
    /// track. True when the offset moved or a drag started.
    pub fn bar_press(&mut self, y: f32) -> bool {
        let Some((top, len)) = self.thumb() else { return false };
        if y >= top && y < top + len {
            self.drag = Some(y - top);
            true
        } else {
            self.page(y > top)
        }
    }

    /// Pointer moved to viewport-local `y` while dragging; true when the offset moved.
    pub fn bar_drag(&mut self, y: f32) -> bool {
        let (Some(grab), Some((_, len))) = (self.drag, self.thumb()) else { return false };
        let room = (self.view_h - len).max(1.0);
        let top = (y - grab).clamp(0.0, room);
        self.set(top / room * self.max())
    }

    pub fn dragging(&self) -> bool {
        self.drag.is_some()
    }

    pub fn bar_release(&mut self) {
        self.drag = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sc(view: f32, content: f32) -> Scroll {
        let mut s = Scroll::new(view);
        s.resize(view, content);
        s
    }

    #[test]
    fn offset_clamps_to_the_content() {
        let mut s = sc(400.0, 1000.0);
        assert_eq!(s.max(), 600.0);
        assert!(s.by(5000.0));
        assert_eq!(s.offset, 600.0);
        assert!(!s.by(10.0), "already at the end");
        assert!(s.by(-5000.0));
        assert_eq!(s.offset, 0.0);
        // Content shorter than the viewport never scrolls.
        let mut short = sc(400.0, 120.0);
        assert!(!short.scrollable());
        assert!(!short.by(50.0) && !short.end() && !short.wheel(-120));
        assert_eq!(short.offset, 0.0);
        assert!(short.thumb().is_none());
    }

    #[test]
    fn wheel_steps_48_px_per_notch_and_follows_the_delta() {
        let mut s = sc(400.0, 2000.0);
        s.wheel(-120);
        assert_eq!(s.offset, 48.0, "wheel toward the user scrolls down");
        s.wheel(-240);
        assert_eq!(s.offset, 144.0);
        s.wheel(120);
        assert_eq!(s.offset, 96.0);
        s.wheel(-60);
        assert_eq!(s.offset, 120.0, "precision wheels scroll proportionally");
        s.wheel(100000);
        assert_eq!(s.offset, 0.0);
    }

    #[test]
    fn paging_and_home_end() {
        let mut s = sc(300.0, 1000.0);
        assert!(s.page(true));
        assert_eq!(s.offset, 276.0);
        s.page(true);
        s.page(true);
        s.page(true);
        assert_eq!(s.offset, 700.0, "clamped at the end");
        assert!(s.page(false));
        assert_eq!(s.offset, 424.0);
        assert!(s.home());
        assert_eq!(s.offset, 0.0);
        assert!(s.end());
        assert_eq!(s.offset, 700.0);
        assert!(!s.end());
    }

    #[test]
    fn reveal_moves_the_least_and_keeps_a_margin() {
        let mut s = sc(300.0, 2000.0);
        assert!(!s.reveal(100.0, 140.0), "already visible");
        assert_eq!(s.offset, 0.0);
        // Below: its bottom lands at the viewport bottom minus the margin.
        assert!(s.reveal(500.0, 544.0));
        assert_eq!(s.offset, 544.0 + VIEW_MARGIN - 300.0);
        // Above: its top lands at the viewport top plus the margin.
        s.offset = 600.0;
        assert!(s.reveal(550.0, 590.0));
        assert_eq!(s.offset, 550.0 - VIEW_MARGIN);
        // Partly hidden at the bottom edge.
        s.offset = 100.0;
        assert!(s.reveal(380.0, 410.0));
        assert_eq!(s.offset, 410.0 + VIEW_MARGIN - 300.0);
        // Something taller than the viewport shows its top.
        s.offset = 0.0;
        assert!(s.reveal(900.0, 1400.0));
        assert_eq!(s.offset, 900.0 - VIEW_MARGIN);
        // Clamped at both ends.
        let mut e = sc(300.0, 320.0);
        e.reveal(310.0, 318.0);
        assert_eq!(e.offset, 20.0);
        e.reveal(0.0, 10.0);
        assert_eq!(e.offset, 0.0);
    }

    #[test]
    fn resize_reclamps() {
        let mut s = sc(300.0, 1000.0);
        s.end();
        assert_eq!(s.offset, 700.0);
        assert!(s.resize(300.0, 500.0));
        assert_eq!(s.offset, 200.0);
        assert!(!s.resize(300.0, 500.0));
        assert!(s.resize(600.0, 500.0));
        assert_eq!(s.offset, 0.0);
    }

    #[test]
    fn thumb_geometry() {
        let mut s = sc(400.0, 800.0);
        let (t0, len) = s.thumb().unwrap();
        assert_eq!(t0, 0.0);
        assert_eq!(len, 200.0);
        s.end();
        let (t1, _) = s.thumb().unwrap();
        assert_eq!(t1, 200.0, "the thumb ends at the bottom");
        s.offset = 200.0;
        assert_eq!(s.thumb().unwrap().0, 100.0, "halfway scroll = halfway thumb");
        // A very long page keeps a usable thumb.
        let long = sc(400.0, 400000.0);
        assert_eq!(long.thumb().unwrap().1, THUMB_MIN);
        let r = s.thumb_rect(380.0).unwrap();
        assert_eq!((r.l, r.r, r.w()), (380.0 - BAR_W, 380.0, BAR_W));
    }

    #[test]
    fn dragging_the_thumb_maps_pixels_to_offset() {
        let mut s = sc(400.0, 800.0);
        assert!(s.bar_press(50.0), "grab inside the thumb");
        assert!(s.dragging());
        assert_eq!(s.offset, 0.0);
        // Thumb is 200 long over a 400 view: 200 px of travel for 400 px of content.
        assert!(s.bar_drag(150.0));
        assert_eq!(s.offset, 200.0, "100 px of thumb travel = 200 px of content");
        assert!(s.bar_drag(1000.0));
        assert_eq!(s.offset, 400.0, "clamped at the end");
        assert!(s.bar_drag(-1000.0));
        assert_eq!(s.offset, 0.0);
        s.bar_release();
        assert!(!s.dragging());
        assert!(!s.bar_drag(100.0), "no drag, no scroll");
    }

    #[test]
    fn clicking_the_track_pages_toward_the_click() {
        let mut s = sc(400.0, 2000.0);
        assert!(s.bar_press(390.0), "below the thumb: page down");
        assert_eq!(s.offset, 376.0);
        assert!(!s.dragging());
        s.offset = 1000.0;
        assert!(s.bar_press(2.0), "above the thumb: page up");
        assert_eq!(s.offset, 624.0);
        // Nothing to scroll: presses do nothing.
        let mut short = sc(400.0, 100.0);
        assert!(!short.bar_press(10.0));
    }
}
