//! Headless command-line modes: `--transcribe-file` (the full text pipeline without a microphone)
//! and `--bench ...` (helpers driven by `bench/bench.ps1` and `tests/e2e.ps1`).
//!
//! ```text
//! --transcribe-file <wav> [--provider id] [--model m] [--llm | --no-llm]
//!                                  [--rewrite-trigger <phrase> | --no-rewrite]
//! --bench dictate <n> <wav> [secs] per round: check the daemon is idle, start, play the file, stop,
//!                                  wait (default 45 s) for history; one JSON line per round
//! --bench latency [n]              print the daemon log's latency lines + median/p95 summary
//! --bench start | stop             idempotent: start only from idle, stop only while recording
//! --bench phase                    print the daemon's phase (idle | arming | recording | thinking)
//! --bench toggle | cancel | quit   one IPC message to the running daemon
//! --bench settings [page]         open the tray popup at a page (default: the Settings index)
//! --bench wait-ready [secs]        block until the daemon window exists
//! --bench mock-stt [--port P] [--delay-ms D] [--text T] [--llm-text L] [--llm-status S]
//!                                  [--llm-delay-ms D]
//!                                  local STT server: OpenAI/Deepgram REST replies over HTTP,
//!                                  Deepgram live protocol over ws:// (same port), and a
//!                                  `/chat/completions` route standing in for the cleanup/rewrite LLM
//! ```
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::latency::{self, Sample};
use super::pipeline;
use crate::core::config::{Config, SttProviderId};
use crate::core::ipc::{self, IpcMsg};
use crate::core::nav::Route;
use crate::core::timing::now_ms;
use crate::core::{history, paths};
use crate::daemon::audio::vad;
use crate::stt::{self, Audio, Credentials, SttOptions};

/// The sentence in `tests/fixtures/tts_fillers.wav`, as a recognizer would return it.
pub const FILLER_SENTENCE: &str = "Um, so I think, uh, we should like ship the new release on Friday, you know, because the the client is waiting.";

fn print_json(v: &Value) {
    println!("{v}");
}

fn flag_value<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).map(String::as_str)
}

// ---- --transcribe-file ----------------------------------------------------------------------

/// What the headless pipeline produced.
#[derive(Debug, Clone, PartialEq)]
pub struct FileResult {
    pub raw: String,
    pub text: String,
    pub provider: String,
    pub model: String,
    pub stt_ms: f64,
    pub cleanup_ms: f64,
    pub total_ms: f64,
    pub ai_cleaned: bool,
    /// What the voice rewrite did: `none`, `done`, `skipped`, `failed` or `nothing` (only the phrase).
    pub rewrite: String,
    /// The text was replaced by the AI provider's concise rewrite.
    pub rewritten: bool,
    /// Why the rewrite fell back to the text as dictated.
    pub rewrite_error: Option<String>,
    /// For a rewritten text: the deterministic text (phrase removed) it started from.
    pub original: Option<String>,
}

impl FileResult {
    fn to_json(&self) -> Value {
        json!({
            "raw": self.raw, "text": self.text, "provider": self.provider, "model": self.model,
            "stt_ms": round1(self.stt_ms), "cleanup_ms": round1(self.cleanup_ms), "total_ms": round1(self.total_ms),
            "ai_cleaned": self.ai_cleaned,
            "rewrite": self.rewrite, "rewritten": self.rewritten, "rewrite_error": self.rewrite_error, "original": self.original,
        })
    }
}

fn round1(v: f64) -> f64 {
    (v * 10.0).round() / 10.0
}

/// Transcribe + clean `audio` exactly like a dictation would (VAD trim, STT, cleanup, optional AI
/// cleanup, snippets), without touching the microphone, clipboard or history.
pub fn transcribe_audio(cfg: &Config, audio: &Audio, opts: &SttOptions, creds: &Credentials) -> Result<FileResult, String> {
    let t0 = now_ms();
    let info = vad::analyze(audio);
    if pipeline::is_silent(&info) {
        return Err("no speech detected in the recording".into());
    }
    let trimmed = vad::trim(audio, 150);
    let t1 = now_ms();
    let transcript = stt::transcribe_with(cfg.stt.provider, &trimmed, opts, creds).map_err(|e| format!("{} [{}]", e.message, e.detail))?;
    let t2 = now_ms();
    let cleaned = pipeline::clean(cfg, &transcript, info.speech_ms);
    let t3 = now_ms();
    Ok(FileResult {
        raw: cleaned.raw,
        text: cleaned.text,
        provider: format!("{:?}", cfg.stt.provider).to_ascii_lowercase(),
        model: opts.model.clone(),
        stt_ms: t2 - t1,
        cleanup_ms: t3 - t2,
        total_ms: t3 - t0,
        ai_cleaned: cleaned.ai_cleaned,
        rewrite: cleaned.rewrite.label().to_string(),
        rewritten: cleaned.rewrite == pipeline::RewriteState::Done,
        rewrite_error: match &cleaned.rewrite {
            pipeline::RewriteState::Failed(e) => Some(e.to_string()),
            _ => None,
        },
        original: cleaned.original,
    })
}

pub fn transcribe_file(args: &[String]) -> i32 {
    let Some(path) = args.first().filter(|a| !a.starts_with("--")) else {
        eprintln!("usage: opentalk --transcribe-file <wav> [--provider id] [--model m] [--llm | --no-llm] [--rewrite-trigger <phrase> | --no-rewrite]");
        return 2;
    };
    let fail = |msg: String| {
        eprintln!("error: {msg}");
        print_json(&json!({ "error": msg }));
        1
    };
    let mut cfg = Config::load();
    if let Some(p) = flag_value(args, "--provider") {
        match serde_json::from_value::<SttProviderId>(Value::String(p.to_ascii_lowercase())) {
            Ok(id) => cfg.stt.provider = id,
            Err(_) => return fail(format!("unknown provider '{p}'")),
        }
    }
    if let Some(m) = flag_value(args, "--model") {
        if cfg.stt.provider == SttProviderId::Custom {
            cfg.stt.custom_model = m.to_string();
        } else {
            cfg.stt.models.insert(cfg.stt.provider, m.to_string());
        }
    }
    if args.iter().any(|a| a == "--no-llm") {
        cfg.ai_cleanup.enabled = false;
    }
    if args.iter().any(|a| a == "--llm") {
        cfg.ai_cleanup.enabled = true;
    }
    if let Some(phrase) = flag_value(args, "--rewrite-trigger") {
        cfg.rewrite_trigger = phrase.to_string();
        cfg.ai_cleanup.rewrite_enabled = true;
    }
    if args.iter().any(|a| a == "--no-rewrite") {
        cfg.ai_cleanup.rewrite_enabled = false;
    }
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) => return fail(format!("{path}: {e}")),
    };
    let Some(audio) = Audio::from_wav(&bytes) else { return fail(format!("{path}: not a PCM16 WAV file")) };
    let (opts, creds) = (stt::options(&cfg), stt::credentials(&cfg));
    match transcribe_audio(&cfg, &audio, &opts, &creds) {
        Ok(r) => {
            print_json(&r.to_json());
            0
        }
        Err(e) => fail(e),
    }
}

// ---- --bench --------------------------------------------------------------------------------

pub fn bench(args: &[String]) -> i32 {
    match args.first().map(String::as_str) {
        Some("dictate") => bench_dictate(&args[1..]),
        Some("latency") => bench_latency(&args[1..]),
        Some("toggle") => send(IpcMsg::Toggle),
        Some("start") => send(IpcMsg::Start),
        Some("stop") => send(IpcMsg::Stop),
        Some("phase") => bench_phase(),
        Some("cancel") => send(IpcMsg::Cancel),
        Some("quit") => send(IpcMsg::Quit),
        Some("settings") => send(IpcMsg::OpenPage(args.get(1).map_or(Route::SettingsIndex, |p| Route::parse(Some(p.as_str()))))),
        Some("wait-ready") => bench_wait_ready(&args[1..]),
        Some("mock-stt") => bench_mock(&args[1..]),
        Some("info") => bench_info(),
        _ => {
            eprintln!("usage: opentalk --bench dictate <n> <wav> [secs] | latency [n] | start | stop | phase | toggle | cancel | quit | settings [page] | wait-ready [secs] | mock-stt [--port P] [--delay-ms D] [--text T]");
            2
        }
    }
}

fn send(msg: IpcMsg) -> i32 {
    if ipc::notify_daemon(msg) {
        0
    } else {
        eprintln!("no OpenTalk daemon is running");
        1
    }
}

fn bench_phase() -> i32 {
    if !ipc::daemon_running() {
        eprintln!("no OpenTalk daemon is running");
        return 1;
    }
    match ipc::query_daemon_phase() {
        Some(p) => {
            print_json(&json!({ "phase": p.name() }));
            0
        }
        None => {
            eprintln!("the daemon did not answer (busy or hung)");
            1
        }
    }
}

fn bench_wait_ready(args: &[String]) -> i32 {
    let secs: f64 = args.first().and_then(|s| s.parse().ok()).unwrap_or(15.0);
    let t0 = Instant::now();
    while t0.elapsed().as_secs_f64() < secs {
        if ipc::daemon_running() {
            print_json(&json!({ "ready_ms": round1(t0.elapsed().as_secs_f64() * 1000.0) }));
            return 0;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    eprintln!("daemon did not appear within {secs} s");
    1
}

/// How often the bench polls the daemon for its phase.
const PHASE_POLL: Duration = Duration::from_millis(10);
/// Longest a round waits for the previous dictation to finish before cancelling it.
const IDLE_WAIT: Duration = Duration::from_secs(10);
/// A round is tried this many times when something else (a stray hotkey press, a toggle) ended
/// the recording before the bench did.
const MAX_ATTEMPTS: u32 = 3;

/// Poll the daemon until its phase satisfies `pred` or `within` passes. The bool says whether it
/// did; the phase is the last one the daemon reported (`None`: it never answered).
fn wait_phase(pred: impl Fn(ipc::DaemonPhase) -> bool, within: Duration) -> (bool, Option<ipc::DaemonPhase>) {
    let t0 = Instant::now();
    let mut last = None;
    loop {
        if let Some(p) = ipc::query_daemon_phase() {
            last = Some(p);
            if pred(p) {
                return (true, last);
            }
        }
        if t0.elapsed() >= within {
            return (false, last);
        }
        std::thread::sleep(PHASE_POLL);
    }
}

fn phase_name(p: Option<ipc::DaemonPhase>) -> &'static str {
    p.map_or("unknown", ipc::DaemonPhase::name)
}

/// What the pre-round check found and did.
#[derive(Debug, Default, PartialEq)]
struct Precheck {
    waited_idle_ms: f64,
    /// The daemon was still busy after [`IDLE_WAIT`]; it was cancelled.
    cancelled_stale: bool,
    /// History holds fewer entries than right after the previous round (an entry vanished).
    prev_entry_missing: bool,
}

/// Newest history entry's id (0 when empty): a round is over when it changes.
fn newest_id() -> i64 {
    history::load_all().first().map_or(0, |e| e.id)
}

/// Make sure the daemon is idle and the previous round's history entry exists before recording.
/// `expected_len` is the history length after the previous round (None for the first).
fn prepare_round(expected_len: Option<usize>) -> Result<Precheck, String> {
    let t0 = Instant::now();
    let mut pre = Precheck::default();
    let (idle, last) = wait_phase(|p| p == ipc::DaemonPhase::Idle, IDLE_WAIT);
    if !idle {
        if last.is_none() {
            return Err("the daemon does not answer phase queries".into());
        }
        eprintln!("daemon still {} after {} s; cancelling it", phase_name(last), IDLE_WAIT.as_secs());
        if !ipc::notify_daemon(IpcMsg::Cancel) {
            return Err("daemon went away".into());
        }
        pre.cancelled_stale = true;
        let (idle, last) = wait_phase(|p| p == ipc::DaemonPhase::Idle, Duration::from_secs(5));
        if !idle {
            return Err(format!("the daemon is not idle (phase {})", phase_name(last)));
        }
    }
    pre.waited_idle_ms = round1(t0.elapsed().as_secs_f64() * 1000.0);
    pre.prev_entry_missing = expected_len.is_some_and(|e| history::load_all().len() < e);
    Ok(pre)
}

/// Latency samples in the daemon log (oldest first).
fn log_samples() -> Vec<Sample> {
    let text = std::fs::read_to_string(paths::log_dir().join("daemon.log")).unwrap_or_default();
    text.lines().filter_map(latency::parse_line).collect()
}

/// How one dictation attempt ended.
#[derive(Debug)]
enum Attempt {
    /// A history entry landed.
    Done { status: String },
    /// No entry within the wait.
    Timeout,
    /// The recording never started, or something other than the bench ended it before the stop.
    Disturbed { why: String },
}

/// `--bench dictate <n> <wav> [secs]`: needs a daemon started with `OPENTALK_INJECT_WAV=<wav>`.
/// Rounds use explicit start / stop messages (never a toggle, whose parity one stray press flips),
/// each preceded by a check that the daemon is idle and the previous entry is in history. Each
/// round waits up to `secs` (default 45) after the stop for its history entry and prints one JSON
/// line: status, recorded_ms, audio_ms and upload_bytes (from the daemon log's latency line).
/// A round whose recording something else ended is reported (`"status":"disturbed"`) and redone.
fn bench_dictate(args: &[String]) -> i32 {
    let (Some(n), Some(wav)) = (args.first().and_then(|s| s.parse::<u32>().ok()), args.get(1)) else {
        eprintln!("usage: opentalk --bench dictate <n> <wav> [secs]");
        return 2;
    };
    let wait = Duration::from_secs(args.get(2).and_then(|s| s.parse().ok()).unwrap_or(45));
    let wav_ms = match std::fs::read(wav).ok().and_then(|b| Audio::from_wav(&b)) {
        Some(a) => a.duration_ms(),
        None => {
            eprintln!("cannot read {wav} as a PCM16 WAV");
            return 2;
        }
    };
    if !ipc::daemon_running() {
        eprintln!("no OpenTalk daemon is running (start it with OPENTALK_INJECT_WAV={wav})");
        return 1;
    }
    let mut failures = 0;
    let mut expected_len = None;
    for i in 1..=n {
        for attempt in 1..=MAX_ATTEMPTS {
            let pre = match prepare_round(expected_len) {
                Ok(p) => p,
                Err(e) => {
                    eprintln!("round {i}: {e}");
                    print_json(&json!({ "round": i, "attempt": attempt, "status": "not-idle", "error": e }));
                    return 1;
                }
            };
            let before_id = newest_id();
            let samples_before = log_samples().len();
            let started = Instant::now();
            let mut stop = started;
            let outcome = dictate_once(wav_ms, wait, before_id, &mut stop);
            let (status, retry) = match &outcome {
                Attempt::Done { status } => (status.clone(), false),
                Attempt::Timeout => ("timeout".to_string(), false),
                Attempt::Disturbed { .. } => ("disturbed".to_string(), attempt < MAX_ATTEMPTS),
            };
            // The latency line may land just after the history entry.
            let mut sample = None;
            if matches!(outcome, Attempt::Done { .. }) {
                let t = Instant::now();
                while t.elapsed() < Duration::from_secs(1) {
                    let all = log_samples();
                    if all.len() > samples_before {
                        sample = all.last().cloned();
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
            }
            let mut row = json!({
                "round": i, "attempt": attempt, "status": status,
                "recorded_ms": round1(stop.duration_since(started).as_secs_f64() * 1000.0),
                "stop_to_history_ms": round1(stop.elapsed().as_secs_f64() * 1000.0),
                "audio_ms": sample.as_ref().map(|s| s.audio_ms),
                "upload_bytes": sample.as_ref().map(|s| s.upload_bytes),
                "waited_idle_ms": pre.waited_idle_ms,
                "cancelled_stale": pre.cancelled_stale,
                "prev_entry_missing": pre.prev_entry_missing,
            });
            if let Attempt::Disturbed { why } = &outcome {
                row["why"] = json!(why);
                row["retry"] = json!(retry);
            }
            print_json(&row);
            if matches!(outcome, Attempt::Disturbed { .. }) {
                // Let an entry for the cut-short recording land before the next check reads history.
                let t = Instant::now();
                while t.elapsed() < Duration::from_secs(2) && newest_id() == before_id {
                    std::thread::sleep(Duration::from_millis(20));
                }
            }
            expected_len = Some(history::load_all().len());
            if retry {
                continue;
            }
            failures += (status != "ok") as i32;
            break;
        }
        std::thread::sleep(Duration::from_millis(400));
    }
    (failures > 0) as i32
}

/// One start / play / stop cycle. `stop` is set to the instant the stop was sent.
fn dictate_once(wav_ms: u64, wait: Duration, before_id: i64, stop: &mut Instant) -> Attempt {
    use ipc::DaemonPhase as P;
    if !ipc::notify_daemon(IpcMsg::Start) {
        return Attempt::Disturbed { why: "daemon went away".into() };
    }
    let (recording, phase) = wait_phase(|p| p == P::Recording, Duration::from_secs(3));
    if !recording {
        let why = format!("start did not begin a recording (phase {})", phase_name(phase));
        if phase.is_some_and(|p| p != P::Idle) {
            ipc::notify_daemon(IpcMsg::Cancel);
        }
        return Attempt::Disturbed { why };
    }
    // The injected file plays in real time; leave a little tail so the VAD sees its end.
    std::thread::sleep(Duration::from_millis(wav_ms + 600));
    let at_stop = wait_phase(|_| true, Duration::from_millis(500)).1;
    *stop = Instant::now();
    match at_stop {
        Some(P::Recording) => {
            ipc::notify_daemon(IpcMsg::Stop);
        }
        // Something else already ended it (a stray toggle); any entry it leaves is short.
        other => {
            return Attempt::Disturbed { why: format!("phase {} when the bench went to stop", phase_name(other)) };
        }
    }
    while stop.elapsed() < wait {
        if let Some(e) = history::load_all().first()
            && e.id != before_id
        {
            return Attempt::Done { status: format!("{:?}", e.status).to_ascii_lowercase() };
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    Attempt::Timeout
}

/// `--bench latency [n]`: the latency lines of the daemon log plus a median / p95 summary.
fn bench_latency(args: &[String]) -> i32 {
    let keep: usize = args.first().and_then(|s| s.parse().ok()).unwrap_or(usize::MAX);
    let log = paths::log_dir().join("daemon.log");
    let text = std::fs::read_to_string(&log).unwrap_or_default();
    let mut samples: Vec<Sample> = text.lines().filter_map(latency::parse_line).collect();
    if samples.len() > keep {
        samples.drain(..samples.len() - keep);
    }
    print_json(&summarize(&samples));
    0
}

pub fn summarize(samples: &[Sample]) -> Value {
    let col = |f: fn(&Sample) -> f64| -> Value {
        let mut v: Vec<f64> = samples.iter().map(f).filter(|x| *x >= 0.0).collect();
        match latency::median_p95(&mut v) {
            Some((m, p)) => json!({ "median": round1(m), "p95": round1(p) }),
            None => Value::Null,
        }
    };
    json!({
        "count": samples.len(),
        "hotkey_pill": col(|s| s.hotkey_pill),
        "hotkey_audio": col(|s| s.hotkey_audio),
        "prep": col(|s| s.prep),
        "stt": col(|s| s.stt),
        "cleanup": col(|s| s.cleanup),
        "paste": col(|s| s.paste),
        "total": col(|s| s.total),
        "local_overhead": col(|s| s.total - s.stt),
        "local_before_paste": col(|s| s.prep + s.cleanup),
        "upload_bytes": col(|s| s.upload_bytes as f64),
        "provider": samples.last().map(|s| s.provider.clone()),
        "model": samples.last().map(|s| s.model.clone()),
    })
}

/// `--bench info`: what the embedded manifest actually did to this process.
fn bench_info() -> i32 {
    use windows::Win32::Globalization::GetACP;
    use windows::Win32::UI::HiDpi::{AreDpiAwarenessContextsEqual, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetThreadDpiAwarenessContext};
    // SAFETY: plain queries without preconditions.
    let (acp, per_monitor_v2) = unsafe {
        (GetACP(), AreDpiAwarenessContextsEqual(GetThreadDpiAwarenessContext(), DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2).as_bool())
    };
    print_json(&json!({ "version": env!("CARGO_PKG_VERSION"), "acp": acp, "per_monitor_v2": per_monitor_v2 }));
    0
}

// ---- mock STT server ------------------------------------------------------------------------

fn bench_mock(args: &[String]) -> i32 {
    let port: u16 = flag_value(args, "--port").and_then(|s| s.parse().ok()).unwrap_or(0);
    let delay = Duration::from_millis(flag_value(args, "--delay-ms").and_then(|s| s.parse().ok()).unwrap_or(0));
    let text = flag_value(args, "--text").unwrap_or(FILLER_SENTENCE).to_string();
    let mut mock = Mock::new(delay, text);
    mock.llm_text = flag_value(args, "--llm-text").map(str::to_string);
    mock.llm_delay = Duration::from_millis(flag_value(args, "--llm-delay-ms").and_then(|s| s.parse().ok()).unwrap_or(0));
    mock.llm_status = flag_value(args, "--llm-status").and_then(|s| s.parse().ok()).unwrap_or(200);
    let listener = match TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], port))) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("cannot bind 127.0.0.1:{port}: {e}");
            return 1;
        }
    };
    let addr = listener.local_addr().map(|a| a.to_string()).unwrap_or_default();
    println!("listening http://{addr}");
    let _ = std::io::stdout().flush();
    serve_with(listener, Arc::new(mock));
    0
}

/// What the mock server answers.
#[derive(Debug)]
pub struct Mock {
    /// Added to every STT reply, to the Deepgram finalize and to chat replies.
    pub delay: Duration,
    /// The transcript every STT request returns.
    pub text: String,
    /// The chat-completions reply content; `None` = a deterministic concise version of the
    /// `<transcript>` in the request (its first 60% of the words).
    pub llm_text: Option<String>,
    /// Extra wait before chat-completions replies, on top of `delay` (`--llm-delay-ms`).
    pub llm_delay: Duration,
    /// HTTP status of chat-completions replies (anything but 200 returns an error body).
    pub llm_status: u16,
    /// Every chat-completions request body received (tests assert the request shape).
    pub llm_requests: std::sync::Mutex<Vec<String>>,
}

impl Mock {
    pub fn new(delay: Duration, text: String) -> Mock {
        Mock { delay, text, llm_text: None, llm_delay: Duration::ZERO, llm_status: 200, llm_requests: Default::default() }
    }
}

/// Accept connections forever; one thread per connection (keep-alive aware). A WebSocket
/// upgrade gets the Deepgram live protocol, anything else plain HTTP.
#[cfg(test)]
pub fn serve_forever(listener: TcpListener, delay: Duration, text: Arc<String>) {
    serve_with(listener, Arc::new(Mock::new(delay, text.to_string())));
}

/// [`serve_forever`] with the full mock configuration.
pub fn serve_with(listener: TcpListener, mock: Arc<Mock>) {
    for conn in listener.incoming().flatten() {
        let mock = mock.clone();
        std::thread::spawn(move || {
            if is_websocket_upgrade(&conn) {
                serve_ws(conn, mock.delay, &mock.text);
            } else {
                let _ = serve_connection(conn, &mock);
            }
        });
    }
}

/// Peek (without consuming) at the request head for `Upgrade: websocket`.
fn is_websocket_upgrade(stream: &TcpStream) -> bool {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let mut buf = [0u8; 4096];
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let n = stream.peek(&mut buf).unwrap_or(0);
        let head = String::from_utf8_lossy(&buf[..n]).to_ascii_lowercase();
        if head.contains("\r\n\r\n") || n == buf.len() || n == 0 || Instant::now() > deadline {
            return head.contains("upgrade: websocket");
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// The reply body for a batch request: OpenAI-style `text` plus Deepgram REST `results`, so one
/// mock serves either provider.
fn batch_body(text: &str) -> String {
    json!({ "text": text, "results": { "channels": [{ "alternatives": [{ "transcript": text }] }] } }).to_string()
}

/// Deepgram live (`/v1/listen`): interim `Results` while audio arrives, the full text as a final
/// `from_finalize` result on `Finalize`, close on `CloseStream`.
fn serve_ws(stream: TcpStream, delay: Duration, text: &str) {
    use tungstenite::Message;
    let _ = stream.set_read_timeout(None);
    let Ok(mut ws) = tungstenite::accept(stream) else { return };
    let result = |t: &str, fin: bool| {
        json!({ "type": "Results", "is_final": fin, "from_finalize": fin, "channel": { "alternatives": [{ "transcript": t }] } }).to_string()
    };
    let first_words: String = text.split_whitespace().take(3).collect::<Vec<_>>().join(" ");
    let mut bytes = 0usize;
    let mut sent_interim = false;
    loop {
        match ws.read() {
            Ok(Message::Binary(b)) => {
                bytes += b.len();
                // One interim result after ~1 s of audio (16 kHz mono i16).
                if !sent_interim && bytes >= 32_000 {
                    sent_interim = true;
                    let _ = ws.send(Message::text(result(&first_words, false)));
                }
            }
            Ok(Message::Text(t)) => {
                if t.contains("\"Finalize\"") {
                    std::thread::sleep(delay);
                    let _ = ws.send(Message::text(result(if bytes > 0 { text } else { "" }, true)));
                } else if t.contains("\"CloseStream\"") {
                    let _ = ws.close(None);
                }
            }
            Ok(Message::Close(_)) | Err(_) => break,
            Ok(_) => {}
        }
    }
}

/// The user message's `<transcript>` text of a chat-completions request body, if present.
fn transcript_in(request: &str) -> Option<String> {
    let v: Value = serde_json::from_str(request).ok()?;
    let user = v.get("messages")?.as_array()?.iter().rev().find(|m| m["role"] == "user")?["content"].as_str()?;
    let start = user.find("<transcript>")? + "<transcript>".len();
    let end = user.rfind("</transcript>")?;
    Some(user[start..end.max(start)].trim().to_string())
}

/// A deterministic "concise rewrite": the first 60% of the words, closed with a full stop.
fn concise(text: &str) -> String {
    let words: Vec<&str> = text.split_whitespace().collect();
    let keep = (words.len() * 3).div_ceil(5).max(1);
    let mut out = words[..keep.min(words.len())].join(" ");
    out = out.trim_end_matches(['.', ',', ';', ':', '!', '?']).to_string();
    out.push('.');
    out
}

/// The chat-completions answer (status, body) for a request body.
fn chat_reply(mock: &Mock, request: &str) -> (u16, String) {
    if mock.llm_status != 200 {
        let body = json!({ "error": { "message": "mock llm error", "code": mock.llm_status } });
        return (mock.llm_status, body.to_string());
    }
    let content = mock.llm_text.clone().unwrap_or_else(|| concise(&transcript_in(request).unwrap_or_default()));
    let body = json!({ "choices": [{ "message": { "role": "assistant", "content": content }, "finish_reason": "stop" }] });
    (200, body.to_string())
}

/// Minimal HTTP/1.1 server loop: POSTs to `.../chat/completions` get a chat reply (the LLM
/// stand-in), other POSTs get `{"text": ...}`, all after `delay`; anything else an empty 200.
fn serve_connection(stream: TcpStream, mock: &Mock) -> std::io::Result<()> {
    stream.set_nodelay(true)?;
    stream.set_read_timeout(Some(Duration::from_secs(60)))?;
    let mut out = stream.try_clone()?;
    let mut reader = BufReader::new(stream);
    loop {
        let mut request_line = String::new();
        if reader.read_line(&mut request_line)? == 0 {
            return Ok(());
        }
        let method = request_line.split_whitespace().next().unwrap_or("").to_string();
        let path = request_line.split_whitespace().nth(1).unwrap_or("").to_string();
        let (mut len, mut chunked, mut close) = (0usize, false, false);
        loop {
            let mut h = String::new();
            if reader.read_line(&mut h)? == 0 {
                return Ok(());
            }
            let h = h.trim_end();
            if h.is_empty() {
                break;
            }
            let lower = h.to_ascii_lowercase();
            if let Some(v) = lower.strip_prefix("content-length:") {
                len = v.trim().parse().unwrap_or(0);
            } else if lower.starts_with("transfer-encoding:") && lower.contains("chunked") {
                chunked = true;
            } else if lower.starts_with("connection:") && lower.contains("close") {
                close = true;
            }
        }
        let is_chat = method == "POST" && path.split('?').next().unwrap_or("").ends_with("/chat/completions");
        // Request bodies are read in full (the STT ones are audio: only chat bodies are kept).
        let mut request = Vec::new();
        let sink: &mut dyn Write = &mut request;
        let mut discard = std::io::sink();
        let dest: &mut dyn Write = if is_chat { sink } else { &mut discard };
        if chunked {
            loop {
                let mut size = String::new();
                reader.read_line(&mut size)?;
                let n = usize::from_str_radix(size.trim().split(';').next().unwrap_or("0"), 16).unwrap_or(0);
                std::io::copy(&mut reader.by_ref().take(n as u64 + 2), dest)?;
                if n == 0 {
                    break;
                }
            }
        } else {
            std::io::copy(&mut reader.by_ref().take(len as u64), dest)?;
        }
        let (status, body) = if is_chat {
            let request = String::from_utf8_lossy(&request).into_owned();
            let reply = chat_reply(mock, &request);
            mock.llm_requests.lock().unwrap_or_else(|e| e.into_inner()).push(request);
            std::thread::sleep(mock.delay + mock.llm_delay);
            reply
        } else if method == "POST" {
            std::thread::sleep(mock.delay);
            (200, batch_body(&mock.text))
        } else {
            (200, String::new())
        };
        let head = format!(
            "HTTP/1.1 {status} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: {}\r\n\r\n",
            if status == 200 { "OK" } else { "Error" },
            body.len(),
            if close { "close" } else { "keep-alive" }
        );
        out.write_all(head.as_bytes())?;
        out.write_all(body.as_bytes())?;
        out.flush()?;
        if close {
            return Ok(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::config::Snippet;

    fn start_mock(delay_ms: u64) -> String {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", l.local_addr().unwrap());
        let text = Arc::new(FILLER_SENTENCE.to_string());
        std::thread::spawn(move || serve_forever(l, Duration::from_millis(delay_ms), text));
        url
    }

    fn fixture(name: &str) -> Audio {
        let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name);
        Audio::from_wav(&std::fs::read(p).unwrap()).unwrap()
    }

    #[test]
    fn mock_server_answers_keep_alive_requests() {
        let base = start_mock(0);
        let agent = crate::core::http::agent();
        for _ in 0..3 {
            let r = agent.post(&format!("{base}/audio/transcriptions")).send_string("hello").unwrap();
            let v: Value = serde_json::from_str(&r.into_string().unwrap()).unwrap();
            assert_eq!(v["text"], FILLER_SENTENCE);
        }
        assert_eq!(agent.head(&format!("{base}/")).call().unwrap().status(), 200);
    }

    #[test]
    fn file_pipeline_removes_fillers_end_to_end_against_the_mock() {
        let base = start_mock(15);
        let mut cfg = Config::default();
        cfg.stt.provider = SttProviderId::Openai;
        cfg.snippets = vec![Snippet { trigger: "ship it".into(), replacement: "deploy".into() }];
        let opts = SttOptions { model: "whisper-1".into(), timeout_ms: 5_000, ..SttOptions::default() };
        let creds = Credentials { api_key: Some("test-key-123456".into()), base_url: Some(base), extra: vec![] };
        let r = transcribe_audio(&cfg, &fixture("tts_fillers.wav"), &opts, &creds).unwrap();
        assert_eq!(r.raw, FILLER_SENTENCE);
        let low = r.text.to_ascii_lowercase();
        for filler in ["um,", " uh,", "you know", "the the", " like "] {
            assert!(!low.contains(filler), "'{filler}' left in: {}", r.text);
        }
        assert!(r.text.contains("ship the new release on Friday"), "{}", r.text);
        assert!(r.stt_ms >= 15.0 && r.total_ms >= r.stt_ms, "{r:?}");
        assert!(!r.ai_cleaned);
        let j = r.to_json();
        assert_eq!(j["provider"], "openai");
        assert!(j["total_ms"].is_number());
    }

    #[test]
    fn streaming_session_against_the_mock_feeds_the_cleanup_pipeline() {
        // The daemon path for a streaming provider: session opened at the press, PCM pushed
        // while recording, finish() at stop, then the same cleanup as batch.
        let base = start_mock(5);
        let ws = base.replacen("http://", "ws://", 1);
        let mut cfg = Config::default();
        cfg.stt.provider = SttProviderId::Deepgram;
        cfg.stt.prefer_streaming = true;
        let creds = Credentials { api_key: Some("dg-test-key-123456".into()), base_url: Some(ws), extra: vec![] };
        let session = crate::stt::streaming::start_with(&cfg, |_| creds, |_| SttOptions { timeout_ms: 5_000, ..SttOptions::default() })
            .expect("deepgram streams")
            .expect("session");
        let mut session = session;
        let audio = fixture("tts_fillers.wav");
        for chunk in audio.samples.chunks(1600) {
            session.push(chunk);
        }
        let transcript = session.finish().expect("finish");
        assert_eq!(transcript.text, FILLER_SENTENCE);
        let cleaned = pipeline::clean(&cfg, &transcript, 4000);
        assert!(cleaned.text.starts_with("So I think we should ship"), "{}", cleaned.text);
        // The batch fallback for the same provider gets a Deepgram REST-shaped reply.
        let opts = SttOptions { model: "nova-3".into(), timeout_ms: 5_000, ..SttOptions::default() };
        let creds = Credentials { api_key: Some("dg-test-key-123456".into()), base_url: Some(base), extra: vec![] };
        let batch = stt::transcribe_with(SttProviderId::Deepgram, &audio, &opts, &creds).expect("batch");
        assert_eq!(batch.text, FILLER_SENTENCE);
    }

    /// A mock with its own settings; returns the base URL and the shared mock (to read the requests).
    fn start_mock_with(configure: impl FnOnce(&mut Mock)) -> (String, Arc<Mock>) {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", l.local_addr().unwrap());
        let mut mock = Mock::new(Duration::ZERO, FILLER_SENTENCE.to_string());
        configure(&mut mock);
        let mock = Arc::new(mock);
        let m = mock.clone();
        std::thread::spawn(move || serve_with(l, m));
        (url, mock)
    }

    fn chat_request(transcript: &str) -> Value {
        json!({
            "model": "m", "temperature": 0,
            "messages": [
                { "role": "system", "content": "sys" },
                { "role": "user", "content": format!("<transcript>\n{transcript}\n</transcript>\n\nOutput only the concise rewrite.") },
            ],
        })
    }

    #[test]
    fn mock_chat_route_returns_a_shorter_choices_reply() {
        let (base, mock) = start_mock_with(|_| {});
        let agent = crate::core::http::agent();
        let sent = "one two three four five six seven eight nine ten.";
        let r = agent.post(&format!("{base}/chat/completions")).send_json(chat_request(sent)).unwrap();
        let v: Value = serde_json::from_str(&r.into_string().unwrap()).unwrap();
        assert_eq!(v["choices"][0]["message"]["content"], "one two three four five six.", "first 60% of the words");
        assert_eq!(v["choices"][0]["finish_reason"], "stop");
        let seen = mock.llm_requests.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert!(seen[0].contains("one two three"), "the request body is recorded");
        // The nested (`/v1/...`) path and a query string are the same route.
        drop(seen);
        let r = agent.post(&format!("{base}/api/v1/chat/completions?x=1")).send_json(chat_request("alpha beta gamma delta epsilon")).unwrap();
        let v: Value = serde_json::from_str(&r.into_string().unwrap()).unwrap();
        assert_eq!(v["choices"][0]["message"]["content"], "alpha beta gamma.");
    }

    #[test]
    fn mock_chat_text_and_status_flags() {
        let (base, _) = start_mock_with(|m| m.llm_text = Some("Fixed answer.".into()));
        let agent = crate::core::http::agent();
        let r = agent.post(&format!("{base}/chat/completions")).send_json(chat_request("whatever words here")).unwrap();
        let v: Value = serde_json::from_str(&r.into_string().unwrap()).unwrap();
        assert_eq!(v["choices"][0]["message"]["content"], "Fixed answer.");

        let (base, _) = start_mock_with(|m| m.llm_status = 500);
        let e = agent.post(&format!("{base}/chat/completions")).send_json(chat_request("x y z")).unwrap_err();
        match e {
            ureq::Error::Status(code, resp) => {
                assert_eq!(code, 500);
                assert!(resp.into_string().unwrap().contains("mock llm error"));
            }
            other => panic!("expected an HTTP status error, got {other:?}"),
        }
        // The LLM through the real client maps to an Http error.
        let mut cfg = Config::default();
        cfg.ai_cleanup.rewrite_timeout_ms = 3_000;
        let r = crate::llm::rewrite_with(&cfg, "one two three four five six", &[], Some("k-test-12345"), Some(&base));
        assert!(matches!(r, Err(crate::llm::LlmError::Http { status: 500, .. })), "{r:?}");
    }

    #[test]
    fn mock_stt_posts_are_unchanged_by_the_chat_route() {
        let (base, mock) = start_mock_with(|m| m.llm_text = Some("nope".into()));
        let agent = crate::core::http::agent();
        let r = agent.post(&format!("{base}/audio/transcriptions")).send_string("audio").unwrap();
        let v: Value = serde_json::from_str(&r.into_string().unwrap()).unwrap();
        assert_eq!(v["text"], FILLER_SENTENCE);
        assert!(mock.llm_requests.lock().unwrap().is_empty());
    }

    #[test]
    fn concise_helper_edge_cases() {
        assert_eq!(concise("one"), "one.");
        assert_eq!(concise(""), ".");
        assert_eq!(concise("a b c d e"), "a b c.");
        assert_eq!(concise("Hello there, friend!"), "Hello there.");
        assert_eq!(transcript_in("not json"), None);
        assert_eq!(transcript_in(&chat_request("hi there").to_string()).as_deref(), Some("hi there"));
    }

    #[test]
    fn file_pipeline_rewrites_when_the_dictation_ends_with_the_phrase() {
        let sentence = "So I was thinking that maybe we could possibly move the meeting from Thursday to Friday afternoon if that works for everyone. Rewrite this concisely.";
        let (base, mock) = start_mock_with(|m| m.text = sentence.to_string());
        let mut cfg = Config::default();
        cfg.stt.provider = SttProviderId::Openai;
        cfg.ai_cleanup.rewrite_timeout_ms = 3_000;
        let opts = SttOptions { model: "whisper-1".into(), timeout_ms: 5_000, ..SttOptions::default() };
        let creds = Credentials { api_key: Some("test-key-123456".into()), base_url: Some(base.clone()), extra: vec![] };
        let audio = fixture("tts_fillers.wav");
        let info = vad::analyze(&audio);
        let transcript = stt::transcribe_with(cfg.stt.provider, &vad::trim(&audio, 150), &opts, &creds).unwrap();
        assert_eq!(transcript.text, sentence);
        let calls = pipeline::LlmCalls {
            cleanup: &|t| crate::llm::cleanup_with(&cfg, t, &[], Some("llm-key-12345"), Some(&base)),
            rewrite: &|t| crate::llm::rewrite_with(&cfg, t, &[], Some("llm-key-12345"), Some(&base)),
        };
        let cleaned = pipeline::clean_with_calls(&cfg, &transcript, info.speech_ms, &calls);
        assert_eq!(cleaned.rewrite, pipeline::RewriteState::Done, "{cleaned:?}");
        assert!(cleaned.text.split_whitespace().count() < 20 && !cleaned.text.to_lowercase().contains("rewrite"), "{}", cleaned.text);
        assert!(cleaned.raw.contains("Rewrite this concisely"));
        let seen = mock.llm_requests.lock().unwrap();
        assert_eq!(seen.len(), 1, "exactly one LLM request");
        let sent = transcript_in(&seen[0]).unwrap();
        assert!(sent.starts_with("So I was thinking") && sent.ends_with("everyone.") && !sent.to_lowercase().contains("concisely"), "{sent}");
        // The file result carries the new fields.
        let r = FileResult {
            raw: cleaned.raw,
            text: cleaned.text,
            provider: "openai".into(),
            model: "whisper-1".into(),
            stt_ms: 1.0,
            cleanup_ms: 2.0,
            total_ms: 3.0,
            ai_cleaned: false,
            rewrite: cleaned.rewrite.label().into(),
            rewritten: true,
            rewrite_error: None,
            original: cleaned.original,
        };
        let j = r.to_json();
        assert_eq!((j["rewritten"].clone(), j["rewrite"].clone(), j["rewrite_error"].clone()), (json!(true), json!("done"), Value::Null));
        assert_eq!(j["original"], sent);
    }

    #[test]
    fn silence_is_never_uploaded() {
        // No server at all: reaching the network would fail differently.
        let cfg = Config::default();
        let opts = SttOptions { model: "whisper-1".into(), timeout_ms: 500, ..SttOptions::default() };
        let creds = Credentials { api_key: Some("test-key-123456".into()), base_url: Some("http://127.0.0.1:1".into()), extra: vec![] };
        let e = transcribe_audio(&cfg, &fixture("silence_2s.wav"), &opts, &creds).unwrap_err();
        assert!(e.contains("no speech"), "{e}");
    }

    #[test]
    fn summary_reports_median_and_local_overhead() {
        let mk = |stt: f64, total: f64| Sample { stt, total, hotkey_pill: 2.0, hotkey_audio: -1.0, ..Sample::default() };
        let v = summarize(&[mk(100.0, 130.0), mk(200.0, 235.0), mk(300.0, 340.0)]);
        assert_eq!(v["count"], 3);
        assert_eq!(v["stt"]["median"], 200.0);
        assert_eq!(v["local_overhead"]["median"], 35.0);
        assert!(v["local_before_paste"].is_object());
        assert!(v["hotkey_audio"].is_null(), "negative (missing) values are skipped");
    }

    #[test]
    fn flags() {
        let a: Vec<String> = ["x.wav", "--provider", "groq"].iter().map(|s| s.to_string()).collect();
        assert_eq!(flag_value(&a, "--provider"), Some("groq"));
        assert_eq!(flag_value(&a, "--model"), None);
    }
}
