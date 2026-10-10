//! Background daemon: tray, floating pill, global hotkey, capture, transcription, paste.
//! Threads: main (message loop + pill + tray + state machine), hook, capture, anim, workers.
pub mod app;
pub mod appicon;
pub mod audio;
pub mod cli;
pub mod foreground;
pub mod hotkey;
pub mod media;
pub mod paste;
pub mod pill;
pub mod sound;
pub mod tray;

/// Entry point for daemon mode. Returns the process exit code.
pub fn run(hidden: bool) -> i32 {
    app::run(hidden)
}
