//! Daemon state machine + message loop: wires the component modules into the application.
//!
//! - [`host`]: hidden message window, tray, hotkey, config reload, IPC, message loop
//! - [`machine`]: pure Idle -> Arming -> Recording -> Thinking state machine (Tap / Hold / Esc)
//! - [`dictation`]: one dictation end to end (capture target, record, stop + VAD, worker thread:
//!   STT -> cleanup -> paste -> history)
//! - [`pipeline`]: the text stages shared with `--transcribe-file`
//! - [`latency`]: per-dictation latency samples (ring buffer + log line)
//! - [`settings`]: the Settings child process
//! - [`cli_cmds`]: `--transcribe-file` and `--bench`
//!
//! States: Idle -> (hotkey down) Arming [Hold mode, 150 ms] -> Recording -> (stop) Thinking
//!         -> Pasting -> Idle;  Esc/cancel from Recording/Thinking -> Idle; errors -> pill error card.
//! On hotkey down: capture foreground Target + app icon, show pill (Preparing), stt::prewarm,
//! start audio::Recorder (+ streaming session when configured), play start cue.
//! On stop: stop recorder, VAD trim, worker thread: stt::transcribe (or stream finish) ->
//! cleanup::process_pre_llm -> optional llm::cleanup -> dictionary::expand_snippets ->
//! paste::paste_text -> history::append. All latencies logged with timing::now_ms().
//! (Paste runs on the worker so a slow modifier-release wait never freezes the pill.)
pub mod cli_cmds;
mod dictation;
mod host;
pub mod latency;
pub mod machine;
pub mod pipeline;
pub mod settings;

pub fn run(hidden: bool) -> i32 {
    host::run(hidden)
}

pub fn cli_transcribe_file(args: &[String]) -> i32 {
    cli_cmds::transcribe_file(args)
}

pub fn cli_bench(args: &[String]) -> i32 {
    cli_cmds::bench(args)
}
