//! Daemon state machine + message loop. TASK integration implements this after the
//! component tasks land.
//!
//! States: Idle -> (hotkey down) Arming [Hold mode, 150 ms] -> Recording -> (stop) Thinking
//!         -> Pasting -> Idle;  Esc/cancel from Recording/Thinking -> Idle; errors -> pill error card.
//! On hotkey down: capture foreground Target + app icon, show pill (Preparing), stt::prewarm,
//! start audio::Recorder (+ streaming session when configured), play start cue.
//! On stop: stop recorder, VAD trim, worker thread: stt::transcribe (or stream finish) ->
//! cleanup::process_pre_llm -> optional llm::cleanup -> dictionary::expand_snippets ->
//! paste::paste_text -> history::append. All latencies logged with timing::now_ms().

pub fn run(hidden: bool) -> i32 {
    let _ = hidden;
    todo!("integration")
}

pub fn cli_transcribe_file(args: &[String]) -> i32 {
    let _ = args;
    todo!("integration")
}

pub fn cli_bench(args: &[String]) -> i32 {
    let _ = args;
    todo!("integration")
}
