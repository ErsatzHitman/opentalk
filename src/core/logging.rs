//! Minimal file logger: `%APPDATA%\OpenTalk\logs\<mode>.log`, line = `[t_ms] LEVEL msg`.
//! Use the `log_info!` / `log_warn!` / `log_err!` / `log_debug!` macros. Secrets must never be logged.
use std::io::Write;
use std::sync::Mutex;

static SINK: Mutex<Option<std::fs::File>> = Mutex::new(None);
static DEBUG: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn init(mode: &str) {
    let name = mode.trim_start_matches('-').replace(['/', '\\'], "_");
    let path = super::paths::log_dir().join(format!("{}.log", if name.is_empty() { "daemon" } else { &name }));
    // Truncate when larger than 2 MB to bound disk use.
    if std::fs::metadata(&path).map(|m| m.len() > 2_000_000).unwrap_or(false) {
        let _ = std::fs::remove_file(&path);
    }
    if let Ok(f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        *SINK.lock().unwrap_or_else(|p| p.into_inner()) = Some(f);
    }
    DEBUG.store(std::env::var("OPENTALK_LOG").map(|v| v == "debug").unwrap_or(false), std::sync::atomic::Ordering::Relaxed);
}

pub fn debug_enabled() -> bool { DEBUG.load(std::sync::atomic::Ordering::Relaxed) }

pub fn write(level: &str, msg: std::fmt::Arguments) {
    let line = format!("[{:>10.1}] {level} {msg}\n", super::timing::now_ms());
    if let Some(f) = SINK.lock().unwrap_or_else(|p| p.into_inner()).as_mut() {
        let _ = f.write_all(line.as_bytes());
    }
    if cfg!(debug_assertions) {
        eprint!("{line}");
    }
}

#[macro_export]
macro_rules! log_info { ($($t:tt)*) => { $crate::core::logging::write("INFO ", format_args!($($t)*)) } }
#[macro_export]
macro_rules! log_warn { ($($t:tt)*) => { $crate::core::logging::write("WARN ", format_args!($($t)*)) } }
#[macro_export]
macro_rules! log_err { ($($t:tt)*) => { $crate::core::logging::write("ERROR", format_args!($($t)*)) } }
#[macro_export]
macro_rules! log_debug { ($($t:tt)*) => { if $crate::core::logging::debug_enabled() { $crate::core::logging::write("DEBUG", format_args!($($t)*)) } } }
