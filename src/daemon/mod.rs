//! Background daemon: tray, floating pill, global hotkey, capture, transcription, paste.
//! Threads: main (message loop + pill + tray + state machine), hook, capture, anim, workers.
pub mod app;
pub mod appicon;
pub mod audio;
pub mod cli;
pub mod foreground;
pub mod hotkey;
pub mod media;
pub mod muteaudio;
pub mod paste;
pub mod pill;
pub mod sound;
pub mod tray;

/// Entry point for daemon mode. Returns the process exit code.
pub fn run(hidden: bool) -> i32 {
    run_with(hidden, None)
}

/// Daemon entry with a route to open once the tray icon exists (`--settings [page]` when no
/// daemon is running). `hidden` (launch at login) never opens a view. `None` = the normal
/// start: first run opens Settings > Speech-to-Text, otherwise nothing.
///
/// If another daemon already runs, this asks it to open `initial` (Home when `None`, nothing
/// when `hidden`) and exits.
pub fn run_with(hidden: bool, initial: Option<crate::core::nav::Route>) -> i32 {
    app::run(hidden, initial)
}
