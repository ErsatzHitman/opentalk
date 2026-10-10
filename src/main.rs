//! OpenTalk — ultra-light Windows dictation (cloud STT only).
//!
//! One executable, several modes:
//!   opentalk.exe                      daemon (tray + pill + hotkey), single instance
//!   opentalk.exe --hidden             daemon, launched at login (never opens UI)
//!   opentalk.exe --settings [page]    open the tray popup at a page (settings[:section] | history |
//!                                     dictionary | snippets | home; default: the Settings index),
//!                                     starting the daemon if none runs
//!   opentalk.exe --transcribe-file F  headless pipeline: STT + cleanup, prints text
//!   opentalk.exe --render-pill S T O  render pill state S at t=T ms to PNG O (goldens)
//!   opentalk.exe --render-tray-popup O  render the tray popup to PNG O (screenshots, goldens)
//!   opentalk.exe --bench ...          benchmark helpers (see bench/bench.ps1)
#![cfg_attr(not(test), windows_subsystem = "windows")]
// rustc embeds its own default manifest next to ours (build.rs); GNU ld reports the pair.
#![allow(linker_messages)]

mod core;
mod daemon;
mod llm;
mod stt;
#[cfg(test)]
mod testenv;

fn main() {
    // Anchor the monotonic clock at process start so logged times measure from launch.
    core::timing::now_ms();
    let args: Vec<String> = std::env::args().skip(1).collect();
    // Helper modes log to their own file; every daemon launch (no argument, --hidden from
    // launch-at-login, --settings, or an unrecognised argument) shares `daemon.log`.
    // Not `unwrap_or`: any other first argument (`--hidden`, a typo) must still log to `daemon`.
    #[allow(clippy::manual_unwrap_or)]
    let log_name = match args.first().map(String::as_str) {
        Some(mode @ ("--transcribe-file" | "--render-pill" | "--render-tray-popup" | "--bench")) => mode,
        _ => "daemon",
    };
    core::logging::init(log_name);
    let code = match args.first().map(String::as_str) {
        // A running daemon is asked to show the page (OpenPage); otherwise this process becomes
        // the daemon and opens it once the tray icon exists. No page: the Settings index.
        Some("--settings") => {
            let route = args.get(1).map_or(core::nav::Route::SettingsIndex, |p| core::nav::Route::parse(Some(p)));
            daemon::run_with(false, Some(route))
        }
        Some("--transcribe-file") => daemon::cli::transcribe_file(&args[1..]),
        Some("--render-pill") => daemon::cli::render_pill(&args[1..]),
        Some("--render-tray-popup") => daemon::cli::render_tray_popup(&args[1..]),
        Some("--bench") => daemon::cli::bench(&args[1..]),
        _ => daemon::run(args.iter().any(|a| a == "--hidden")),
    };
    std::process::exit(code);
}
