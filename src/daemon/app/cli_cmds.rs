//! Headless command-line modes: `--transcribe-file` (the full text pipeline without a microphone)
//! and `--bench ...` (helpers driven by `bench/bench.ps1` and `tests/e2e.ps1`).
//!
//! ```text
//! --transcribe-file <wav> [--provider id] [--model m] [--llm | --no-llm]
//! --bench dictate <n> <wav>        toggle a running daemon twice per round, wait for history
//! --bench latency [n]              print the daemon log's latency lines + median/p95 summary
//! --bench toggle | cancel | quit | settings   one IPC message to the running daemon
//! --bench wait-ready [secs]        block until the daemon window exists
//! --bench mock-stt [--port P] [--delay-ms D] [--text T]   local STT server: OpenAI/Deepgram REST
//!                                  replies over HTTP, Deepgram live protocol over ws:// (same port)
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
}

impl FileResult {
    fn to_json(&self) -> Value {
        json!({
            "raw": self.raw, "text": self.text, "provider": self.provider, "model": self.model,
            "stt_ms": round1(self.stt_ms), "cleanup_ms": round1(self.cleanup_ms), "total_ms": round1(self.total_ms),
            "ai_cleaned": self.ai_cleaned,
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
    })
}

pub fn transcribe_file(args: &[String]) -> i32 {
    let Some(path) = args.first().filter(|a| !a.starts_with("--")) else {
        eprintln!("usage: opentalk --transcribe-file <wav> [--provider id] [--model m] [--llm | --no-llm]");
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
        Some("cancel") => send(IpcMsg::Cancel),
        Some("quit") => send(IpcMsg::Quit),
        Some("settings") => send(IpcMsg::OpenSettings),
        Some("wait-ready") => bench_wait_ready(&args[1..]),
        Some("mock-stt") => bench_mock(&args[1..]),
        Some("info") => bench_info(),
        _ => {
            eprintln!("usage: opentalk --bench dictate <n> <wav> | latency [n] | toggle | cancel | quit | wait-ready [secs] | mock-stt [--port P] [--delay-ms D] [--text T]");
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

/// `--bench dictate <n> <wav>`: needs a daemon started with `OPENTALK_INJECT_WAV=<wav>`.
fn bench_dictate(args: &[String]) -> i32 {
    let (Some(n), Some(wav)) = (args.first().and_then(|s| s.parse::<u32>().ok()), args.get(1)) else {
        eprintln!("usage: opentalk --bench dictate <n> <wav>");
        return 2;
    };
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
    for i in 1..=n {
        let before = history::load_all().len();
        let started = Instant::now();
        if !ipc::notify_daemon(IpcMsg::Toggle) {
            eprintln!("daemon went away");
            return 1;
        }
        // The injected file plays in real time; leave a little tail so the VAD sees its end.
        std::thread::sleep(Duration::from_millis(wav_ms + 600));
        let stop = Instant::now();
        ipc::notify_daemon(IpcMsg::Toggle);
        let mut status = "timeout".to_string();
        while stop.elapsed() < Duration::from_secs(45) {
            let all = history::load_all();
            if all.len() > before {
                status = format!("{:?}", all[0].status).to_ascii_lowercase();
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let ok = status == "ok";
        failures += (!ok) as i32;
        print_json(&json!({
            "round": i, "status": status,
            "recorded_ms": round1(stop.duration_since(started).as_secs_f64() * 1000.0),
            "stop_to_history_ms": round1(stop.elapsed().as_secs_f64() * 1000.0),
        }));
        std::thread::sleep(Duration::from_millis(400));
    }
    (failures > 0) as i32
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
    serve_forever(listener, delay, Arc::new(text));
    0
}

/// Accept connections forever; one thread per connection (keep-alive aware). A WebSocket
/// upgrade gets the Deepgram live protocol, anything else plain HTTP.
pub fn serve_forever(listener: TcpListener, delay: Duration, text: Arc<String>) {
    for conn in listener.incoming().flatten() {
        let text = text.clone();
        std::thread::spawn(move || {
            if is_websocket_upgrade(&conn) {
                serve_ws(conn, delay, &text);
            } else {
                let _ = serve_connection(conn, delay, &text);
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

/// Minimal HTTP/1.1 server loop: POSTs get `{"text": ...}` after `delay`, anything else an empty 200.
fn serve_connection(stream: TcpStream, delay: Duration, text: &str) -> std::io::Result<()> {
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
        if chunked {
            loop {
                let mut size = String::new();
                reader.read_line(&mut size)?;
                let n = usize::from_str_radix(size.trim().split(';').next().unwrap_or("0"), 16).unwrap_or(0);
                std::io::copy(&mut reader.by_ref().take(n as u64 + 2), &mut std::io::sink())?;
                if n == 0 {
                    break;
                }
            }
        } else {
            std::io::copy(&mut reader.by_ref().take(len as u64), &mut std::io::sink())?;
        }
        let body = if method == "POST" {
            std::thread::sleep(delay);
            batch_body(text)
        } else {
            String::new()
        };
        let head = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: {}\r\n\r\n",
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
