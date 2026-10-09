//! "Pause media while dictating" (OpenWhispr pauseMediaOnDictation). TASK win-input implements:
//! pause only if a media session is actually playing (WinRT GlobalSystemMediaTransportControls
//! via the `windows` crate if practical, else skip), and resume only what we paused.

/// Pause playing media. Returns true if something was paused (and should be resumed).
pub fn pause_if_playing() -> bool {
    todo!("win-input")
}
pub fn resume() {
    todo!("win-input")
}
