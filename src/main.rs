//! OpenTalk — ultra-light Windows dictation (cloud STT only).
//!
//! One executable, several modes:
//!   opentalk.exe                      daemon (tray + pill + hotkey), single instance
//!   opentalk.exe --hidden             daemon, launched at login (never opens UI)
//!   opentalk.exe --settings [page]    egui control panel (history | dictionary | settings)
//!   opentalk.exe --transcribe-file F  headless pipeline: STT + cleanup, prints text
//!   opentalk.exe --render-pill S T O  render pill state S at t=T ms to PNG O (goldens)
//!   opentalk.exe --bench ...          benchmark helpers (see bench/bench.ps1)
#![cfg_attr(not(test), windows_subsystem = "windows")]
#![allow(dead_code)]
// rustc embeds its own default manifest next to ours (build.rs); GNU ld reports the pair.
#![allow(linker_messages)]

mod core;
mod daemon;
mod llm;
mod stt;
mod ui;

fn main() {
    // Anchor the monotonic clock at process start so logged times measure from launch.
    core::timing::now_ms();
    let args: Vec<String> = std::env::args().skip(1).collect();
    core::logging::init(args.first().map(String::as_str).unwrap_or("daemon"));
    let code = match args.first().map(String::as_str) {
        Some("--settings") => {
            ui::run(args.get(1).map(String::as_str));
            0
        }
        Some("--transcribe-file") => daemon::cli::transcribe_file(&args[1..]),
        Some("--render-pill") => daemon::cli::render_pill(&args[1..]),
        Some("--bench") => daemon::cli::bench(&args[1..]),
        _ => daemon::run(args.iter().any(|a| a == "--hidden")),
    };
    std::process::exit(code);
}
