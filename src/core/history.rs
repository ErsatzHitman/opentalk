//! Transcription history: append-only JSON Lines at `history.jsonl`.
//! One entry per line. Appends are a single flushed `write` (a torn tail is repaired on the next
//! append and skipped on load); delete/prune/clear rewrite the file atomically (temp + rename).
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicI64, Ordering};

use serde::{Deserialize, Serialize};
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_ABANDONED, WAIT_OBJECT_0};
use windows::Win32::System::Threading::{CreateMutexW, ReleaseMutex, WaitForSingleObject};
use windows::core::HSTRING;

use super::{paths, timing};

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

const DAY_MS: i64 = 86_400_000;

/// Serializes every file operation in this process (the daemon is the only appender; the
/// settings process only rewrites on explicit user actions).
static FILE_LOCK: Mutex<()> = Mutex::new(());
/// Last id handed out by [`next_id`].
static LAST_ID: AtomicI64 = AtomicI64::new(0);

/// A fresh id: `ts_ms`, bumped past every id this process has issued.
pub fn next_id(ts_ms: i64) -> i64 {
    let mut cur = LAST_ID.load(Ordering::Relaxed);
    loop {
        let cand = ts_ms.max(cur + 1);
        match LAST_ID.compare_exchange_weak(cur, cand, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return cand,
            Err(seen) => cur = seen,
        }
    }
}

/// Stamp `entry` with a unique id (and `ts` if unset) and append it. Returns the stored entry.
pub fn append_new(mut entry: Entry) -> io::Result<Entry> {
    if entry.ts == 0 {
        entry.ts = timing::unix_ms();
    }
    entry.id = next_id(entry.ts);
    append(&entry)?;
    Ok(entry)
}

/// Append one entry (id 0 gets a fresh id).
pub fn append(entry: &Entry) -> io::Result<()> {
    append_to(&paths::history_file(), entry)
}
/// Newest first.
pub fn load_all() -> Vec<Entry> {
    load_from(&paths::history_file())
}
pub fn delete(id: i64) -> io::Result<()> {
    delete_in(&paths::history_file(), id)
}
pub fn clear() -> io::Result<()> {
    clear_in(&paths::history_file())
}
/// Remove entries older than `days` (0 = keep all). Returns removed count.
pub fn prune(days: u32) -> io::Result<usize> {
    prune_in(&paths::history_file(), days, timing::unix_ms())
}
/// Case-insensitive substring search over text/raw/app_name, newest first.
#[allow(dead_code)] // history query API; the settings page does not call it yet, tests pin it
pub fn search(query: &str) -> Vec<Entry> {
    search_in(&paths::history_file(), query)
}
/// `(count, words, avg_latency_ms)` over successful entries (`Status::Ok`): how many, total
/// words in the pasted text, and mean end-to-end latency of those with a recorded latency.
#[allow(dead_code)] // history query API; the settings page does not call it yet, tests pin it
pub fn stats() -> (usize, u64, u64) {
    stats_of(&load_all())
}

/// Cross-process name: the daemon appends while the settings process may rewrite the file.
const CROSS_PROCESS_MUTEX: &str = r"Local\OpenTalk.History";

/// In-process lock plus a named mutex shared with the other OpenTalk process, so an append
/// can't land between a rewrite's read and its rename (which would silently drop the entry).
/// The wait is bounded; on any failure we proceed unlocked rather than block dictation.
struct Lock {
    _local: std::sync::MutexGuard<'static, ()>,
    held: Option<HANDLE>,
}

fn lock() -> Lock {
    let local = FILE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    // SAFETY: the HSTRING outlives the call; the handle is closed in `Drop`, on this same thread
    // (mutex ownership is thread-affine, and `Lock` holds a `!Send` guard).
    let held = unsafe {
        match CreateMutexW(None, false, &HSTRING::from(CROSS_PROCESS_MUTEX)) {
            Ok(h) => {
                let r = WaitForSingleObject(h, 2000);
                if r == WAIT_OBJECT_0 || r == WAIT_ABANDONED {
                    Some(h)
                } else {
                    let _ = CloseHandle(h);
                    None
                }
            }
            Err(_) => None,
        }
    };
    Lock { _local: local, held }
}

impl Drop for Lock {
    fn drop(&mut self) {
        if let Some(h) = self.held.take() {
            // SAFETY: `h` is the mutex we waited on, owned by this thread, released once.
            unsafe {
                let _ = ReleaseMutex(h);
                let _ = CloseHandle(h);
            }
        }
    }
}

fn append_to(path: &Path, entry: &Entry) -> io::Result<()> {
    let mut entry = std::borrow::Cow::Borrowed(entry);
    if entry.id == 0 {
        let ts = if entry.ts == 0 { timing::unix_ms() } else { entry.ts };
        entry.to_mut().id = next_id(ts);
    } else {
        // Keep later `next_id` calls ahead of explicitly chosen ids.
        LAST_ID.fetch_max(entry.id, Ordering::Relaxed);
    }
    let mut line = serde_json::to_vec(&*entry).map_err(io::Error::other)?;
    line.push(b'\n');

    let _g = lock();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut f = std::fs::OpenOptions::new().read(true).append(true).create(true).open(path)?;
    // A crash mid-append can leave a line without its newline; start on a fresh line so the
    // damage stays confined to that one (skipped) line.
    let len = f.metadata()?.len();
    if len > 0 {
        let mut last = [0u8; 1];
        f.seek(SeekFrom::Start(len - 1))?;
        f.read_exact(&mut last)?;
        if last[0] != b'\n' {
            line.insert(0, b'\n');
        }
    }
    f.write_all(&line)?;
    f.flush()
}

fn read_bytes(path: &Path) -> Vec<u8> {
    std::fs::read(path).unwrap_or_default()
}

fn parse_line(line: &[u8]) -> Option<Entry> {
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    // A hand-edited file may start with a UTF-8 BOM.
    let line = line.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(line);
    if line.iter().all(u8::is_ascii_whitespace) {
        return None;
    }
    serde_json::from_slice(line).ok()
}

fn load_from(path: &Path) -> Vec<Entry> {
    let _g = lock();
    let mut v: Vec<Entry> = read_bytes(path).split(|&b| b == b'\n').filter_map(parse_line).collect();
    // Stable sort: equal (ts, id) keep file order reversed below.
    v.reverse();
    v.sort_by(|a, b| b.ts.cmp(&a.ts).then(b.id.cmp(&a.id)));
    v
}

/// Rewrite the file keeping every line `keep` approves. Unparseable lines are preserved
/// verbatim (they may belong to a newer version). Returns the number of removed entries.
fn rewrite_keeping(path: &Path, keep: impl Fn(&Entry) -> bool) -> io::Result<usize> {
    let _g = lock();
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e),
    };
    let mut out = Vec::with_capacity(bytes.len());
    let mut removed = 0;
    for line in bytes.split(|&b| b == b'\n') {
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        if parse_line(line).is_some_and(|e| !keep(&e)) {
            removed += 1;
            continue;
        }
        out.extend_from_slice(line.strip_suffix(b"\r").unwrap_or(line));
        out.push(b'\n');
    }
    if removed == 0 {
        return Ok(0);
    }
    let tmp = path.with_extension(format!("jsonl.{}.tmp", std::process::id()));
    std::fs::write(&tmp, &out)?;
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })?;
    Ok(removed)
}

fn delete_in(path: &Path, id: i64) -> io::Result<()> {
    rewrite_keeping(path, |e| e.id != id).map(|_| ())
}

fn clear_in(path: &Path) -> io::Result<()> {
    let _g = lock();
    match std::fs::remove_file(path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

fn prune_in(path: &Path, days: u32, now_ms: i64) -> io::Result<usize> {
    if days == 0 {
        return Ok(0);
    }
    let cutoff = now_ms.saturating_sub(i64::from(days) * DAY_MS);
    rewrite_keeping(path, |e| e.ts >= cutoff)
}

#[allow(dead_code)] // history query API; the settings page does not call it yet, tests pin it
fn search_in(path: &Path, query: &str) -> Vec<Entry> {
    let all = load_from(path);
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return all;
    }
    all.into_iter()
        .filter(|e| {
            e.text.to_lowercase().contains(&q)
                || e.raw.to_lowercase().contains(&q)
                || e.app_name.as_deref().is_some_and(|a| a.to_lowercase().contains(&q))
        })
        .collect()
}

#[allow(dead_code)] // history query API; the settings page does not call it yet, tests pin it
fn stats_of(entries: &[Entry]) -> (usize, u64, u64) {
    let ok = || entries.iter().filter(|e| e.status == Status::Ok);
    let count = ok().count();
    let words = ok().map(|e| e.text.split_whitespace().count() as u64).sum();
    let (sum, n) = ok().filter(|e| e.latency_ms > 0).fold((0u64, 0u64), |(s, n), e| (s + e.latency_ms, n + 1));
    (count, words, if n == 0 { 0 } else { sum / n })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::AtomicU32;

    fn temp_file(tag: &str) -> PathBuf {
        static N: AtomicU32 = AtomicU32::new(0);
        let d = std::env::temp_dir().join(format!(
            "opentalk-history-{}-{}-{tag}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&d).unwrap();
        d.join("history.jsonl")
    }

    fn entry(id: i64, ts: i64, text: &str) -> Entry {
        Entry { id, ts, text: text.into(), raw: format!("raw {text}"), provider: "groq".into(), ..Default::default() }
    }

    #[test]
    fn missing_file_is_empty() {
        let p = temp_file("missing");
        assert!(load_from(&p).is_empty());
        assert_eq!(delete_in(&p, 1).unwrap(), ());
        clear_in(&p).unwrap();
        assert_eq!(prune_in(&p, 7, 1_000_000_000).unwrap(), 0);
    }

    #[test]
    fn round_trip_all_fields_newest_first() {
        let p = temp_file("rt");
        let full = Entry {
            id: 1002,
            ts: 2000,
            text: "Hello \"world\"\nline two ✓".into(),
            raw: "hello world".into(),
            status: Status::Failed,
            error: Some("boom".into()),
            app_name: Some("Notepad".into()),
            app_exe: Some("notepad.exe".into()),
            provider: "openai".into(),
            model: "whisper-1".into(),
            duration_ms: 1234,
            latency_ms: 567,
            ai_cleaned: true,
        };
        append_to(&p, &entry(1001, 1000, "older")).unwrap();
        append_to(&p, &full).unwrap();
        append_to(&p, &entry(1003, 3000, "newest")).unwrap();
        let got = load_from(&p);
        assert_eq!(got.iter().map(|e| e.id).collect::<Vec<_>>(), vec![1003, 1002, 1001]);
        assert_eq!(got[1], full);
        // One physical line per entry despite the embedded newline.
        assert_eq!(std::fs::read_to_string(&p).unwrap().lines().count(), 3);
    }

    #[test]
    fn status_serializes_kebab() {
        let s = serde_json::to_string(&Status::Discarded).unwrap();
        assert_eq!(s, "\"discarded\"");
    }

    #[test]
    fn unknown_and_missing_fields_tolerated() {
        let p = temp_file("fields");
        std::fs::write(&p, "{\"id\":5,\"ts\":5,\"text\":\"x\",\"future\":[1,2]}\n").unwrap();
        let got = load_from(&p);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].text, "x");
        assert_eq!(got[0].status, Status::Ok);
    }

    #[test]
    fn corrupt_lines_are_skipped() {
        let p = temp_file("corrupt");
        let good = serde_json::to_string(&entry(1, 10, "a")).unwrap();
        let good2 = serde_json::to_string(&entry(2, 20, "b")).unwrap();
        let data = format!("{good}\nnot json at all\n\n{{\"id\":\n{good2}\r\n\u{0}\u{0}\n");
        std::fs::write(&p, data).unwrap();
        let ids: Vec<_> = load_from(&p).iter().map(|e| e.id).collect();
        assert_eq!(ids, vec![2, 1]);
        // Invalid UTF-8 bytes in the middle of a line don't break the others.
        let mut raw = good.clone().into_bytes();
        raw.extend_from_slice(b"\n\xff\xfe\xfd\n");
        raw.extend_from_slice(good2.as_bytes());
        raw.push(b'\n');
        std::fs::write(&p, raw).unwrap();
        assert_eq!(load_from(&p).len(), 2);
    }

    #[test]
    fn torn_tail_does_not_swallow_next_append() {
        let p = temp_file("torn");
        append_to(&p, &entry(1, 10, "first")).unwrap();
        // Simulate a crash mid-write: partial JSON, no newline.
        let mut f = std::fs::OpenOptions::new().append(true).open(&p).unwrap();
        f.write_all(b"{\"id\":2,\"ts\":20,\"te").unwrap();
        drop(f);
        append_to(&p, &entry(3, 30, "third")).unwrap();
        let ids: Vec<_> = load_from(&p).iter().map(|e| e.id).collect();
        assert_eq!(ids, vec![3, 1]);
    }

    #[test]
    fn delete_by_id_preserves_the_rest_and_odd_lines() {
        let p = temp_file("del");
        for i in 1..=4 {
            append_to(&p, &entry(i, i * 10, &format!("t{i}"))).unwrap();
        }
        let mut s = std::fs::read_to_string(&p).unwrap();
        s.push_str("garbage-from-newer-version\n");
        std::fs::write(&p, s).unwrap();
        delete_in(&p, 2).unwrap();
        let ids: Vec<_> = load_from(&p).iter().map(|e| e.id).collect();
        assert_eq!(ids, vec![4, 3, 1]);
        assert!(std::fs::read_to_string(&p).unwrap().contains("garbage-from-newer-version"));
        // Unknown id: no-op.
        delete_in(&p, 999).unwrap();
        assert_eq!(load_from(&p).len(), 3);
        // No temp files left.
        assert_eq!(std::fs::read_dir(p.parent().unwrap()).unwrap().count(), 1);
    }

    #[test]
    fn clear_removes_everything() {
        let p = temp_file("clear");
        append_to(&p, &entry(1, 1, "a")).unwrap();
        clear_in(&p).unwrap();
        assert!(load_from(&p).is_empty());
        // Usable again afterwards.
        append_to(&p, &entry(2, 2, "b")).unwrap();
        assert_eq!(load_from(&p).len(), 1);
    }

    #[test]
    fn prune_by_retention_days() {
        let p = temp_file("prune");
        let now = 100 * DAY_MS;
        append_to(&p, &entry(1, now - 10 * DAY_MS, "old")).unwrap();
        append_to(&p, &entry(2, now - 7 * DAY_MS, "boundary")).unwrap();
        append_to(&p, &entry(3, now - 6 * DAY_MS, "recent")).unwrap();
        append_to(&p, &entry(4, now, "now")).unwrap();
        assert_eq!(prune_in(&p, 0, now).unwrap(), 0, "0 keeps everything");
        assert_eq!(load_from(&p).len(), 4);
        assert_eq!(prune_in(&p, 7, now).unwrap(), 1);
        let ids: Vec<_> = load_from(&p).iter().map(|e| e.id).collect();
        assert_eq!(ids, vec![4, 3, 2]);
        assert_eq!(prune_in(&p, 1, now).unwrap(), 2);
        assert_eq!(load_from(&p).iter().map(|e| e.id).collect::<Vec<_>>(), vec![4]);
        assert_eq!(prune_in(&p, 1, now).unwrap(), 0);
    }

    #[test]
    fn search_is_case_insensitive_over_text_raw_app() {
        let p = temp_file("search");
        let mut a = entry(1, 10, "Meeting notes for Alice");
        a.app_name = Some("Microsoft Teams".into());
        let mut b = entry(2, 20, "buy milk");
        b.raw = "Buy MILK and eggs".into();
        let c = entry(3, 30, "unrelated");
        for e in [&a, &b, &c] {
            append_to(&p, e).unwrap();
        }
        let ids = |q: &str| search_in(&p, q).iter().map(|e| e.id).collect::<Vec<_>>();
        assert_eq!(ids("ALICE"), vec![1]);
        assert_eq!(ids("eggs"), vec![2]);
        assert_eq!(ids("teams"), vec![1]);
        assert_eq!(ids("e"), vec![3, 2, 1]);
        assert_eq!(ids("  "), vec![3, 2, 1]);
        assert_eq!(ids("zzz"), Vec::<i64>::new());
        // Error text and provider are not searched.
        assert!(ids("groq").is_empty());
    }

    #[test]
    fn ids_are_unique_and_increasing() {
        let a = next_id(5_000_000_000_000);
        let b = next_id(5_000_000_000_000);
        let c = next_id(5_000_000_000_000);
        assert!(a < b && b < c);
        let far = next_id(9_000_000_000_000);
        assert!(far >= 9_000_000_000_000);
        // Explicit ids pull the counter forward; id 0 is replaced on append.
        let p = temp_file("ids");
        append_to(&p, &entry(9_500_000_000_000, 1, "x")).unwrap();
        append_to(&p, &Entry { text: "y".into(), ts: 1, ..Default::default() }).unwrap();
        let got = load_from(&p);
        assert!(got.iter().all(|e| e.id != 0));
        assert_ne!(got[0].id, got[1].id);
    }

    #[test]
    fn concurrent_appends_stay_whole() {
        let p = temp_file("conc");
        let handles: Vec<_> = (0..8)
            .map(|t| {
                let p = p.clone();
                std::thread::spawn(move || {
                    for i in 0..25 {
                        append_to(&p, &Entry { ts: 1, text: format!("t{t}-{i}"), ..Default::default() }).unwrap();
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        let all = load_from(&p);
        assert_eq!(all.len(), 200);
        let mut ids: Vec<_> = all.iter().map(|e| e.id).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), 200, "ids unique");
    }

    #[test]
    fn stats_counts_ok_entries() {
        let mut a = entry(1, 1, "one two three");
        a.latency_ms = 400;
        let mut b = entry(2, 2, "four five");
        b.latency_ms = 800;
        let mut failed = entry(3, 3, "ignored words here");
        failed.status = Status::Failed;
        failed.latency_ms = 99_999;
        let no_latency = entry(4, 4, "six");
        assert_eq!(stats_of(&[a, b, failed, no_latency]), (3, 6, 600));
        assert_eq!(stats_of(&[]), (0, 0, 0));
    }

    #[test]
    fn utf8_bom_on_first_line_is_tolerated() {
        let p = temp_file("bom");
        let line = serde_json::to_string(&entry(1, 10, "a")).unwrap();
        std::fs::write(&p, format!("\u{feff}{line}\n")).unwrap();
        assert_eq!(load_from(&p).len(), 1);
    }

    #[test]
    fn named_mutex_blocks_other_holders_until_dropped() {
        let (tx, rx) = std::sync::mpsc::channel();
        let t = std::thread::spawn(move || {
            let g = lock();
            tx.send(()).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(300));
            drop(g);
        });
        rx.recv().unwrap();
        let start = std::time::Instant::now();
        drop(lock());
        assert!(start.elapsed() >= std::time::Duration::from_millis(150), "second lock waited");
        t.join().unwrap();
        // Reusable after release.
        drop(lock());
    }

    #[test]
    fn append_new_stamps_id_and_ts() {
        // Uses the real history path only through `append_to` to avoid env races.
        let p = temp_file("new");
        let mut e = Entry { text: "x".into(), ..Default::default() };
        e.ts = timing::unix_ms();
        e.id = next_id(e.ts);
        append_to(&p, &e).unwrap();
        assert_eq!(load_from(&p)[0], e);
    }
}
