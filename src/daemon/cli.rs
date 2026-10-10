//! Command-line helper modes (no UI). The exe is a GUI-subsystem binary, so each helper
//! first attaches to the parent console (`AttachConsole(ATTACH_PARENT_PROCESS)`) to print.

pub fn attach_console() {
    // SAFETY: plain call without preconditions; it fails harmlessly when there is no parent console.
    unsafe {
        let _ = windows::Win32::System::Console::AttachConsole(windows::Win32::System::Console::ATTACH_PARENT_PROCESS);
    }
}

/// `--transcribe-file <wav> [--provider id] [--model m] [--no-llm]` — full pipeline, prints JSON.
pub fn transcribe_file(args: &[String]) -> i32 {
    attach_console();
    super::app::cli_transcribe_file(args)
}

/// `--render-pill <state> <t_ms> <out.png> [--dark] [--scale 1.0] [--app-icon path]`
pub fn render_pill(args: &[String]) -> i32 {
    attach_console();
    super::pill::cli_render(args)
}

/// `--render-tray-popup <out.png> [--dark] [--recording|--transcribing] [--recent] [--retry]
/// [--pill-hidden] [--hover <row>] [--focus] [--scale 1.0] [--transparent] [--no-text]`
pub fn render_tray_popup(args: &[String]) -> i32 {
    attach_console();
    super::tray::cli_render_popup(args)
}

/// `--bench <scenario> ...` — see bench/bench.ps1.
pub fn bench(args: &[String]) -> i32 {
    attach_console();
    super::app::cli_bench(args)
}
