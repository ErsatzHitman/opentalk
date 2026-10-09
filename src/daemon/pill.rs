//! The floating recording indicator — exact reproduction of OpenWhispr's VoicePill
//! (docs/SPEC.md "Pill") + the Spokenly-style app-icon bubble. TASK pill implements; it may split
//! into submodules under src/daemon/pill/ (render.rs, glow.rs, text.rs, window.rs, anim.rs).
//!
//! Window: layered 208x120 (DPI-scaled), WS_EX_LAYERED|TOPMOST|TOOLWINDOW|NOACTIVATE, rendered with
//! tiny-skia into a premultiplied BGRA DIB and pushed with UpdateLayeredWindow. Animation frames
//! only while something animates (own high-res timer thread posts ticks to the pill window).
//! Transparent pixels pass clicks through.
use super::appicon::AppIcon;
use crate::core::config::{PanelPosition, Theme};
use std::sync::Arc;
use std::sync::atomic::AtomicU32;

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
    _private: (),
}

impl Pill {
    /// Create the window (hidden or idle per options). `level` is the live mic RMS (f32 bits).
    /// `on_event` runs on the UI thread from the window procedure.
    pub fn create(level: Arc<AtomicU32>, opts: PillOptions, on_event: Box<dyn FnMut(PillEvent)>) -> Result<Pill, String> {
        let _ = (level, opts, on_event);
        todo!("pill")
    }
    pub fn set_mode(&mut self, mode: PillMode) {
        let _ = mode;
        todo!("pill")
    }
    pub fn mode(&self) -> PillMode {
        todo!("pill")
    }
    /// Active-app bubble content (None hides the bubble).
    pub fn set_app(&mut self, app: Option<AppIcon>) {
        let _ = app;
        todo!("pill")
    }
    /// Show the error card (auto-dismiss 3/4/5 s by length). `retry` adds a Retry button.
    pub fn show_error(&mut self, title: &str, description: &str, retry: bool) {
        let _ = (title, description, retry);
        todo!("pill")
    }
    pub fn set_options(&mut self, opts: PillOptions) {
        let _ = opts;
        todo!("pill")
    }
    /// Move to the display under the cursor (called at every dictation start).
    pub fn follow_cursor_display(&mut self) {
        todo!("pill")
    }
    pub fn hwnd(&self) -> isize {
        todo!("pill")
    }
}

/// `--render-pill <state> <t_ms> <out.png> [--dark] [--scale s] [--app-icon path] [--level 0..1]`
/// States: idle, hover, preparing, recording, thinking, unavailable, cancel, error, tooltip.
pub fn cli_render(args: &[String]) -> i32 {
    let _ = args;
    todo!("pill")
}
