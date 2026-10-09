//! Transcription history: append-only JSON Lines at `history.jsonl`.
//! TASK core-data implements this (append, load newest-first, delete by id, clear, prune by
//! retention days, search, stats). Writes must be crash-safe; delete/prune rewrite atomically.
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
#[serde(rename_all = "kebab-case")]
pub enum Status { #[default] Ok, Failed, Discarded }

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
#[serde(default)]
pub struct Entry {
    /// Unique id (unix ms at creation; bump by 1 on collision).
    pub id: i64,
    /// Unix ms.
    pub ts: i64,
    /// Final text that was pasted.
    pub text: String,
    /// Raw STT output before cleanup.
    pub raw: String,
    pub status: Status,
    pub error: Option<String>,
    /// Target app (friendly name, exe file name).
    pub app_name: Option<String>,
    pub app_exe: Option<String>,
    pub provider: String,
    pub model: String,
    /// Speech duration (ms) and end-to-end latency stop->paste (ms).
    pub duration_ms: u64,
    pub latency_ms: u64,
    pub ai_cleaned: bool,
}

pub fn append(entry: &Entry) -> std::io::Result<()> { let _ = entry; todo!("core-data") }
/// Newest first.
pub fn load_all() -> Vec<Entry> { todo!("core-data") }
pub fn delete(id: i64) -> std::io::Result<()> { let _ = id; todo!("core-data") }
pub fn clear() -> std::io::Result<()> { todo!("core-data") }
/// Remove entries older than `days` (0 = keep all). Returns removed count.
pub fn prune(days: u32) -> std::io::Result<usize> { let _ = days; todo!("core-data") }
/// Case-insensitive substring search over text/raw/app_name, newest first.
pub fn search(query: &str) -> Vec<Entry> { let _ = query; todo!("core-data") }
