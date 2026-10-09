//! Filesystem locations. Everything lives under %APPDATA%\OpenTalk.
use std::path::PathBuf;

/// `%APPDATA%\OpenTalk` (override with `OPENTALK_HOME` for tests/benchmarks). Created on demand.
pub fn app_dir() -> PathBuf {
    let base = std::env::var_os("OPENTALK_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("APPDATA").unwrap_or_else(|| ".".into())).join("OpenTalk")
        });
    let _ = std::fs::create_dir_all(&base);
    base
}
pub fn config_file() -> PathBuf { app_dir().join("config.json") }
pub fn secrets_file() -> PathBuf { app_dir().join("secrets.json") }
pub fn history_file() -> PathBuf { app_dir().join("history.jsonl") }
pub fn log_dir() -> PathBuf {
    let d = app_dir().join("logs");
    let _ = std::fs::create_dir_all(&d);
    d
}
