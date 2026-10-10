//! The floating recording indicator — exact reproduction of OpenWhispr's VoicePill
//! (docs/SPEC.md "Pill") + the Spokenly-style app-icon bubble.
//!
//! Window: layered 208x120 (DPI-scaled), WS_EX_LAYERED|TOPMOST|TOOLWINDOW|NOACTIVATE, rendered with
//! a pure software renderer into a premultiplied BGRA DIB and pushed with UpdateLayeredWindow.
//! Animation frames only while something animates (own high-res timer thread posts ticks to the
//! pill window). Transparent pixels pass clicks through.
//!
//! Layout of this module:
//! - `model`   — `RenderState`: all visual state as CSS-transition-style tracks (pure, testable)
//! - `render`  — `render(state, t_ms) -> Pixmap`, no Win32 (deterministic goldens)
//! - `raster`  — SDF rasteriser (capsules, rings, soft shadows)
//! - `glow`    — the Signal glow (analytic conic ring + pre-blurred halo/comet sprites)
//! - `text`    — GDI `DrawTextW` into grayscale masks
//! - `card`    — the dictation error card
//! - `anim`    — easing, tracks, waveform envelope + delay line, looping keyframes
//! - `diag`    — per-recording timing diagnostics (one log line)
//! - `theme`   — design tokens and system theme lookup
//! - `window`  — Win32 layer: layered window, DPI, hit-testing, drag, menu, animation timer
//! - `preview` — `--render-pill` CLI and golden scenarios
use super::appicon::AppIcon;
use crate::core::config::{PanelPosition, Theme};
use std::sync::Arc;
use std::sync::atomic::AtomicU32;

#[cfg(test)]
mod allocs;
mod anim;
mod capture;
mod card;
mod diag;
mod glass;
mod glow;
mod model;
mod pacer;
mod preview;
mod raster;
mod render;
mod text;
mod theme;
mod window;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PillMode {
    /// 40x40 circle with logo (or hidden when auto-hide is on).
    Idle,
    /// Mic warming up: circle, logo pulsing.
    Preparing,
    /// Entrance sequence then 98x36 capsule with live waveform.
    Recording,
    /// Transcribing: 40x40 circle + signal glow.
    Thinking,
    /// Mic unavailable/reconnecting.
    Unavailable,
    /// Window hidden entirely.
    Hidden,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PillEvent {
    /// Pill clicked (toggle listening) — not emitted after a drag.
    Clicked,
    CancelClicked,
    ErrorRetry,
    ErrorDismissed,
    /// Right-click menu choices.
    MenuToggleListening,
    MenuHideForNow,
    MenuOpenSettings,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PillOptions {
    pub position: PanelPosition,
    pub auto_hide: bool,
    pub show_app_bubble: bool,
    pub theme: Theme,
    /// Tooltip text in idle/hover, e.g. "Ctrl + Win".
    pub hotkey_label: String,
}

pub struct Pill {
    win: window::Window,
}

impl Pill {
    /// Create the window (hidden or idle per options). `level` is the live mic display level, 0..1 and
    /// exactly 0 in a silent room (f32 bits; see `audio::meter`).
    /// `on_event` runs on the UI thread from the window procedure.
    pub fn create(level: Arc<AtomicU32>, opts: PillOptions, on_event: Box<dyn FnMut(PillEvent)>) -> Result<Pill, String> {
        let win = window::Window::create(level, opts, on_event)?;
        Ok(Pill { win })
    }

    pub fn set_mode(&mut self, mode: PillMode) {
        self.win.with(|i| i.set_mode(mode));
    }

    /// Active-app bubble content (None hides the bubble).
    pub fn set_app(&mut self, app: Option<AppIcon>) {
        self.win.with(|i| {
            let bubble = app.and_then(|a| model::AppBubble::new(&a.name, &a.rgba, a.size, i.st.scale));
            i.set_app(bubble);
        });
    }

    /// Show the error card (auto-dismiss 3/4/5 s by length). `retry` adds a Retry button.
    pub fn show_error(&mut self, title: &str, description: &str, retry: bool) {
        self.win.with(|i| i.show_error(title, description, retry));
    }

    pub fn set_options(&mut self, opts: PillOptions) {
        self.win.with(|i| i.set_options(opts));
    }

    /// Move to the display under the cursor (called at every dictation start).
    pub fn follow_cursor_display(&mut self) {
        self.win.with(|i| i.follow_cursor_display());
    }
}

/// `--render-pill <state> <t_ms> <out.png> [--dark] [--scale s] [--app-icon path] [--level 0..1]`
/// States: idle, hover, preparing, recording, thinking, unavailable, cancel, error, tooltip.
pub fn cli_render(args: &[String]) -> i32 {
    preview::cli_render(args)
}

/// The software-render helpers the tray popup (`daemon::tray`) shares with the pill, so it does not
/// duplicate the rasteriser, the GDI text path or the layered-window bitmap.
pub(crate) mod kit {
    pub(crate) use super::card::draw_retry_icon;
    pub(crate) use super::raster::{Canvas, DevRect, Rect, sd_round_box, sd_segment};
    pub(crate) use super::render::draw_logo;
    pub(crate) use super::text::{Font, content_height, draw as draw_text, mask as text_mask, measure as text_measure, prefix_widths as text_prefix_widths};
    pub(crate) use super::theme::{Palette, Rgba, resolve_dark};
    pub(crate) use super::window::{Dib, scale_from_dpi};
}
