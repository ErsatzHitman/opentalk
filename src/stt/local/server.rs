//! The sherpa-onnx websocket server that runs the chosen local model, and the client for it.
//!
//! One server process at a time, started for the selected model (and, for multilingual Whisper,
//! the chosen language) and kept warm, so only the first dictation after a start waits for the
//! model to load. It listens on 127.0.0.1 only, on a port picked at start. The process belongs to
//! a kill-on-close job object: if OpenTalk exits or crashes, Windows ends the server too.
//!
//! Protocols (sherpa-onnx `offline-websocket-server-impl.cc`, `online-websocket-server-impl.cc`):
//! - offline: binary `[i32 sample_rate][i32 byte_count][f32 samples...]`, which may span several
//!   frames; the server answers one JSON text (`{"text": ...}`); "Done" ends the connection.
//! - online: binary frames of f32 samples while recording, then "Done"; the server sends JSON
//!   results (`text`, `segment`, `is_final`) as it decodes and "Done!" when it has finished.
use super::catalog::{Kind, LocalModel};
use super::install::{self, Manifest};
use crate::stt::{SttError, SttErrorCode, StreamingSession, Transcript};
use std::io::{BufRead, BufReader};
use std::net::{SocketAddr, TcpStream};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::sync::mpsc;
use std::time::{Duration, Instant};
use tungstenite::{Message, WebSocket};

/// Loading a 1-2 GB model from a slow disk can take a while.
const START_TIMEOUT: Duration = Duration::from_secs(180);
/// Audio per binary frame (1 MiB of f32 samples).
const FRAME_BYTES: usize = 1 << 20;

fn err(code: SttErrorCode, message: impl Into<String>, detail: impl Into<String>) -> SttError {
    SttError::new(code, message).with_detail(detail)
}

/// What the running server was started for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Key {
    pub model: String,
    /// Whisper language ("" = detect); empty for the other models.
    pub language: String,
}

struct Server {
    key: Key,
    port: u16,
    child: Child,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The running server. Held only for quick looks and swaps, never while a model loads, so the UI
/// and daemon threads can ask what is running without waiting.
static SERVER: Mutex<Option<Server>> = Mutex::new(None);
/// Serialises starts: a second caller waits here for the first one's model instead of loading
/// another copy.
static START: Mutex<()> = Mutex::new(());

/// ONNX intra-op threads for one decode: three quarters of the cores, at most four (more threads
/// stop helping a single stream and starve the rest of the machine).
pub fn threads() -> usize {
    let cores = std::thread::available_parallelism().map_or(2, |n| n.get());
    (cores * 3 / 4).clamp(1, 4)
}

/// The language Whisper is told to expect: "" detects it; English-only models need none.
pub fn whisper_language(m: &LocalModel, language: &str) -> String {
    let l = language.trim();
    if m.kind != Kind::Whisper || m.english_only || l.is_empty() || l.eq_ignore_ascii_case("auto") {
        return String::new();
    }
    l.split(['-', '_']).next().unwrap_or("").to_ascii_lowercase()
}

/// The server executable and its arguments for `m` (paths from `manifest` inside `dir`).
pub fn command(m: &LocalModel, manifest: &Manifest, dir: &std::path::Path, language: &str, port: u16) -> Result<(&'static str, Vec<String>), String> {
    let file = |role: &str| manifest.path(dir, role).map(|p| p.to_string_lossy().into_owned()).ok_or_else(|| format!("the model has no {role} file"));
    let mut args = Vec::new();
    let mut flag = |name: &str, value: String| args.push(format!("--{name}={value}"));
    let exe = match m.kind {
        Kind::Whisper => {
            flag("whisper-encoder", file("encoder")?);
            flag("whisper-decoder", file("decoder")?);
            flag("tokens", file("tokens")?);
            flag("whisper-language", whisper_language(m, language));
            flag("whisper-task", "transcribe".into());
            install::OFFLINE_SERVER
        }
        Kind::Moonshine => {
            flag("moonshine-preprocessor", file("preprocessor")?);
            flag("moonshine-encoder", file("encoder")?);
            flag("moonshine-uncached-decoder", file("uncached_decoder")?);
            flag("moonshine-cached-decoder", file("cached_decoder")?);
            flag("tokens", file("tokens")?);
            install::OFFLINE_SERVER
        }
        Kind::NemoTransducer => {
            flag("encoder", file("encoder")?);
            flag("decoder", file("decoder")?);
            flag("joiner", file("joiner")?);
            flag("tokens", file("tokens")?);
            flag("model-type", "nemo_transducer".into());
            install::OFFLINE_SERVER
        }
        Kind::Qwen3Asr => {
            flag("qwen3-asr-conv-frontend", file("conv_frontend")?);
            flag("qwen3-asr-encoder", file("encoder")?);
            flag("qwen3-asr-decoder", file("decoder")?);
            flag("qwen3-asr-tokenizer", file("tokenizer")?);
            flag("qwen3-asr-max-new-tokens", "512".into());
            install::OFFLINE_SERVER
        }
        #[cfg(test)]
        Kind::TestCtc => {
            flag("tdnn-model", file("model")?);
            flag("tokens", file("tokens")?);
            install::OFFLINE_SERVER
        }
        Kind::StreamingTransducer => {
            flag("encoder", file("encoder")?);
            flag("decoder", file("decoder")?);
            flag("joiner", file("joiner")?);
            flag("tokens", file("tokens")?);
            // the 10 ms default decode tick adds idle time to a faster-than-real-time decode
            flag("loop-interval-ms", "2".into());
            // must cover the model's 560 ms chunk so the flush decodes the last words
            flag("end-tail-padding", "0.6".into());
            install::ONLINE_SERVER
        }
    };
    let threads = threads().to_string();
    flag("num-threads", threads);
    flag("port", port.to_string());
    if exe == install::OFFLINE_SERVER {
        flag("num-work-threads", "1".into());
        // the client splits long audio itself; this only has to admit the longest piece
        flag("max-utterance-length", "900".into());
    } else {
        flag("num-work-threads", "1".into());
    }
    Ok((exe, args))
}

fn free_port() -> Result<u16, SttError> {
    std::net::TcpListener::bind("127.0.0.1:0")
        .and_then(|l| l.local_addr())
        .map(|a| a.port())
        .map_err(|e| err(SttErrorCode::Unavailable, "The local model could not start.", format!("no free port: {e}")))
}

/// A kill-on-close job object for the server process (created once, held for the app's life).
fn job() -> Option<windows::Win32::Foundation::HANDLE> {
    use std::sync::OnceLock;
    use windows::Win32::System::JobObjects::{
        CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation, SetInformationJobObject,
    };
    static JOB: OnceLock<Option<usize>> = OnceLock::new();
    JOB.get_or_init(|| unsafe {
        let h = CreateJobObjectW(None, None).ok()?;
        let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        SetInformationJobObject(h, JobObjectExtendedLimitInformation, &info as *const _ as *const _, std::mem::size_of_val(&info) as u32).ok()?;
        Some(h.0 as usize)
    })
    .map(|h| windows::Win32::Foundation::HANDLE(h as *mut _))
}

fn spawn(m: &LocalModel, manifest: &Manifest, key: &Key) -> Result<Server, SttError> {
    use std::os::windows::io::AsRawHandle;
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    if !install::runtime_ready() {
        return Err(err(SttErrorCode::NotConfigured, "The local engine is not installed. Download a model in Settings > Speech-to-Text.", "runtime missing"));
    }
    let port = free_port()?;
    let dir = install::model_dir(m.id);
    let (exe, args) = command(m, manifest, &dir, &key.language, port).map_err(|e| err(SttErrorCode::NotConfigured, format!("{} is incomplete. Download it again.", m.name), e))?;
    let runtime = install::runtime_dir();
    let mut child = Command::new(runtime.join(exe))
        .args(&args)
        .current_dir(&runtime)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .map_err(|e| err(SttErrorCode::Unavailable, "The local model could not start.", format!("spawn {exe}: {e}")))?;
    if let Some(job) = job() {
        let process = windows::Win32::Foundation::HANDLE(child.as_raw_handle());
        let _ = unsafe { windows::Win32::System::JobObjects::AssignProcessToJobObject(job, process) };
    }
    // Watch stderr for "Listening on:", then keep draining it so the server never blocks on a
    // full pipe. The last lines explain a failed start.
    let stderr = child.stderr.take().expect("stderr is piped");
    let (tx, rx) = mpsc::channel::<Result<(), String>>();
    let name = m.id.to_string();
    let _ = std::thread::Builder::new().name("local-stt-log".into()).spawn(move || {
        let mut tail: Vec<String> = Vec::new();
        let mut ready = false;
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            let line = line.trim().to_string();
            if !ready && line.contains("Listening on:") {
                ready = true;
                let _ = tx.send(Ok(()));
            }
            if !line.is_empty() && !line.contains("winediag") {
                crate::log_debug!("local-stt[{name}] {}", line.chars().take(300).collect::<String>());
                tail.push(line);
                if tail.len() > 6 {
                    tail.remove(0);
                }
            }
        }
        if !ready {
            let _ = tx.send(Err(tail.join(" | ")));
        }
    });
    let started = Instant::now();
    let outcome = rx.recv_timeout(START_TIMEOUT);
    let mut server = Server { key: key.clone(), port, child };
    match outcome {
        Ok(Ok(())) => {
            crate::log_info!("local model {} ready on port {port} in {:.1} s", m.id, started.elapsed().as_secs_f32());
            Ok(server)
        }
        Ok(Err(tail)) => {
            let code = server.child.wait().ok().and_then(|s| s.code());
            Err(err(SttErrorCode::Unavailable, format!("{} could not start. Try downloading it again.", m.name), format!("exit {code:?}: {tail}")))
        }
        Err(_) => {
            drop(server);
            Err(err(SttErrorCode::Timeout, format!("{} took too long to load.", m.name), "no 'Listening on' within the start timeout"))
        }
    }
}

/// The port of a running server for `m` (started now if needed, replacing any other).
pub fn ensure(m: &LocalModel, manifest: &Manifest, language: &str) -> Result<u16, SttError> {
    let key = Key { model: m.id.to_string(), language: whisper_language(m, language) };
    let _starting = START.lock().unwrap_or_else(|p| p.into_inner());
    let old = {
        let mut slot = SERVER.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(s) = slot.as_mut() {
            let alive = matches!(s.child.try_wait(), Ok(None));
            if alive && s.key == key {
                return Ok(s.port);
            }
            if !alive {
                crate::log_warn!("the local model server for {} had exited; restarting it", s.key.model);
            }
        }
        slot.take()
    };
    drop(old); // stop the old one first: two large models may not fit in memory together
    let server = spawn(m, manifest, &key)?;
    let port = server.port;
    *SERVER.lock().unwrap_or_else(|p| p.into_inner()) = Some(server);
    Ok(port)
}

/// Stop the server (engine switched to the cloud, model removed, app exit).
pub fn stop() {
    let old = SERVER.lock().unwrap_or_else(|p| p.into_inner()).take();
    if let Some(s) = old {
        crate::log_info!("stopping the local model server ({})", s.key.model);
    }
}

/// The model the running server holds, if any.
pub fn running() -> Option<Key> {
    SERVER.lock().unwrap_or_else(|p| p.into_inner()).as_ref().map(|s| s.key.clone())
}

// --- client -------------------------------------------------------------------------------------

fn lost(detail: impl std::fmt::Display) -> SttError {
    err(SttErrorCode::Unavailable, "The local model stopped answering. Try again.", detail.to_string())
}

fn connect(port: u16, read_timeout: Duration) -> Result<WebSocket<TcpStream>, SttError> {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let tcp = TcpStream::connect_timeout(&addr, Duration::from_secs(3)).map_err(lost)?;
    let _ = tcp.set_nodelay(true);
    tcp.set_read_timeout(Some(read_timeout)).map_err(lost)?;
    tcp.set_write_timeout(Some(Duration::from_secs(30))).map_err(lost)?;
    let (ws, _) = tungstenite::client(format!("ws://127.0.0.1:{port}"), tcp).map_err(|e| lost(format!("handshake: {e}")))?;
    Ok(ws)
}

pub fn to_f32(pcm: &[i16]) -> Vec<f32> {
    pcm.iter().map(|&s| s as f32 / 32768.0).collect()
}

fn f32_bytes(samples: &[f32]) -> Vec<u8> {
    let mut v = Vec::with_capacity(samples.len() * 4);
    for s in samples {
        v.extend_from_slice(&s.to_le_bytes());
    }
    v
}

/// The `text` of a sherpa-onnx result (the whole message when it is not JSON).
pub fn result_text(msg: &str) -> String {
    match serde_json::from_str::<serde_json::Value>(msg) {
        Ok(v) => v.get("text").and_then(|t| t.as_str()).unwrap_or("").trim().to_string(),
        Err(_) => msg.trim().to_string(),
    }
}

fn timed_out(e: &tungstenite::Error) -> bool {
    matches!(e, tungstenite::Error::Io(io) if matches!(io.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut))
}

/// Transcribe one piece of audio with the offline server on `port`.
pub fn decode_offline(port: u16, pcm: &[i16], rate: u32) -> Result<String, SttError> {
    // generous: a large model on a slow CPU runs at about real time
    let secs = pcm.len() as u64 / rate.max(1) as u64;
    let timeout = Duration::from_secs(30 + secs * 3);
    let mut ws = connect(port, timeout)?;
    let body = f32_bytes(&to_f32(pcm));
    let mut first = Vec::with_capacity(8 + body.len().min(FRAME_BYTES));
    first.extend_from_slice(&(rate as i32).to_le_bytes());
    first.extend_from_slice(&(body.len() as i32).to_le_bytes());
    let head = body.len().min(FRAME_BYTES);
    first.extend_from_slice(&body[..head]);
    ws.send(Message::Binary(first.into())).map_err(lost)?;
    for chunk in body[head..].chunks(FRAME_BYTES) {
        ws.send(Message::Binary(chunk.to_vec().into())).map_err(lost)?;
    }
    let text = loop {
        match ws.read() {
            Ok(Message::Text(t)) => break result_text(t.as_str()),
            Ok(Message::Close(frame)) => return Err(lost(format!("closed: {}", frame.map(|f| f.reason.to_string()).unwrap_or_default()))),
            Ok(_) => continue,
            Err(e) if timed_out(&e) => return Err(err(SttErrorCode::Timeout, "The local model took too long.", e.to_string())),
            Err(e) => return Err(lost(e)),
        }
    };
    let _ = ws.send(Message::Text("Done".into()));
    let _ = ws.close(None);
    Ok(text)
}

/// Accumulates online results: the latest text of each segment, in order.
#[derive(Default)]
pub struct Segments {
    order: Vec<i64>,
    text: std::collections::HashMap<i64, String>,
}

impl Segments {
    pub fn push(&mut self, msg: &str) {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(msg) else { return };
        let seg = v.get("segment").and_then(|s| s.as_i64()).unwrap_or(0);
        let text = v.get("text").and_then(|t| t.as_str()).unwrap_or("").trim().to_string();
        if !self.text.contains_key(&seg) {
            self.order.push(seg);
        }
        self.text.insert(seg, text);
    }

    pub fn text(&self) -> String {
        self.order.iter().filter_map(|s| self.text.get(s)).filter(|t| !t.is_empty()).cloned().collect::<Vec<_>>().join(" ")
    }
}

enum Cmd {
    Audio(Vec<i16>),
    Finish(mpsc::Sender<Result<String, SttError>>),
    Cancel,
}

/// A dictation streamed to the online server while it is recorded.
pub struct OnlineSession {
    tx: mpsc::Sender<Cmd>,
    stage: Vec<i16>,
}

/// Samples gathered before a frame goes out (100 ms at 16 kHz).
const STAGE: usize = 1600;

impl OnlineSession {
    pub fn open(port: u16) -> Result<OnlineSession, SttError> {
        let mut ws = connect(port, Duration::from_secs(15))?;
        let (tx, rx) = mpsc::channel::<Cmd>();
        std::thread::Builder::new()
            .name("local-stt-stream".into())
            .spawn(move || {
                let mut failed: Option<SttError> = None;
                while let Ok(cmd) = rx.recv() {
                    match cmd {
                        Cmd::Audio(pcm) if failed.is_none() => {
                            if let Err(e) = ws.send(Message::Binary(f32_bytes(&to_f32(&pcm)).into())) {
                                failed = Some(lost(e));
                            }
                        }
                        Cmd::Audio(_) => {}
                        Cmd::Finish(reply) => {
                            let out = match failed.take() {
                                Some(e) => Err(e),
                                None => finish_online(&mut ws),
                            };
                            let _ = reply.send(out);
                            break;
                        }
                        Cmd::Cancel => {
                            let _ = ws.close(None);
                            break;
                        }
                    }
                }
            })
            .map_err(|e| lost(format!("thread: {e}")))?;
        Ok(OnlineSession { tx, stage: Vec::with_capacity(STAGE) })
    }
}

fn finish_online(ws: &mut WebSocket<TcpStream>) -> Result<String, SttError> {
    ws.send(Message::Text("Done".into())).map_err(lost)?;
    let mut segs = Segments::default();
    loop {
        match ws.read() {
            Ok(Message::Text(t)) if t.as_str() == "Done!" => break,
            Ok(Message::Text(t)) => segs.push(t.as_str()),
            Ok(Message::Close(_)) => break,
            Ok(_) => {}
            Err(e) if timed_out(&e) => return Err(err(SttErrorCode::Timeout, "The local model took too long.", e.to_string())),
            Err(tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed) => break,
            Err(e) => return Err(lost(e)),
        }
    }
    let _ = ws.close(None);
    Ok(segs.text())
}

impl StreamingSession for OnlineSession {
    fn push(&mut self, pcm: &[i16]) {
        self.stage.extend_from_slice(pcm);
        if self.stage.len() >= STAGE {
            let _ = self.tx.send(Cmd::Audio(std::mem::take(&mut self.stage)));
        }
    }

    fn finish(mut self: Box<Self>) -> Result<Transcript, SttError> {
        if !self.stage.is_empty() {
            let _ = self.tx.send(Cmd::Audio(std::mem::take(&mut self.stage)));
        }
        let (reply, answer) = mpsc::channel();
        self.tx.send(Cmd::Finish(reply)).map_err(|_| lost("stream thread gone"))?;
        let text = answer.recv_timeout(Duration::from_secs(60)).map_err(|_| err(SttErrorCode::Timeout, "The local model took too long.", "no final result"))??;
        if text.trim().is_empty() {
            return Err(crate::stt::errors::no_speech());
        }
        Ok(Transcript { text: text.trim().to_string(), detected_language: None })
    }

    fn cancel(self: Box<Self>) {
        let _ = self.tx.send(Cmd::Cancel);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stt::local::catalog;
    use std::collections::BTreeMap;
    use std::net::TcpListener;

    fn manifest(roles: &[&str]) -> Manifest {
        Manifest { id: "m".into(), files: roles.iter().map(|r| (r.to_string(), format!("{r}.bin"))).collect::<BTreeMap<_, _>>() }
    }

    #[test]
    fn command_lines_per_kind() {
        let dir = std::path::Path::new("C:\\m");
        let turbo = catalog::get("whisper-large-v3-turbo").unwrap();
        let (exe, args) = command(turbo, &manifest(&["encoder", "decoder", "tokens"]), dir, "de-DE", 7001).unwrap();
        assert_eq!(exe, install::OFFLINE_SERVER);
        assert!(args.contains(&"--whisper-encoder=C:\\m\\encoder.bin".to_string()), "{args:?}");
        assert!(args.contains(&"--whisper-language=de".to_string()));
        assert!(args.contains(&"--port=7001".to_string()));
        assert!(args.contains(&"--max-utterance-length=900".to_string()));

        let distil = catalog::get("distil-whisper-small.en").unwrap();
        let (_, args) = command(distil, &manifest(&["encoder", "decoder", "tokens"]), dir, "de", 1).unwrap();
        assert!(args.contains(&"--whisper-language=".to_string()), "English-only models are not told a language");

        let parakeet = catalog::get("parakeet-tdt-0.6b-v3").unwrap();
        let (_, args) = command(parakeet, &manifest(&["encoder", "decoder", "joiner", "tokens"]), dir, "auto", 1).unwrap();
        assert!(args.contains(&"--model-type=nemo_transducer".to_string()));

        let qwen = catalog::get("qwen3-asr-0.6b").unwrap();
        let (_, args) = command(qwen, &manifest(&["conv_frontend", "encoder", "decoder", "tokenizer"]), dir, "auto", 1).unwrap();
        assert!(args.iter().any(|a| a.starts_with("--qwen3-asr-tokenizer=")));

        let moon = catalog::get("moonshine-tiny").unwrap();
        let (_, args) = command(moon, &manifest(&["preprocessor", "encoder", "uncached_decoder", "cached_decoder", "tokens"]), dir, "auto", 1).unwrap();
        assert!(args.iter().any(|a| a.starts_with("--moonshine-cached-decoder=")));

        let nemo = catalog::get("nemotron-3.5-asr-streaming-0.6b").unwrap();
        let (exe, args) = command(nemo, &manifest(&["encoder", "decoder", "joiner", "tokens"]), dir, "auto", 1).unwrap();
        assert_eq!(exe, install::ONLINE_SERVER);
        assert!(args.contains(&"--end-tail-padding=0.6".to_string()));
        assert!(!args.iter().any(|a| a.starts_with("--model-type")), "the online server takes no model type");

        assert!(command(parakeet, &manifest(&["encoder"]), dir, "auto", 1).is_err(), "a missing file is an error");
    }

    #[test]
    fn whisper_language_codes() {
        let small = catalog::get("whisper-small").unwrap();
        assert_eq!(whisper_language(small, "auto"), "");
        assert_eq!(whisper_language(small, ""), "");
        assert_eq!(whisper_language(small, "pt-BR"), "pt");
        assert_eq!(whisper_language(catalog::get("parakeet-tdt-0.6b-v3").unwrap(), "de"), "", "only Whisper takes a language");
        assert!((1..=4).contains(&threads()));
    }

    #[test]
    fn online_results_keep_the_latest_text_per_segment() {
        let mut s = Segments::default();
        s.push(r#"{"text": "hel", "segment": 0, "is_final": false}"#);
        s.push(r#"{"text": "hello", "segment": 0, "is_final": true}"#);
        s.push(r#"{"text": "wor", "segment": 1, "is_final": false}"#);
        s.push(r#"{"text": "world", "segment": 1, "is_final": false}"#);
        s.push("not json");
        assert_eq!(s.text(), "hello world");
        assert_eq!(result_text(r#"{"lang": "", "text": " hi there ", "tokens": []}"#), "hi there");
        assert_eq!(result_text("plain"), "plain");
    }

    /// A one-connection websocket server speaking the sherpa-onnx protocols, for the client tests.
    fn mock(online: bool) -> (u16, std::thread::JoinHandle<Vec<f32>>) {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let h = std::thread::spawn(move || {
            let (tcp, _) = l.accept().unwrap();
            let mut ws = tungstenite::accept(tcp).unwrap();
            let mut got: Vec<u8> = Vec::new();
            let mut expected = None;
            loop {
                match ws.read() {
                    Ok(Message::Binary(b)) => {
                        if online {
                            got.extend_from_slice(&b);
                            let n = got.len() / 4;
                            let _ = ws.send(Message::Text(format!(r#"{{"text": "partial {n}", "segment": 0, "is_final": false}}"#).into()));
                        } else {
                            if expected.is_none() {
                                let rate = i32::from_le_bytes(b[0..4].try_into().unwrap());
                                assert_eq!(rate, 16_000);
                                expected = Some(i32::from_le_bytes(b[4..8].try_into().unwrap()) as usize);
                                got.extend_from_slice(&b[8..]);
                            } else {
                                got.extend_from_slice(&b);
                            }
                            if Some(got.len()) == expected {
                                let _ = ws.send(Message::Text(r#"{"lang": "", "text": "hello world", "timestamps": []}"#.into()));
                            }
                        }
                    }
                    Ok(Message::Text(t)) if t.as_str() == "Done" => {
                        if online {
                            let _ = ws.send(Message::Text(r#"{"text": "hello", "segment": 0, "is_final": true}"#.into()));
                            let _ = ws.send(Message::Text(r#"{"text": "world", "segment": 1, "is_final": true}"#.into()));
                            let _ = ws.send(Message::Text("Done!".into()));
                        }
                        let _ = ws.close(None);
                    }
                    Ok(_) => {}
                    Err(_) => break,
                }
            }
            got.chunks_exact(4).map(|c| f32::from_le_bytes(c.try_into().unwrap())).collect()
        });
        (port, h)
    }

    #[test]
    fn offline_protocol_round_trip_over_several_frames() {
        let (port, h) = mock(false);
        // 40 s of audio: more than one 1 MiB frame
        let pcm: Vec<i16> = (0..16_000 * 40).map(|i| ((i % 200) as i16 - 100) * 100).collect();
        assert_eq!(decode_offline(port, &pcm, 16_000).unwrap(), "hello world");
        let got = h.join().unwrap();
        assert_eq!(got.len(), pcm.len(), "every sample arrived");
        assert!((got[1] - pcm[1] as f32 / 32768.0).abs() < 1e-6, "as f32 in -1..1");
    }

    #[test]
    fn online_session_streams_and_joins_final_segments() {
        let (port, h) = mock(true);
        let mut s: Box<dyn StreamingSession> = Box::new(OnlineSession::open(port).unwrap());
        for _ in 0..30 {
            s.push(&[1000i16; 320]);
        }
        let t = s.finish().unwrap();
        assert_eq!(t.text, "hello world");
        assert_eq!(h.join().unwrap().len(), 30 * 320, "staged audio is flushed before Done");
    }

    #[test]
    fn a_dead_port_is_unavailable() {
        let port = free_port().unwrap();
        let e = decode_offline(port, &[0i16; 1600], 16_000).unwrap_err();
        assert_eq!(e.code, SttErrorCode::Unavailable);
    }
}
