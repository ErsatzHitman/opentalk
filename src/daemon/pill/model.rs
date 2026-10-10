//! `RenderState`: the pill's complete visual model. Every animated quantity is a [`Track`] with
//! CSS-transition semantics, so `geometry(t)` / `render(state, t)` are pure functions of time.
//! Mutators take the current time (`now`, ms on any monotonic clock).

use super::PillMode;
use super::anim::*;
use super::card::ErrorCard;
use super::glass::{self, Backdrop};
use super::raster::{Rect, sd_capsule};
use super::theme::{Palette, Rgba};
use crate::core::config::PanelPosition;
use std::sync::Arc;
use tiny_skia::Pixmap;

pub const WIN_W: f32 = 208.0;
pub const WIN_H: f32 = 120.0;
/// Distance of the pill group from the window's docked edge/bottom.
pub const DOCK: f32 = 12.0;
pub const IDLE_SIZE: (f32, f32) = (40.0, 40.0);
pub const REC_SIZE: (f32, f32) = (98.0, 36.0);
/// Gap between the pill and its detached cancel button (the app bubble's gap on the other side).
pub const CANCEL_GAP: f32 = 8.0;
/// The button is clickable once it has emerged this far (`cancel_t`).
pub const CANCEL_LIVE: f32 = 0.8;
/// Size the button grows from as it slides out of the pill (fraction of its final diameter).
pub const CANCEL_FROM: f32 = 0.6;
pub const BUBBLE_GAP: f32 = 8.0;
/// Width of the layered window for the two non-right panel positions: bubble 36 + 8 + capsule 98 + 8 +
/// cancel 36 plus the 12 px glow margin on both sides does not fit 208 px there. The window is
/// transparent and click-through outside the surfaces, so extra width costs nothing visible; it grows
/// on the side away from the screen edge, so the pill stays where it was.
pub const WIN_W_LEFT: f32 = 212.0;
pub const WIN_W_CENTER: f32 = 264.0;
pub const ICON_PX: f32 = 22.0;
/// Cross-fade between two app icons when the foreground app changes during a dictation.
pub const ICON_SWAP_MS: f64 = 180.0;
pub const LOGO_SLOT: f32 = 22.0;
pub const LANE_W: f32 = 52.0;
pub const LANE_H: f32 = 24.0;
pub const TOOLTIP_GAP: f32 = 8.0;

/// Pill chrome state (`STATE_APPEARANCE`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Look {
    Idle,
    Hover,
    Recording,
    Processing,
    Thinking,
    Unavailable,
}

/// `box-shadow` layer (black).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Shadow {
    pub dy: f32,
    pub blur: f32,
    pub spread: f32,
    pub alpha: f32,
}

impl Shadow {
    pub fn lerp(self, to: Shadow, t: f32) -> Shadow {
        let l = |a: f32, b: f32| a + (b - a) * t;
        Shadow { dy: l(self.dy, to.dy), blur: l(self.blur, to.blur), spread: l(self.spread, to.spread), alpha: l(self.alpha, to.alpha) }
    }
}

/// The glass surface's state-dependent look: the drop shadow, how far the tint is lifted (hover) and
/// the border strength (the `/60` processing states). The material itself lives in `glass.rs`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Appearance {
    pub shadow: Shadow,
    /// 0 at rest, 1 hovered (`liquid-glass--hover`: tint 6 % -> 11 %).
    pub lift: f32,
    /// Border alpha multiplier (1, or 0.6 for processing / thinking / unavailable).
    pub edge: f32,
}

impl Appearance {
    pub fn lerp(self, to: Appearance, t: f32) -> Appearance {
        let l = |a: f32, b: f32| a + (b - a) * t;
        Appearance { shadow: self.shadow.lerp(to.shadow, t), lift: l(self.lift, to.lift), edge: l(self.edge, to.edge) }
    }
}

/// `STATE_APPEARANCE` on the glass material: hover lifts the tint and the shadow, the processing
/// states fade the border to 60 %.
pub fn appearance(look: Look, pal: &Palette) -> Appearance {
    let tk = glass::tokens(pal.dark);
    match look {
        Look::Idle | Look::Recording => Appearance { shadow: tk.shadow, lift: 0.0, edge: 1.0 },
        Look::Hover => Appearance { shadow: tk.shadow_lift, lift: 1.0, edge: 1.0 },
        Look::Processing | Look::Thinking | Look::Unavailable => Appearance { shadow: tk.shadow, lift: 0.0, edge: 0.6 },
    }
}

/// Colour of the logo (`currentColor`): foreground except processing (fg/70) and unavailable (muted).
pub fn logo_color(look: Look, pal: &Palette) -> Rgba {
    match look {
        Look::Processing => pal.fg.with_alpha(0.7),
        Look::Unavailable => pal.muted_fg,
        _ => pal.fg,
    }
}

fn pulses(look: Look) -> bool {
    matches!(look, Look::Processing | Look::Unavailable)
}

/// Active-app bubble content, prepared for one device scale.
#[derive(Clone)]
pub struct AppBubble {
    pub name: String,
    /// Premultiplied source at full resolution.
    src: Arc<Pixmap>,
    /// Source box-filtered to `ICON_PX * scale` device px (what is drawn).
    pub icon: Arc<Pixmap>,
}

impl std::fmt::Debug for AppBubble {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppBubble").field("name", &self.name).finish()
    }
}

impl AppBubble {
    /// From straight RGBA (`size` x `size`).
    pub fn new(name: &str, rgba: &[u8], size: u32, scale: f32) -> Option<AppBubble> {
        let need = (size as usize).checked_mul(size as usize).and_then(|n| n.checked_mul(4));
        if size == 0 || need.is_none_or(|n| rgba.len() < n) {
            return None;
        }
        let mut src = Pixmap::new(size, size)?;
        for (d, s) in src.data_mut().chunks_exact_mut(4).zip(rgba.chunks_exact(4)) {
            let a = s[3] as u32;
            d[0] = ((s[0] as u32 * a + 127) / 255) as u8;
            d[1] = ((s[1] as u32 * a + 127) / 255) as u8;
            d[2] = ((s[2] as u32 * a + 127) / 255) as u8;
            d[3] = s[3];
        }
        let src = Arc::new(src);
        let icon = Arc::new(resample(&src, (ICON_PX * scale).round().max(1.0) as u32)?);
        Some(AppBubble { name: name.to_string(), src, icon })
    }

    /// Same picture (not necessarily the same allocation): a swap to it is invisible.
    pub fn same_image(&self, other: &AppBubble) -> bool {
        Arc::ptr_eq(&self.src, &other.src) || (self.src.width() == other.src.width() && self.src.data() == other.src.data())
    }

    /// Re-prepare for a new device scale.
    pub fn rescale(&self, scale: f32) -> AppBubble {
        // an unrepresentable size keeps the icon prepared for the previous scale
        let icon = resample(&self.src, (ICON_PX * scale).round().max(1.0) as u32).map(Arc::new).unwrap_or_else(|| self.icon.clone());
        AppBubble { name: self.name.clone(), src: self.src.clone(), icon }
    }
}

/// Area-average (box) resample of a square premultiplied pixmap.
pub fn resample(src: &Pixmap, out: u32) -> Option<Pixmap> {
    let n = src.width();
    let out = out.max(1);
    let mut dst = Pixmap::new(out, out)?;
    if n == out {
        dst.data_mut().copy_from_slice(src.data());
        return Some(dst);
    }
    let ratio = n as f32 / out as f32;
    let sd = src.data();
    for y in 0..out {
        for x in 0..out {
            let (x0, x1) = (x as f32 * ratio, (x + 1) as f32 * ratio);
            let (y0, y1) = (y as f32 * ratio, (y + 1) as f32 * ratio);
            let mut acc = [0.0f32; 4];
            let mut wsum = 0.0;
            if ratio <= 1.0 {
                // upscale: bilinear at the pixel centre
                let sx = ((x as f32 + 0.5) * ratio - 0.5).clamp(0.0, (n - 1) as f32);
                let sy = ((y as f32 + 0.5) * ratio - 0.5).clamp(0.0, (n - 1) as f32);
                let (ix, iy) = (sx.floor() as u32, sy.floor() as u32);
                let (fx, fy) = (sx - ix as f32, sy - iy as f32);
                for (dx, dy, w) in [(0, 0, (1.0 - fx) * (1.0 - fy)), (1, 0, fx * (1.0 - fy)), (0, 1, (1.0 - fx) * fy), (1, 1, fx * fy)] {
                    let px = (ix + dx).min(n - 1);
                    let py = (iy + dy).min(n - 1);
                    let i = ((py * n + px) * 4) as usize;
                    for c in 0..4 {
                        acc[c] += sd[i + c] as f32 * w;
                    }
                    wsum += w;
                }
            } else {
                for sy in (y0.floor() as u32)..(y1.ceil() as u32).min(n) {
                    let wy = (y1.min(sy as f32 + 1.0) - y0.max(sy as f32)).max(0.0);
                    for sx in (x0.floor() as u32)..(x1.ceil() as u32).min(n) {
                        let wx = (x1.min(sx as f32 + 1.0) - x0.max(sx as f32)).max(0.0);
                        let w = wx * wy;
                        let i = ((sy * n + sx) * 4) as usize;
                        for c in 0..4 {
                            acc[c] += sd[i + c] as f32 * w;
                        }
                        wsum += w;
                    }
                }
            }
            let o = ((y * out + x) * 4) as usize;
            if wsum > 0.0 {
                let d = dst.data_mut();
                for c in 0..4 {
                    d[o + c] = (acc[c] / wsum + 0.5).min(255.0) as u8;
                }
            }
        }
    }
    Some(dst)
}

/// A usable device scale: real DPI scales are 0.5..8, so anything outside 0.25..16 (or not finite) is a
/// failed monitor query and must not size a pixmap to zero or to gigabytes.
pub fn sane_scale(scale: f32) -> f32 {
    if scale.is_finite() { scale.clamp(0.25, 16.0) } else { 1.0 }
}

/// Pointer state derived from hit-testing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Pointer {
    /// Cursor over pill, cancel button or the gap between (`onMouseEnter` of the row).
    pub group: bool,
    /// Cursor over the pill itself (tooltip wrapper).
    pub pill: bool,
    pub bubble: bool,
    pub cancel: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hit {
    None,
    Pill,
    Cancel,
    /// Between pill and cancel button (keeps hover alive).
    Gap,
    Bubble,
    ErrorCard,
    ErrorRetry,
}

/// Where everything sits at one instant (logical px, window space).
#[derive(Clone, Copy, Debug)]
pub struct Geometry {
    pub compact: f32,
    pub pill: Rect,
    pub cancel_t: f32,
    pub bubble_t: f32,
    /// Cancel circle centre/radius (the detached button; the radius grows from `CANCEL_FROM`).
    pub cancel: (f32, f32, f32),
    /// Bubble circle centre/radius.
    pub bubble: (f32, f32, f32),
}

/// Tooltip alignment relative to its anchor (`voicePillPopoverAlign`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Align {
    Left,
    Right,
    Center,
}

pub fn align_for(pos: PanelPosition) -> Align {
    match pos {
        PanelPosition::BottomRight => Align::Right,
        PanelPosition::BottomLeft => Align::Left,
        PanelPosition::BottomCenter => Align::Center,
    }
}

pub struct RenderState {
    pub scale: f32,
    pub pal: Palette,
    pub position: PanelPosition,
    pub hotkey_label: String,
    pub show_app_bubble: bool,
    pub(crate) mode: PillMode,
    pub(crate) pointer: Pointer,
    pub(crate) look: Look,
    pub(crate) compact: Track,
    pub(crate) grad: Track,
    pub(crate) wave_live: Track,
    pub(crate) wave_rest: Track,
    pub(crate) divider: Track,
    pub(crate) glow: Track,
    pub(crate) cancel: Spring,
    pub(crate) bubble: Track,
    pub(crate) cancel_hot: Track,
    pub(crate) appearance: Blend<Appearance>,
    pub(crate) glow_since: Option<f64>,
    pub(crate) pulse_since: Option<f64>,
    /// Start of the current recording entrance (hold + expand + waveform reveal).
    pub(crate) rec_start: Option<f64>,
    pub(crate) wave: Waveform,
    pub(crate) app: Option<AppBubble>,
    /// The icon being replaced and the 0..1 progress of the cross-fade to `app` (see `set_app`).
    pub(crate) prev_icon: Option<Arc<Pixmap>>,
    pub(crate) icon_swap: Track,
    pub(crate) error: Option<ErrorCard>,
    pub(crate) dragging: bool,
    /// The frosted screen under the window; `None` renders the glass over a flat neutral backdrop.
    pub(crate) backdrop: Option<Arc<Backdrop>>,
}

fn retarget(track: &mut Track, now: f64, to: f32, dur: f64, ease: Bezier) {
    if (track.target() - to).abs() > 1e-4 {
        track.set_at(now, to, dur, ease);
    }
}

impl RenderState {
    pub fn new(pal: Palette, scale: f32, position: PanelPosition) -> RenderState {
        let look = Look::Idle;
        RenderState {
            scale: sane_scale(scale),
            pal,
            position,
            hotkey_label: String::new(),
            show_app_bubble: true,
            mode: PillMode::Idle,
            pointer: Pointer::default(),
            look,
            compact: Track::new(0.0),
            grad: Track::new(0.0),
            wave_live: Track::new(0.0),
            wave_rest: Track::new(0.0),
            divider: Track::new(0.0),
            glow: Track::new(0.0),
            cancel: Spring::new(0.0),
            bubble: Track::new(0.0),
            cancel_hot: Track::new(0.0),
            appearance: Blend::new(appearance(look, &pal)),
            glow_since: None,
            pulse_since: None,
            rec_start: None,
            wave: Waveform::new(),
            app: None,
            prev_icon: None,
            icon_swap: Track::new(1.0),
            error: None,
            dragging: false,
            backdrop: None,
        }
    }

    /// Install (or clear) the captured backdrop. Returns whether the picture changed.
    pub fn set_backdrop(&mut self, b: Option<Arc<Backdrop>>) -> bool {
        let changed = self.backdrop.as_ref().map(|b| b.id) != b.as_ref().map(|b| b.id);
        self.backdrop = b;
        changed
    }

    /// The backdrop the glass is painted over: the capture, else the neutral one for the theme.
    pub fn backdrop_or_neutral(&self) -> Arc<Backdrop> {
        self.backdrop.clone().unwrap_or_else(|| Backdrop::neutral(self.pal.dark))
    }

    pub fn mode(&self) -> PillMode {
        self.mode
    }
    pub fn pointer(&self) -> Pointer {
        self.pointer
    }
    pub fn app(&self) -> Option<&AppBubble> {
        self.app.as_ref()
    }
    pub fn error(&self) -> Option<&ErrorCard> {
        self.error.as_ref()
    }
    pub fn error_mut(&mut self) -> Option<&mut ErrorCard> {
        self.error.as_mut()
    }
    pub fn waveform(&self) -> &Waveform {
        &self.wave
    }
    pub fn look(&self) -> Look {
        self.look
    }

    /// Theme switch: snaps chrome (no transition).
    pub fn set_palette(&mut self, pal: Palette, now: f64) {
        self.pal = pal;
        let a = appearance(self.look, &pal);
        self.appearance = Blend::new(a);
        let _ = now;
    }

    /// DPI change: re-prepare the icon.
    pub fn set_scale(&mut self, scale: f32) {
        let scale = sane_scale(scale);
        if (self.scale - scale).abs() > f32::EPSILON {
            self.scale = scale;
            if let Some(a) = &self.app {
                self.app = Some(a.rescale(scale));
            }
        }
    }

    pub fn set_position(&mut self, p: PanelPosition) {
        self.position = p;
    }

    /// The footprint target (`isCompactPill`) for the current mode.
    fn compact_target(&self) -> f32 {
        if matches!(self.mode, PillMode::Recording | PillMode::Unavailable) { 1.0 } else { 0.0 }
    }

    fn desired_look(&self) -> Look {
        match self.mode {
            PillMode::Idle | PillMode::Hidden => {
                if self.pointer.group {
                    Look::Hover
                } else {
                    Look::Idle
                }
            }
            PillMode::Preparing => Look::Processing,
            PillMode::Recording => Look::Recording,
            PillMode::Thinking => Look::Thinking,
            PillMode::Unavailable => Look::Unavailable,
        }
    }

    fn cancel_wanted(&self) -> bool {
        self.pointer.group && matches!(self.mode, PillMode::Preparing | PillMode::Recording | PillMode::Thinking | PillMode::Unavailable)
    }

    fn bubble_wanted(&self) -> bool {
        self.app.is_some() && self.show_app_bubble && matches!(self.mode, PillMode::Recording | PillMode::Thinking | PillMode::Unavailable)
    }

    /// Time at which the waveform layer may become visible.
    fn wave_visible_at(&self, now: f64) -> f64 {
        match self.rec_start {
            Some(s) => (s + WAVEFORM_AT_MS).max(now),
            None => now,
        }
    }

    /// Re-derive every track from (mode, pointer, app). Idempotent.
    fn sync(&mut self, now: f64, entering_recording: bool) {
        let pal = self.pal;
        // chrome
        let look = self.desired_look();
        if look != self.look {
            let cur = self.appearance_at(now);
            self.appearance.retarget(cur, appearance(look, &pal), now, CHROME_MS, CSS_EASE_OUT);
            if pulses(look) && !pulses(self.look) {
                self.pulse_since = Some(now);
            }
            if !pulses(look) {
                self.pulse_since = None;
            }
            self.look = look;
        }
        // footprint
        if entering_recording {
            retarget(&mut self.compact, now, 0.0, EXPANSION_MS, HOUSE);
            self.compact.set_at(now + ENTRANCE_HOLD_MS, 1.0, EXPANSION_MS, HOUSE);
        } else {
            let target = self.compact_target();
            retarget(&mut self.compact, now, target, EXPANSION_MS, HOUSE);
        }
        // waveform / divider layers
        let rec = self.mode == PillMode::Recording;
        let unavail = self.mode == PillMode::Unavailable;
        let at = self.wave_visible_at(now);
        if entering_recording {
            retarget(&mut self.wave_live, now, 0.0, WAVEFORM_FADE_MS, TW_EASE_OUT);
            self.wave_live.set_at(now + WAVEFORM_AT_MS, 1.0, WAVEFORM_FADE_MS, TW_EASE_OUT);
        } else {
            let to = if rec { 1.0 } else { 0.0 };
            if rec && at > now {
                // entrance still pending: leave the scheduled fade in place
            } else {
                retarget(&mut self.wave_live, at, to, WAVEFORM_FADE_MS, TW_EASE_OUT);
            }
        }
        let rest_to = if unavail { 1.0 } else { 0.0 };
        retarget(&mut self.wave_rest, at, rest_to, WAVEFORM_FADE_MS, TW_EASE_OUT);
        retarget(&mut self.divider, at, rest_to, EXPANSION_MS, HOUSE);
        // glow
        let thinking = self.mode == PillMode::Thinking;
        if thinking && self.glow_since.is_none() {
            self.glow_since = Some(now);
        }
        if !thinking {
            self.glow_since = None;
        }
        retarget(&mut self.glow, now, if thinking { 1.0 } else { 0.0 }, CHROME_MS, CSS_EASE_OUT);
        // hover gradient
        let grad_to = if matches!(self.mode, PillMode::Idle | PillMode::Hidden) && self.pointer.group { 0.72 } else { 0.0 };
        retarget(&mut self.grad, now, grad_to, 200.0, TW_EASE_OUT);
        // cancel button emergence
        let want = if self.cancel_wanted() { 1.0 } else { 0.0 };
        if (self.cancel.target() - want).abs() > 1e-4 {
            // pops out on an underdamped spring (overshoot + bounce), tucks back in without one
            self.cancel.set(now, want, if want > 0.5 { CANCEL_POP } else { CANCEL_TUCK });
        }
        // cancel glyph colour (hover)
        retarget(&mut self.cancel_hot, now, if self.pointer.cancel { 1.0 } else { 0.0 }, 150.0, Bezier::new(0.4, 0.0, 0.2, 1.0));
        // app bubble: fades in with the expansion
        if self.bubble_wanted() {
            let at = match self.rec_start {
                Some(s) => (s + ENTRANCE_HOLD_MS).max(now),
                None => now,
            };
            if (self.bubble.target() - 1.0).abs() > 1e-4 {
                self.bubble.set_at(at, 1.0, EXPANSION_MS, HOUSE);
            }
        } else {
            retarget(&mut self.bubble, now, 0.0, EXPANSION_MS, HOUSE);
        }
    }

    pub fn appearance_at(&self, t: f64) -> Appearance {
        let p = self.appearance.progress(t);
        self.appearance.from().lerp(self.appearance.to(), p)
    }

    pub fn set_mode(&mut self, mode: PillMode, now: f64) {
        if mode == self.mode {
            return;
        }
        let prev = self.mode;
        self.mode = mode;
        if matches!(mode, PillMode::Preparing | PillMode::Recording) {
            // starting a new dictation dismisses a lingering error
            if let Some(e) = self.error.as_mut() {
                e.dismiss_now(now);
            }
        }
        let continuing = prev == PillMode::Unavailable && mode == PillMode::Recording;
        let entering = mode == PillMode::Recording && !continuing && !(prev == PillMode::Recording);
        if entering {
            self.wave.reset();
            self.rec_start = Some(now);
        } else if !matches!(mode, PillMode::Recording | PillMode::Unavailable) {
            self.rec_start = None;
        }
        let entering_compact = entering && self.compact.target() < 0.5;
        // From Preparing the circle is already there; from a compact state there is no hold.
        self.sync(now, entering_compact);
        if entering && !entering_compact {
            // already compact (e.g. coming from Unavailable): no entrance replay
            self.rec_start = None;
        }
    }

    pub fn set_pointer(&mut self, p: Pointer, now: f64) {
        if p == self.pointer {
            return;
        }
        self.pointer = p;
        self.sync(now, false);
    }

    pub fn set_dragging(&mut self, d: bool) {
        self.dragging = d;
    }

    pub fn set_app(&mut self, app: Option<AppBubble>, now: f64) {
        // A different icon replacing a visible one cross-fades; the first icon of a dictation (or a
        // re-evaluation with the same one) does not, the bubble's own fade-in covers it.
        let swap = match (&self.app, &app) {
            (Some(old), Some(new)) => !old.same_image(new) && self.bubble.value(now) > 0.02,
            _ => false,
        };
        if swap {
            // a swap interrupted by another keeps the icon that is currently on screen as the "from"
            self.prev_icon = self.app.as_ref().map(|a| a.icon.clone());
            self.icon_swap.snap(0.0);
            self.icon_swap.set_at(now, 1.0, ICON_SWAP_MS, CSS_EASE_OUT);
        } else if app.is_none() {
            self.prev_icon = None;
            self.icon_swap.snap(1.0);
        }
        self.app = app;
        self.sync(now, false);
    }

    /// Opacity weight of the current icon (1 when settled) and the icon fading out, if any.
    pub fn icon_mix(&self, t: f64) -> (f32, Option<&Arc<Pixmap>>) {
        let w = self.icon_swap.value(t).clamp(0.0, 1.0);
        (w, if w < 0.999 { self.prev_icon.as_ref() } else { None })
    }

    // --- layout --------------------------------------------------------------------------

    pub fn geometry(&self, t: f64) -> Geometry {
        let c = self.compact.value(t).clamp(0.0, 1.0);
        let w = IDLE_SIZE.0 + (REC_SIZE.0 - IDLE_SIZE.0) * c;
        let h = IDLE_SIZE.1 + (REC_SIZE.1 - IDLE_SIZE.1) * c;
        // the spring overshoots past 1 (and the button with it); everything else uses 0..1
        let cancel_e = self.cancel.value(t).clamp(0.0, 1.25);
        let cancel_t = cancel_e.min(1.0);
        let bubble_t = if self.app.is_some() { self.bubble.value(t).clamp(0.0, 1.0) } else { 0.0 };
        let slot_b = bubble_t * (h + BUBBLE_GAP);
        let ww = win_w(self.position);
        // The cancel button never moves the pill, except docked to the right screen edge: the window
        // cannot grow past the edge, so there the group (pill + button) keeps its 12 px dock instead.
        let slot_c = if self.position == PanelPosition::BottomRight { cancel_t * (CANCEL_GAP + h) } else { 0.0 };
        let x = match self.position {
            PanelPosition::BottomRight => ww - DOCK - slot_c - w,
            PanelPosition::BottomLeft => DOCK + slot_b,
            PanelPosition::BottomCenter => (ww - (slot_b + w)) * 0.5 + slot_b,
        };
        let y = WIN_H - DOCK - h;
        let pill = Rect::from_xywh(x, y, w, h);
        let cy = pill.cy();
        let bubble = (pill.l - slot_b + h * 0.5, cy, h * 0.5);
        Geometry { compact: c, pill, cancel_t, bubble_t, cancel: cancel_circle(pill, cancel_e), bubble }
    }

    /// Hit test in logical window coordinates.
    pub fn hit_test(&self, lx: f32, ly: f32, t: f64) -> Hit {
        if let Some(e) = &self.error {
            return e.hit_test(lx, ly, self.scale);
        }
        let g = self.geometry(t);
        let p = g.pill;
        if sd_capsule(lx, ly, p.cx(), p.cy(), p.w(), p.h()) <= 0.0 {
            return Hit::Pill;
        }
        if g.cancel_t > 0.0 || self.cancel.target() > 0.5 {
            let (cx, cy, r) = g.cancel;
            if g.cancel_t > 0.0 && (lx - cx).hypot(ly - cy) <= r {
                return if g.cancel_t >= CANCEL_LIVE { Hit::Cancel } else { Hit::Gap };
            }
            // the gap between pill and button, and the spot the button is heading for, count as
            // hovered while it is wanted, so crossing the gap never drops the hover; the transparent
            // corners beyond the circle do not
            let rest = cancel_circle(p, if self.cancel.target() > 0.5 { 1.0 } else { g.cancel_t });
            if (lx - rest.0).hypot(ly - rest.1) <= rest.2 {
                return Hit::Gap;
            }
            if lx >= p.r - 1.0 && lx <= rest.0 - rest.2 && (ly - p.cy()).abs() <= p.h() * 0.5 {
                return Hit::Gap;
            }
        }
        if g.bubble_t > 0.3 {
            let (bx, by, br) = g.bubble;
            if (lx - bx).hypot(ly - by) <= br {
                return Hit::Bubble;
            }
        }
        Hit::None
    }

    pub fn pointer_for(&self, hit: Hit) -> Pointer {
        Pointer {
            group: matches!(hit, Hit::Pill | Hit::Cancel | Hit::Gap),
            pill: hit == Hit::Pill,
            bubble: hit == Hit::Bubble,
            cancel: hit == Hit::Cancel,
        }
    }

    /// Cursor-over-cancel is only meaningful when interactive.
    pub fn cancel_interactive(&self, t: f64) -> bool {
        self.cancel.value(t) >= CANCEL_LIVE
    }

    // --- tooltip ---------------------------------------------------------------------------

    /// `getMicTooltip`.
    pub fn tooltip_text(&self) -> Option<String> {
        match self.mode {
            PillMode::Recording => Some("Recording...".into()),
            PillMode::Unavailable => Some("Waiting for microphone\u{2026}".into()),
            PillMode::Preparing | PillMode::Thinking => Some("Processing...".into()),
            PillMode::Idle | PillMode::Hidden => {
                if self.hotkey_label.is_empty() {
                    None
                } else {
                    Some(self.hotkey_label.clone())
                }
            }
        }
    }

    // --- ticking ---------------------------------------------------------------------------

    /// Advance sampling state. `level` is the live mic display level (0..1, exactly 0 in silence)
    /// and `peak` the highest one since the previous tick. Returns the error-expiry flag.
    pub fn tick(&mut self, now: f64, level: f32, peak: f32) -> TickOutcome {
        if self.mode == PillMode::Recording {
            self.wave.tick(now, level, peak);
        }
        let mut out = TickOutcome::default();
        if let Some(e) = self.error.as_mut() {
            if e.update(now) {
                out.error_expired = true;
            }
            if e.finished(now) {
                self.error = None;
                out.error_gone = true;
            }
        }
        out
    }

    /// [`RenderState::tick`] with the live level as its own peak (previews, tests and anything
    /// that has no capture thread behind it).
    pub fn tick_level(&mut self, now: f64, level: f32) -> TickOutcome {
        self.tick(now, level, level)
    }

    /// `true` while any frame-by-frame animation is running at `t`.
    pub fn is_animating(&self, t: f64) -> bool {
        if self.mode == PillMode::Recording {
            return true;
        }
        let tracks = [
            &self.compact,
            &self.grad,
            &self.wave_live,
            &self.wave_rest,
            &self.divider,
            &self.glow,
            &self.bubble,
            &self.cancel_hot,
            &self.icon_swap,
        ];
        if tracks.iter().any(|tr| tr.is_active(t)) || self.cancel.is_active(t) || self.appearance.is_active(t) {
            return true;
        }
        if self.pulse_since.is_some() || self.glow_since.is_some() {
            return true;
        }
        if self.wave.is_easing(t) {
            return true;
        }
        if let Some(e) = &self.error {
            return e.is_animating(t);
        }
        false
    }

    /// `true` while the pill's footprint is changing at `t` (expand, shrink, cancel bud, app bubble).
    pub fn is_resizing(&self, t: f64) -> bool {
        self.compact.is_active(t) || self.cancel.is_active(t) || self.bubble.is_active(t)
    }

    /// Should the layered window be shown at `t` (auto-hide aware)?
    pub fn wants_visible(&self, t: f64, auto_hide: bool) -> bool {
        if self.error.is_some() {
            return true;
        }
        match self.mode {
            PillMode::Hidden => false,
            PillMode::Idle if auto_hide => self.is_animating(t) || self.pointer.group,
            _ => true,
        }
    }

    /// Snap every track to its final value (for static renders and tests).
    pub fn settle(&mut self) {
        for t in [
            &mut self.compact,
            &mut self.grad,
            &mut self.wave_live,
            &mut self.wave_rest,
            &mut self.divider,
            &mut self.glow,
            &mut self.bubble,
            &mut self.cancel_hot,
            &mut self.icon_swap,
        ] {
            let v = t.target();
            t.snap(v);
        }
        let c = self.cancel.target();
        self.cancel.snap(c);
        let to = self.appearance.to();
        self.appearance = Blend::new(to);
    }

    // --- derived values used by the renderer -------------------------------------------------

    pub fn pulse_opacity_at(&self, t: f64) -> f32 {
        match self.pulse_since {
            Some(s) => pulse_opacity(t - s),
            None => 1.0,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TickOutcome {
    /// The error card's timer ran out (card now fading out).
    pub error_expired: bool,
    /// The card finished fading and was removed.
    pub error_gone: bool,
}

/// Width of the layered window for a panel position (logical px).
pub fn win_w(pos: PanelPosition) -> f32 {
    match pos {
        PanelPosition::BottomRight => WIN_W,
        PanelPosition::BottomLeft => WIN_W_LEFT,
        PanelPosition::BottomCenter => WIN_W_CENTER,
    }
}

/// The detached cancel button for an emergence `e` (0 hidden inside the pill's right end, 1 at rest):
/// a circle as tall as the pill, `CANCEL_GAP` right of it, sliding out of the right end while it grows.
/// `e` above 1 is the spring's overshoot: a little further out and a little rounder.
pub fn cancel_circle(pill: Rect, e: f32) -> (f32, f32, f32) {
    let e = e.clamp(0.0, 1.25);
    let rad = pill.h() * 0.5;
    let cx = (pill.r - rad) + e * (CANCEL_GAP + 2.0 * rad);
    (cx, pill.cy(), rad * (CANCEL_FROM + (1.0 - CANCEL_FROM) * e))
}

/// Window size in logical px for the current content.
pub fn window_size(st: &RenderState) -> (f32, f32) {
    match &st.error {
        Some(e) => {
            let l = e.layout(st.scale);
            (l.win_w, l.win_h)
        }
        None => (win_w(st.position), WIN_H),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn st() -> RenderState {
        RenderState::new(Palette::LIGHT, 1.0, PanelPosition::BottomRight)
    }

    #[test]
    fn idle_geometry_matches_spec() {
        let s = st();
        let g = s.geometry(0.0);
        // 40x40 circle, right edge 12 px from the window edge, bottom 12 px up
        assert_eq!(g.pill, Rect::from_xywh(156.0, 68.0, 40.0, 40.0));
    }

    #[test]
    fn recording_entrance_timeline() {
        let mut s = st();
        s.set_mode(PillMode::Recording, 0.0);
        // 0..260 ms: still the 40x40 circle (in recording colours)
        assert_eq!(s.geometry(0.0).pill.w(), 40.0);
        assert_eq!(s.geometry(259.0).pill.w(), 40.0);
        // 560 ms: 98x36
        let g = s.geometry(560.0);
        assert!((g.pill.w() - 98.0).abs() < 0.01 && (g.pill.h() - 36.0).abs() < 0.01, "{g:?}");
        assert!((g.pill.r - 196.0).abs() < 0.01 && (g.pill.b - 108.0).abs() < 0.01);
        // expansion is mid-way at ~290 ms (house curve is front-loaded)
        let mid = s.geometry(300.0).pill.w();
        assert!(mid > 50.0 && mid < 98.0, "{mid}");
        // waveform hidden until 660, fully shown at 860
        assert_eq!(s.wave_live.value(600.0), 0.0);
        assert!(s.wave_live.value(760.0) > 0.5 && s.wave_live.value(760.0) < 1.0);
        assert!((s.wave_live.value(860.0) - 1.0).abs() < 1e-5);
        assert_eq!(s.look(), Look::Recording);
    }

    #[test]
    fn stop_shrinks_and_lights_glow() {
        let mut s = st();
        s.set_mode(PillMode::Recording, 0.0);
        s.set_mode(PillMode::Thinking, 1000.0);
        assert!((s.geometry(1000.0).pill.w() - 98.0).abs() < 0.01);
        assert!((s.geometry(1300.0).pill.w() - 40.0).abs() < 0.01);
        assert_eq!(s.glow.value(1000.0), 0.0);
        assert!((s.glow.value(1220.0) - 1.0).abs() < 1e-5);
        assert!(s.glow_since.is_some());
        assert!(s.is_animating(1500.0));
        s.set_mode(PillMode::Idle, 3000.0);
        assert!(s.glow_since.is_none());
        assert!(!s.is_animating(3300.0));
    }

    #[test]
    fn stop_during_hold_cancels_expansion() {
        let mut s = st();
        s.set_mode(PillMode::Recording, 0.0);
        s.set_mode(PillMode::Thinking, 100.0);
        assert_eq!(s.geometry(500.0).pill.w(), 40.0);
    }

    fn hovering(pos: PanelPosition, bubble: bool) -> RenderState {
        let mut s = st();
        s.set_position(pos);
        s.set_mode(PillMode::Recording, 0.0);
        if bubble {
            s.set_app(Some(AppBubble::new("Explorer", &vec![255u8; 8 * 8 * 4], 8, 1.0).unwrap()), 0.0);
        }
        s.settle();
        s
    }

    #[test]
    fn hover_detaches_a_round_cancel_button_docked_right() {
        let mut s = hovering(PanelPosition::BottomRight, false);
        s.set_pointer(Pointer { group: true, pill: true, bubble: false, cancel: false }, 1000.0);
        // before emergence: pill at the right dock, button hidden inside its right end
        let g0 = s.geometry(1000.0);
        assert!((g0.pill.r - 196.0).abs() < 0.01 && g0.cancel_t == 0.0);
        assert_eq!(s.hit_test(g0.pill.r + 10.0, 90.0, 1000.0), Hit::Gap, "the spot the button is heading for is hovered");
        // after: a 36 px circle (the pill's height) 8 px right of the pill; the window cannot grow past the
        // screen edge, so the group (pill + 8 + 36) keeps the pill's 12 px dock and the pill slides left 44
        let g = s.geometry(1400.0);
        assert!((g.pill.r - 152.0).abs() < 0.01, "{g:?}");
        assert!((g.cancel.2 * 2.0 - 36.0).abs() < 0.01 && (g.cancel.0 - g.cancel.2 - g.pill.r - 8.0).abs() < 0.01, "{g:?}");
        assert!((g.cancel.0 + g.cancel.2 - (208.0 - 12.0)).abs() < 0.01);
        assert_eq!(g.cancel.1, g.pill.cy());
        assert_eq!(s.hit_test(g.cancel.0, 90.0, 1400.0), Hit::Cancel);
        // the gap between the surfaces keeps the hover, the transparent corners beyond the circle do not
        assert_eq!(s.hit_test(g.pill.r + 4.0, 90.0, 1400.0), Hit::Gap);
        assert_eq!(s.hit_test(g.cancel.0 + g.cancel.2 - 1.0, g.cancel.1 - g.cancel.2 + 1.0, 1400.0), Hit::None);
        assert_eq!(s.hit_test(g.pill.r + 4.0, g.pill.t - 4.0, 1400.0), Hit::None);
        assert_eq!(s.hit_test(100.0, 90.0, 1400.0), Hit::Pill);
        assert_eq!(s.hit_test(5.0, 5.0, 1400.0), Hit::None);
        // leave: the button retracts into the pill, which slides back
        s.set_pointer(Pointer::default(), 2000.0);
        let g = s.geometry(2400.0);
        assert!((g.pill.r - 196.0).abs() < 0.01 && g.cancel_t == 0.0);
    }

    #[test]
    fn cancel_button_never_moves_the_pill_off_the_right_edge() {
        for pos in [PanelPosition::BottomLeft, PanelPosition::BottomCenter] {
            for bubble in [false, true] {
                let mut s = hovering(pos, bubble);
                let before = s.geometry(1000.0);
                s.set_pointer(Pointer { group: true, pill: true, bubble: false, cancel: false }, 1000.0);
                for t in (1000..1400).step_by(20) {
                    let g = s.geometry(t as f64);
                    assert_eq!(g.pill, before.pill, "{pos:?} bubble {bubble} t {t}");
                    assert_eq!(g.bubble, before.bubble);
                }
                let g = s.geometry(1400.0);
                // the button, its 12 px glow margin and the window all fit
                let ww = win_w(pos);
                assert!(g.cancel.0 + g.cancel.2 + DOCK <= ww + 0.01, "{pos:?} bubble {bubble}: {g:?} in {ww}");
                assert!(g.bubble.0 - g.bubble.2 >= 0.0);
                assert_eq!(s.hit_test(g.cancel.0, g.cancel.1, 1400.0), Hit::Cancel);
            }
        }
    }

    #[test]
    fn cancel_is_clickable_only_once_it_has_emerged() {
        let mut s = hovering(PanelPosition::BottomLeft, false);
        s.set_pointer(Pointer { group: true, pill: true, bubble: false, cancel: false }, 1000.0);
        // sliding out: the circle is part of the hovered group but not yet a button
        let early = (4000..5120).map(|t| t as f64 * 0.25).find(|&t| s.cancel.value(t) > 0.3 && s.cancel.value(t) < 0.6).unwrap();
        let g = s.geometry(early);
        assert_eq!(s.hit_test(g.cancel.0 + g.cancel.2 - 2.0, g.cancel.1, early), Hit::Gap);
        assert!(!s.cancel_interactive(early));
        assert!(s.cancel_interactive(1300.0));
        assert_eq!(s.pointer_for(Hit::Cancel), Pointer { group: true, pill: false, bubble: false, cancel: true });
        assert!(s.pointer_for(Hit::Gap).group);
    }

    #[test]
    fn cancel_circle_follows_the_pill_height() {
        let pill = Rect::from_xywh(100.0, 70.0, 40.0, 40.0);
        let (cx, cy, r) = cancel_circle(pill, 1.0);
        assert_eq!((cx, cy, r), (140.0 + 8.0 + 20.0, 90.0, 20.0));
        let (cx, _, r) = cancel_circle(pill, 0.0);
        assert!((cx - 120.0).abs() < 1e-4 && (r - 20.0 * CANCEL_FROM).abs() < 1e-4);
    }

    #[test]
    fn window_width_per_panel_position() {
        assert_eq!(win_w(PanelPosition::BottomRight), 208.0);
        assert!(win_w(PanelPosition::BottomLeft) > 208.0 && win_w(PanelPosition::BottomCenter) > win_w(PanelPosition::BottomLeft));
        let mut s = st();
        s.set_position(PanelPosition::BottomCenter);
        assert_eq!(window_size(&s), (WIN_W_CENTER, WIN_H));
    }

    #[test]
    fn positions_per_panel_position() {
        let mut s = st();
        s.set_position(PanelPosition::BottomLeft);
        assert_eq!(s.geometry(0.0).pill.l, 12.0);
        s.set_position(PanelPosition::BottomCenter);
        let p = s.geometry(0.0).pill;
        assert!((p.cx() - win_w(PanelPosition::BottomCenter) * 0.5).abs() < 0.01);
    }

    #[test]
    fn bubble_slot_does_not_move_right_anchored_pill() {
        let mut s = st();
        let icon = AppBubble::new("Explorer", &vec![255u8; 8 * 8 * 4], 8, 1.0).unwrap();
        s.set_mode(PillMode::Recording, 0.0);
        s.set_app(Some(icon), 0.0);
        s.settle();
        let g = s.geometry(1000.0);
        assert!((g.pill.r - 196.0).abs() < 0.01);
        // bubble: diameter == pill height, 8 px gap
        assert!((g.bubble.2 * 2.0 - 36.0).abs() < 0.01);
        assert!((g.pill.l - (g.bubble.0 + g.bubble.2) - 8.0).abs() < 0.01);
    }

    fn bubble_of(name: &str, v: u8) -> AppBubble {
        AppBubble::new(name, &vec![v; 8 * 8 * 4], 8, 1.0).unwrap()
    }

    #[test]
    fn a_new_app_cross_fades_in_the_visible_bubble() {
        let mut s = st();
        s.set_mode(PillMode::Recording, 0.0);
        s.set_app(Some(bubble_of("Notepad", 100)), 0.0);
        s.settle();
        // settled: nothing is fading
        let t0 = 2000.0;
        let (w, prev) = s.icon_mix(t0);
        assert!(w == 1.0 && prev.is_none());
        s.set_app(Some(bubble_of("Chrome", 200)), t0);
        assert_eq!(s.app().unwrap().name, "Chrome");
        let (w, prev) = s.icon_mix(t0);
        assert!(w < 0.01 && prev.is_some(), "starts on the old icon");
        let (w, prev) = s.icon_mix(t0 + ICON_SWAP_MS * 0.5);
        assert!(w > 0.2 && w < 0.95 && prev.is_some(), "mid-way: {w}");
        assert!(s.icon_swap.is_active(t0 + ICON_SWAP_MS * 0.5));
        let (w, prev) = s.icon_mix(t0 + ICON_SWAP_MS + 1.0);
        assert_eq!((w, prev.is_none()), (1.0, true), "done: only the new icon");
        assert!(!s.icon_swap.is_active(t0 + ICON_SWAP_MS + 1.0));
        // the bubble itself never moves for it
        let a = s.geometry(t0).bubble;
        let b = s.geometry(t0 + ICON_SWAP_MS * 0.5).bubble;
        assert_eq!(a, b);
    }

    #[test]
    fn no_cross_fade_for_the_first_icon_the_same_picture_or_a_hidden_bubble() {
        let mut s = st();
        s.set_mode(PillMode::Recording, 0.0);
        // first icon of the dictation: the bubble's own fade-in covers it
        s.set_app(Some(bubble_of("Notepad", 100)), 0.0);
        assert!(s.prev_icon.is_none() && !s.icon_swap.is_active(0.0));
        s.settle();
        // the same picture again (a config re-evaluation, a duplicate lookup)
        s.set_app(Some(bubble_of("Notepad", 100)), 2000.0);
        assert!(s.prev_icon.is_none() && !s.icon_swap.is_active(2000.0));
        // clearing resets
        s.set_app(None, 2100.0);
        assert!(s.prev_icon.is_none());
        s.set_app(Some(bubble_of("Chrome", 200)), 2200.0);
        assert!(s.prev_icon.is_none(), "from nothing, not a swap");
        // a bubble that is not on screen yet (Preparing) swaps instantly
        let mut p = st();
        p.set_app(Some(bubble_of("Notepad", 100)), 0.0);
        p.set_app(Some(bubble_of("Chrome", 200)), 10.0);
        assert!(p.prev_icon.is_none() && !p.icon_swap.is_active(10.0));
    }

    #[test]
    fn a_swap_during_a_swap_fades_from_the_icon_on_screen() {
        let mut s = st();
        s.set_mode(PillMode::Recording, 0.0);
        s.set_app(Some(bubble_of("A", 50)), 0.0);
        s.settle();
        s.set_app(Some(bubble_of("B", 120)), 3000.0);
        let b_icon = s.app().unwrap().icon.clone();
        s.set_app(Some(bubble_of("C", 220)), 3050.0);
        assert!(Arc::ptr_eq(s.prev_icon.as_ref().unwrap(), &b_icon), "the previous icon is the one shown last");
        assert!(s.icon_mix(3050.0).0 < 0.01);
    }

    #[test]
    fn tooltip_texts() {
        let mut s = st();
        assert_eq!(s.tooltip_text(), None);
        s.hotkey_label = "Ctrl + Win".into();
        assert_eq!(s.tooltip_text().as_deref(), Some("Ctrl + Win"));
        s.set_mode(PillMode::Recording, 0.0);
        assert_eq!(s.tooltip_text().as_deref(), Some("Recording..."));
        s.set_mode(PillMode::Thinking, 1.0);
        assert_eq!(s.tooltip_text().as_deref(), Some("Processing..."));
    }

    #[test]
    fn appearance_tokens() {
        let l = appearance(Look::Idle, &Palette::LIGHT);
        let tk = glass::tokens(false);
        assert_eq!(l.shadow, tk.shadow);
        assert_eq!((l.lift, l.edge), (0.0, 1.0));
        // dark mode shadows are stronger than light mode's
        assert!(appearance(Look::Idle, &Palette::DARK).shadow.alpha > l.shadow.alpha);
        // hover lifts the tint and the shadow; recording looks like idle
        let h = appearance(Look::Hover, &Palette::LIGHT);
        assert_eq!(h.lift, 1.0);
        assert_eq!(h.shadow, tk.shadow_lift);
        assert_eq!(appearance(Look::Recording, &Palette::LIGHT), l);
        // processing / thinking / unavailable fade the border to 60 %
        for look in [Look::Processing, Look::Thinking, Look::Unavailable] {
            assert!((appearance(look, &Palette::DARK).edge - 0.6).abs() < 1e-6);
        }
        // transitions interpolate every field
        let mid = l.lerp(h, 0.5);
        assert!((mid.lift - 0.5).abs() < 1e-6 && mid.shadow.dy > l.shadow.dy && mid.shadow.dy < h.shadow.dy);
    }
}
