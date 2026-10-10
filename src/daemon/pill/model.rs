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
pub const CANCEL_SIZE: f32 = 28.0;
pub const CANCEL_GAP: f32 = 8.0;
pub const BUBBLE_GAP: f32 = 8.0;
pub const ICON_PX: f32 = 22.0;
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
    /// Cancel circle centre/radius.
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
    pub(crate) cancel: Track,
    pub(crate) fused: Track,
    pub(crate) bubble: Track,
    pub(crate) cancel_hot: Track,
    pub(crate) appearance: Blend<Appearance>,
    pub(crate) glow_since: Option<f64>,
    pub(crate) pulse_since: Option<f64>,
    /// Start of the current recording entrance (hold + expand + waveform reveal).
    pub(crate) rec_start: Option<f64>,
    pub(crate) wave: Waveform,
    pub(crate) app: Option<AppBubble>,
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
            cancel: Track::new(0.0),
            fused: Track::new(0.0),
            bubble: Track::new(0.0),
            cancel_hot: Track::new(0.0),
            appearance: Blend::new(appearance(look, &pal)),
            glow_since: None,
            pulse_since: None,
            rec_start: None,
            wave: Waveform::new(),
            app: None,
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
        // cancel button emergence (rAF-driven, 280 ms * |delta|, house curve)
        let want = if self.cancel_wanted() { 1.0 } else { 0.0 };
        if (self.cancel.target() - want).abs() > 1e-4 {
            let cur = self.cancel.value(now);
            let dur = CANCEL_EMERGE_MS * (want - cur).abs() as f64;
            self.cancel.set_at(now, want, dur.max(1.0), HOUSE);
        }
        // the fused skin owns the surface while the bud exists
        let fused_want = if want > 0.0 || self.cancel.value(now) > 0.0 { 1.0 } else { 0.0 };
        if fused_want > 0.5 {
            retarget(&mut self.fused, now, 1.0, CHROME_MS, CSS_EASE_OUT);
        } else {
            retarget(&mut self.fused, now, 0.0, CHROME_MS, CSS_EASE_OUT);
        }
        if want == 0.0 && self.cancel.value(now) > 0.0 {
            // release the surface when the bud has been swallowed
            let end = self.cancel.end_time();
            self.fused.set_at(end, 0.0, CHROME_MS, CSS_EASE_OUT);
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
        self.app = app;
        self.sync(now, false);
    }

    // --- layout --------------------------------------------------------------------------

    pub fn geometry(&self, t: f64) -> Geometry {
        let c = self.compact.value(t).clamp(0.0, 1.0);
        let w = IDLE_SIZE.0 + (REC_SIZE.0 - IDLE_SIZE.0) * c;
        let h = IDLE_SIZE.1 + (REC_SIZE.1 - IDLE_SIZE.1) * c;
        let cancel_t = self.cancel.value(t).clamp(0.0, 1.0);
        let bubble_t = if self.app.is_some() { self.bubble.value(t).clamp(0.0, 1.0) } else { 0.0 };
        let slot_c = cancel_t * (CANCEL_GAP + CANCEL_SIZE);
        let slot_b = bubble_t * (h + BUBBLE_GAP);
        let x = match self.position {
            PanelPosition::BottomRight => WIN_W - DOCK - slot_c - w,
            PanelPosition::BottomLeft => DOCK + slot_b,
            PanelPosition::BottomCenter => (WIN_W - (slot_b + w + slot_c)) * 0.5 + slot_b,
        };
        let y = WIN_H - DOCK - h;
        let pill = Rect::from_xywh(x, y, w, h);
        let cy = pill.cy();
        let cancel = (pill.r + slot_c - CANCEL_SIZE * 0.5, cy, CANCEL_SIZE * 0.5);
        let bubble = (pill.l - slot_b + h * 0.5, cy, h * 0.5);
        Geometry { compact: c, pill, cancel_t, bubble_t, cancel, bubble }
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
        if g.cancel_t > 0.0 {
            let (cx, cy, r) = g.cancel;
            if (lx - cx).hypot(ly - cy) <= r && g.cancel_t * (CANCEL_GAP + CANCEL_SIZE) >= CANCEL_SIZE {
                return Hit::Cancel;
            }
            // the row between pill and bud
            if lx >= p.r - 1.0 && lx <= cx + r && (ly - p.cy()).abs() <= p.h() * 0.5 {
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
        self.cancel.value(t) * (CANCEL_GAP + CANCEL_SIZE) >= CANCEL_SIZE
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

    /// Advance sampling state. `rms` is the live mic level. Returns the error-expiry flag.
    pub fn tick(&mut self, now: f64, rms: f32) -> TickOutcome {
        if self.mode == PillMode::Recording {
            self.wave.tick(now, rms);
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
            &self.cancel,
            &self.fused,
            &self.bubble,
            &self.cancel_hot,
        ];
        if tracks.iter().any(|tr| tr.is_active(t)) || self.appearance.is_active(t) {
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
            &mut self.cancel,
            &mut self.fused,
            &mut self.bubble,
            &mut self.cancel_hot,
        ] {
            let v = t.target();
            t.snap(v);
        }
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

/// Window size in logical px for the current content.
pub fn window_size(st: &RenderState) -> (f32, f32) {
    match &st.error {
        Some(e) => {
            let l = e.layout(st.scale);
            (l.win_w, l.win_h)
        }
        None => (WIN_W, WIN_H),
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

    #[test]
    fn hover_cancel_and_position_shift() {
        let mut s = st();
        s.set_mode(PillMode::Recording, 0.0);
        s.settle();
        s.set_pointer(Pointer { group: true, pill: true, bubble: false, cancel: false }, 1000.0);
        // before emergence: pill at the right dock
        assert!((s.geometry(1000.0).pill.r - 196.0).abs() < 0.01);
        // after: the group (pill + 8 + 28) stays anchored, pill slides left 36 px
        let g = s.geometry(1400.0);
        assert!((g.pill.r - 160.0).abs() < 0.01, "{g:?}");
        assert!((g.cancel.0 - 182.0).abs() < 0.01 && g.cancel.2 == 14.0);
        assert_eq!(s.hit_test(182.0, 90.0, 1400.0), Hit::Cancel);
        assert_eq!(s.hit_test(164.0, 90.0, 1400.0), Hit::Gap);
        assert_eq!(s.hit_test(100.0, 90.0, 1400.0), Hit::Pill);
        assert_eq!(s.hit_test(5.0, 5.0, 1400.0), Hit::None);
        // leave: the bud retracts
        s.set_pointer(Pointer::default(), 2000.0);
        assert!((s.geometry(2400.0).pill.r - 196.0).abs() < 0.01);
    }

    #[test]
    fn positions_per_panel_position() {
        let mut s = st();
        s.set_position(PanelPosition::BottomLeft);
        assert_eq!(s.geometry(0.0).pill.l, 12.0);
        s.set_position(PanelPosition::BottomCenter);
        let p = s.geometry(0.0).pill;
        assert!((p.cx() - 104.0).abs() < 0.01);
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
