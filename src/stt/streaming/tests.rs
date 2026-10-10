//! Streaming tests against in-process mock WebSocket servers (no network, no keys).
use super::test_support::{Conn, Mock, Options, serve, text_frame};
use super::*;
use crate::stt::{SttErrorCode, Transcript};
use base64::Engine;
use serde_json::{Value, json};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tungstenite::Message;

const KEY: &str = "test-key-123";

/// Tests that pin connection-loss behaviour (RST timing, cancel latency) take this lock: they
/// are sensitive to scheduling, and running them one at a time keeps a loaded machine from
/// stretching the windows they depend on.
static SERIAL: Mutex<()> = Mutex::new(());

fn serial() -> std::sync::MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}

type Log = Arc<Mutex<Vec<String>>>;

fn creds(m: &Mock) -> Credentials {
    Credentials { api_key: Some(KEY.into()), base_url: Some(m.ws_url()), extra: Vec::new() }
}

fn opts(model: &str) -> SttOptions {
    SttOptions { model: model.into(), language: None, hints: Vec::new(), timeout_ms: 5000 }
}

fn pcm(n: usize, amp: i16) -> Vec<i16> {
    (0..n).map(|i| if i % 2 == 0 { amp } else { -amp }).collect()
}

fn new_log() -> Log {
    Arc::new(Mutex::new(Vec::new()))
}

fn lines(log: &Log) -> Vec<String> {
    log.lock().unwrap().clone()
}

/// A completion signal the script fires when it returns, so tests can inspect its log.
fn signal() -> (Arc<Mutex<mpsc::Sender<()>>>, mpsc::Receiver<()>) {
    let (tx, rx) = mpsc::channel();
    (Arc::new(Mutex::new(tx)), rx)
}

fn fire(tx: &Arc<Mutex<mpsc::Sender<()>>>) {
    let _ = tx.lock().unwrap().send(());
}

fn wait(rx: &mpsc::Receiver<()>) {
    rx.recv_timeout(Duration::from_secs(30)).expect("mock script finished");
}

fn dg_results(text: &str, is_final: bool, from_finalize: bool) -> String {
    json!({"type":"Results","is_final":is_final,"from_finalize":from_finalize,"channel":{"alternatives":[{"transcript":text}]}}).to_string()
}

fn decode_pcm(b64: &str) -> Vec<i16> {
    let bytes = base64::engine::general_purpose::STANDARD.decode(b64).expect("valid base64");
    bytes.chunks_exact(2).map(|c| i16::from_le_bytes([c[0], c[1]])).collect()
}

fn feed(session: &mut Box<dyn StreamingSession>, samples: &[i16], chunk: usize) {
    for c in samples.chunks(chunk) {
        session.push(c);
    }
}

// ------------------------------------------------------------------------------------------
// registry gating

#[test]
fn get_returns_providers_for_streaming_models_only() {
    use SttProviderId::*;
    for (id, model, expect) in [
        (Deepgram, "nova-3", true),
        (Assemblyai, "universal-3-5-pro", true),
        (Assemblyai, "universal-streaming-english", true),
        (Assemblyai, "universal-streaming-multilingual", true),
        (Openai, "gpt-4o-mini-transcribe", true),
        (Openai, "gpt-4o-transcribe", true),
        (Openai, "gpt-transcribe", false),
        (Openai, "whisper-1", false),
        (Gemini, "gemini-3.5-transcribe-live", true),
        (Gemini, "gemini-3.5-transcribe", false),
        // registry says Corti streams, but there is no transport here: batch handles it
        (Corti, "corti-transcribe", false),
        (Tinfoil, "voxtral-small-24b", false),
        (Groq, "whisper-large-v3", false),
        (Custom, "whisper-1", false),
        (Openai, "no-such-model", false),
    ] {
        let got = get(id, model);
        assert_eq!(got.is_some(), expect, "{id:?}/{model}");
        if let Some(p) = got {
            assert_eq!(p.id(), id);
        }
    }
}

fn cfg_for(id: SttProviderId, model: &str, prefer: bool) -> Config {
    let mut cfg = Config::default();
    cfg.stt.provider = id;
    cfg.stt.models.insert(id, model.to_string());
    cfg.stt.prefer_streaming = prefer;
    cfg
}

fn start_cfg(cfg: &Config, c: Credentials) -> Option<Result<Box<dyn StreamingSession>, SttError>> {
    start_with(cfg, move |_| c.clone(), |_| SttOptions::default())
}

#[test]
fn start_requires_preference_unless_streaming_only() {
    let unused = Credentials::default();
    // streamable but not preferred -> batch
    assert!(start_cfg(&cfg_for(SttProviderId::Deepgram, "nova-3", false), unused.clone()).is_none());
    // preferred but the model has no streaming transport -> batch
    assert!(start_cfg(&cfg_for(SttProviderId::Openai, "whisper-1", true), unused.clone()).is_none());
    assert!(start_cfg(&cfg_for(SttProviderId::Groq, "whisper-large-v3", true), unused.clone()).is_none());
    // Corti/Tinfoil: unsupported even when preferred
    assert!(start_cfg(&cfg_for(SttProviderId::Corti, "corti-transcribe", true), unused.clone()).is_none());
    assert!(start_cfg(&cfg_for(SttProviderId::Tinfoil, "voxtral-small-24b", true), unused.clone()).is_none());
    // unknown model -> batch
    assert!(start_cfg(&cfg_for(SttProviderId::Deepgram, "nova-9", true), unused.clone()).is_none());
}

#[test]
fn start_reports_missing_key() {
    let r = start_cfg(&cfg_for(SttProviderId::Deepgram, "nova-3", true), Credentials::default()).expect("streams");
    assert_eq!(r.err().expect("no key").code, SttErrorCode::NotConfigured);
    // streaming-only models stream without the preference
    let r = start_cfg(&cfg_for(SttProviderId::Assemblyai, "universal-3-5-pro", false), Credentials::default()).expect("streaming-only");
    assert_eq!(r.err().expect("no key").code, SttErrorCode::NotConfigured);
    let r = start_cfg(&cfg_for(SttProviderId::Gemini, "gemini-3.5-transcribe-live", false), Credentials { api_key: Some("  ".into()), ..Default::default() })
        .expect("streaming-only");
    assert_eq!(r.err().expect("blank key").code, SttErrorCode::NotConfigured);
}

#[test]
fn start_uses_default_model_and_fills_empty_option_model() {
    let mock = serve(Options::default(), |c: &mut Conn| {
        let _ = c.recv(Duration::from_secs(1));
    });
    // no model chosen for AssemblyAI: the registry default (universal-3-5-pro, streaming-only) applies
    let mut cfg = Config::default();
    cfg.stt.provider = SttProviderId::Assemblyai;
    let c = creds(&mock);
    let session = start_with(&cfg, move |_| c.clone(), |_| SttOptions::default()).expect("streams").expect("starts");
    session.cancel();
}

#[test]
fn ws_base_override_rules() {
    // one test owns the environment variable, so parallel tests never race on it
    let plain = Credentials::default();
    assert_eq!(ws_url(&plain, DEFAULT_DEEPGRAM_ORIGIN, "/v1/listen?x=1"), "wss://api.deepgram.com/v1/listen?x=1");
    assert_eq!(http_url(&plain, DEFAULT_ASSEMBLYAI_ORIGIN, "/v3/token"), "https://streaming.assemblyai.com/v3/token");
    // an https batch URL in base_url is not a WebSocket origin
    let batch = Credentials { base_url: Some("https://api.openai.com/v1".into()), ..Default::default() };
    assert_eq!(ws_url(&batch, DEFAULT_OPENAI_ORIGIN, "/v1/realtime"), "wss://api.openai.com/v1/realtime");
    let over = Credentials { base_url: Some("ws://127.0.0.1:9/".into()), ..Default::default() };
    assert_eq!(ws_url(&over, DEFAULT_OPENAI_ORIGIN, "/v1/realtime"), "ws://127.0.0.1:9/v1/realtime");
    assert_eq!(http_url(&over, DEFAULT_ASSEMBLYAI_ORIGIN, "/v3/token"), "http://127.0.0.1:9/v3/token");

    // SAFETY: the only test touching this variable; other tests always set base_url.
    unsafe { std::env::set_var(WS_BASE_ENV, "ws://127.0.0.1:7") };
    assert_eq!(ws_url(&plain, DEFAULT_GEMINI_ORIGIN, "/p"), "ws://127.0.0.1:7/p");
    assert_eq!(ws_url(&over, DEFAULT_GEMINI_ORIGIN, "/p"), "ws://127.0.0.1:9/p", "credentials win over the environment");
    unsafe { std::env::remove_var(WS_BASE_ENV) };
}

// ------------------------------------------------------------------------------------------
// Deepgram over the generic engine

fn deepgram_flow_server(log: Log, tx: Arc<Mutex<mpsc::Sender<()>>>) -> Mock {
    serve(Options::default(), move |c| {
        let req = c.req.clone();
        log.lock().unwrap().push(format!("url:{}", req.target));
        log.lock().unwrap().push(format!("auth:{}", req.header("authorization").unwrap_or("")));
        let mut first_audio = true;
        while let Some(m) = c.recv(Duration::from_secs(5)) {
            match m {
                Message::Binary(b) => {
                    log.lock().unwrap().push(format!("bin:{}", b.len()));
                    if first_audio {
                        first_audio = false;
                        c.send(&dg_results("hel", false, false));
                        c.send(&dg_results("hello", true, false));
                        c.send(&dg_results("wor", false, false));
                    }
                }
                Message::Text(t) => {
                    log.lock().unwrap().push(format!("txt:{}", t.as_str()));
                    if t.as_str().contains("Finalize") {
                        c.send(&dg_results("world", true, true));
                    }
                    if t.as_str().contains("CloseStream") {
                        break;
                    }
                }
                _ => {}
            }
        }
        fire(&tx);
    })
}

#[test]
fn deepgram_handshake_frames_and_finalize_sequence() {
    let log = new_log();
    let (tx, rx) = signal();
    let mock = deepgram_flow_server(log.clone(), tx);
    let mut o = opts("nova-3");
    o.language = Some("en-US".into());
    o.hints = vec!["OpenTalk".into(), "Whispr".into()];
    let mut s = deepgram::Deepgram.start(&o, &creds(&mock)).expect("starts");
    // 2400 + 300 samples pushed in uneven pieces: three 50 ms frames and a 300-sample tail
    feed(&mut s, &pcm(2700, 1000), 450);
    let t0 = Instant::now();
    let out = s.finish().expect("transcript");
    let took = t0.elapsed();
    assert_eq!(out.text, "hello world");
    assert!(took < Duration::from_millis(900), "from_finalize ends the wait early, took {took:?}");
    wait(&rx);

    let l = lines(&log);
    assert_eq!(
        l[0],
        "url:/v1/listen?encoding=linear16&sample_rate=16000&channels=1&model=nova-3&punctuate=true&interim_results=true&language=en-US&keyterm=OpenTalk&keyterm=Whispr"
    );
    assert_eq!(l[1], format!("auth:Token {KEY}"));
    assert_eq!(&l[2..], &["bin:1600", "bin:1600", "bin:1600", "bin:600", r#"txt:{"type":"Finalize"}"#, r#"txt:{"type":"CloseStream"}"#]);
}

#[test]
fn deepgram_quiet_period_settles_without_finalize_reply() {
    let (tx, rx) = signal();
    let mock = serve(Options::default(), move |c| {
        let mut said = false;
        while let Some(m) = c.recv(Duration::from_secs(5)) {
            if let Message::Binary(_) = m {
                if !said {
                    said = true;
                    c.send(&dg_results("just one segment", true, false));
                }
            } else if let Message::Text(t) = m {
                if t.as_str().contains("CloseStream") {
                    break;
                }
            }
        }
        fire(&tx);
    });
    let mut s = deepgram::Deepgram.start(&opts("nova-3"), &creds(&mock)).unwrap();
    feed(&mut s, &pcm(1600, 500), 800);
    std::thread::sleep(Duration::from_millis(100));
    let t0 = Instant::now();
    let out = s.finish().unwrap();
    let took = t0.elapsed();
    assert_eq!(out.text, "just one segment");
    assert!(took >= Duration::from_millis(230), "waits for the 250 ms quiet period, took {took:?}");
    assert!(took < Duration::from_millis(1000), "does not wait for the ceiling, took {took:?}");
    wait(&rx);
}

#[test]
fn deepgram_outstanding_partial_waits_for_the_ceiling_and_is_the_fallback() {
    let mock = serve(Options::default(), |c| {
        let mut said = false;
        while let Some(m) = c.recv(Duration::from_secs(6)) {
            if let Message::Binary(_) = m {
                if !said {
                    said = true;
                    c.send(&dg_results("only a partial", false, false));
                }
            }
        }
    });
    let mut s = deepgram::Deepgram.start(&opts("nova-3"), &creds(&mock)).unwrap();
    feed(&mut s, &pcm(1600, 500), 800);
    std::thread::sleep(Duration::from_millis(100));
    let t0 = Instant::now();
    let out = s.finish().unwrap();
    let took = t0.elapsed();
    assert_eq!(out.text, "only a partial");
    assert!(took >= Duration::from_millis(1900) && took < Duration::from_millis(2600), "hard ceiling is 2000 ms, took {took:?}");
}

#[test]
fn silent_server_yields_empty_text_at_the_quiet_period() {
    let mock = serve(Options::default(), |c| while c.recv(Duration::from_secs(4)).is_some() {});
    let mut s = deepgram::Deepgram.start(&opts("nova-3"), &creds(&mock)).unwrap();
    feed(&mut s, &pcm(800, 0), 800);
    let out = s.finish().unwrap();
    assert_eq!(out.text, "");
}

#[test]
fn finish_without_any_audio_returns_immediately() {
    let mock = serve(Options::default(), |c| while c.recv(Duration::from_secs(4)).is_some() {});
    let s = deepgram::Deepgram.start(&opts("nova-3"), &creds(&mock)).unwrap();
    let t0 = Instant::now();
    assert_eq!(s.finish().unwrap().text, "");
    assert!(t0.elapsed() < Duration::from_millis(500));
}

#[test]
fn audio_before_the_socket_is_open_is_capped_at_three_seconds() {
    let log = new_log();
    let l2 = log.clone();
    let (tx, rx) = signal();
    let mock = serve(Options { accept_delay: Duration::from_millis(400), ..Options::default() }, move |c| {
        let mut bytes = 0usize;
        while let Some(m) = c.recv(Duration::from_secs(5)) {
            match m {
                Message::Binary(b) => bytes += b.len(),
                Message::Text(t) if t.as_str().contains("Finalize") => c.send(&dg_results("", true, true)),
                Message::Text(t) if t.as_str().contains("CloseStream") => break,
                _ => {}
            }
        }
        l2.lock().unwrap().push(format!("bytes:{bytes}"));
        fire(&tx);
    });
    let mut s = deepgram::Deepgram.start(&opts("nova-3"), &creds(&mock)).unwrap();
    // 20 s of audio arrives while the handshake is still pending
    let t0 = Instant::now();
    feed(&mut s, &pcm(80_000, 800), 800);
    assert!(t0.elapsed() < Duration::from_millis(100), "push must never block the capture thread");
    std::thread::sleep(Duration::from_millis(1200));
    s.push(&pcm(800, 800)); // after the socket opened everything goes through
    s.finish().unwrap();
    wait(&rx);
    assert_eq!(lines(&log), vec![format!("bytes:{}", (48_000 + 800) * 2)]);
}

#[test]
fn deepgram_sends_keepalive_when_the_audio_stalls() {
    let log = new_log();
    let l2 = log.clone();
    let (tx, rx) = signal();
    let mock = serve(Options::default(), move |c| {
        while let Some(m) = c.recv(Duration::from_secs(6)) {
            if let Message::Text(t) = m {
                l2.lock().unwrap().push(t.as_str().to_string());
                if t.as_str().contains("KeepAlive") {
                    break;
                }
            }
        }
        fire(&tx);
    });
    let mut s = deepgram::Deepgram.start(&opts("nova-3"), &creds(&mock)).unwrap();
    s.push(&pcm(800, 100));
    let t0 = Instant::now();
    wait(&rx);
    let waited = t0.elapsed();
    assert!(waited >= Duration::from_millis(2800) && waited < Duration::from_secs(5), "KeepAlive every 3 s of silence, got {waited:?}");
    assert_eq!(lines(&log), vec![r#"{"type":"KeepAlive"}"#]);
    s.cancel();
}

#[test]
fn server_frame_split_across_tcp_segments_is_reassembled() {
    let mock = serve(Options::default(), |c| {
        let mut sent = false;
        while let Some(m) = c.recv(Duration::from_secs(5)) {
            if let Message::Text(t) = m {
                if t.as_str().contains("Finalize") && !sent {
                    sent = true;
                    let frame = text_frame(&dg_results("split frame", true, true));
                    // header + part of the payload, a pause longer than several socket polls, the rest
                    c.raw(&frame[..9]);
                    std::thread::sleep(Duration::from_millis(120));
                    c.raw(&frame[9..]);
                }
            }
        }
    });
    let mut s = deepgram::Deepgram.start(&opts("nova-3"), &creds(&mock)).unwrap();
    feed(&mut s, &pcm(1600, 100), 800);
    assert_eq!(s.finish().unwrap().text, "split frame");
}

#[test]
fn cancel_closes_the_connection_immediately() {
    let _serial = serial();
    let (started_tx, started_rx) = signal();
    let (tx, rx) = signal();
    let closed = Arc::new(Mutex::new(false));
    let c2 = closed.clone();
    let mock = serve(Options::default(), move |c| {
        // The client is connected and streaming once its first audio arrives.
        let _ = c.recv(Duration::from_secs(20));
        fire(&started_tx);
        *c2.lock().unwrap() = c.closed_within(Duration::from_secs(20));
        fire(&tx);
    });
    let mut s = deepgram::Deepgram.start(&opts("nova-3"), &creds(&mock)).unwrap();
    s.push(&pcm(800, 100));
    s.push(&pcm(800, 100));
    wait(&started_rx);
    let t0 = Instant::now();
    s.cancel();
    // "Immediately" = the caller is not held up by the I/O thread (no handshake or settle wait);
    // the bound only has to be far below those multi-second waits, not below a scheduler hiccup.
    assert!(t0.elapsed() < Duration::from_secs(1), "cancel took {:?}", t0.elapsed());
    wait(&rx);
    assert!(*closed.lock().unwrap(), "server saw the connection go away");
}

#[test]
fn cancel_while_connecting_does_not_wait_for_the_handshake() {
    let mock = serve(Options { accept_delay: Duration::from_secs(3), ..Options::default() }, |_| {});
    let mut s = deepgram::Deepgram.start(&opts("nova-3"), &creds(&mock)).unwrap();
    s.push(&pcm(800, 100));
    std::thread::sleep(Duration::from_millis(150));
    let t0 = Instant::now();
    s.cancel();
    assert!(t0.elapsed() < Duration::from_millis(100));
}

#[test]
fn dropping_a_session_cancels_it() {
    let (tx, rx) = signal();
    let mock = serve(Options::default(), move |c| {
        let _ = c.recv(Duration::from_secs(1));
        assert!(c.closed_within(Duration::from_secs(2)));
        fire(&tx);
    });
    let mut s = deepgram::Deepgram.start(&opts("nova-3"), &creds(&mock)).unwrap();
    s.push(&pcm(800, 100));
    std::thread::sleep(Duration::from_millis(200));
    drop(s);
    wait(&rx);
}

#[test]
fn upgrade_refusal_is_classified_without_leaking_the_key() {
    let mock = serve(Options { reject: Some((401, "invalid credentials sk-live-1")), ..Options::default() }, |_| {});
    let mut s = deepgram::Deepgram.start(&opts("nova-3"), &creds(&mock)).unwrap();
    s.push(&pcm(800, 100));
    let e = s.finish().expect_err("auth failure");
    assert_eq!(e.code, SttErrorCode::AuthFailed);
    assert_eq!(e.status, Some(401));
    assert!(e.message.contains("Deepgram"));
    assert!(!format!("{e:?}").contains(KEY));
}

#[test]
fn unreachable_server_is_reported() {
    let addr = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap()
    };
    let c = Credentials { api_key: Some(KEY.into()), base_url: Some(format!("ws://{addr}")), extra: vec![] };
    let mut s = deepgram::Deepgram.start(&opts("nova-3"), &c).unwrap();
    s.push(&pcm(800, 100));
    let e = s.finish().expect_err("nothing listens");
    assert_eq!(e.code, SttErrorCode::Unreachable);
}

/// One session whose server sends a final, keeps the connection open without reading for a
/// while (so audio piles up unread on its side), then drops it: closing a socket with unread
/// data sends an RST, and Windows then discards what the client has not read yet. The session
/// reads whenever the socket is readable before it writes, so it already holds the text.
fn reset_after_final_round() -> Result<Transcript, SttError> {
    let mock = serve(Options::default(), |c| {
        let _ = c.recv(Duration::from_secs(20));
        c.send(&dg_results("kept text", true, false));
        // not reading: the client's next frames stay unread in our receive buffer
        std::thread::sleep(Duration::from_millis(250));
        // script returns: the socket is dropped without a close frame, with unread data (RST)
    });
    let mut s = deepgram::Deepgram.start(&opts("nova-3"), &creds(&mock)).unwrap();
    // ~420 ms of pushes in 100 ms commands: the reset lands in the middle of them
    for _ in 0..14 {
        s.push(&pcm(800, 100));
        s.push(&pcm(800, 100));
        std::thread::sleep(Duration::from_millis(30));
    }
    s.finish()
}

#[test]
fn connection_dropped_mid_session_keeps_text_or_errors() {
    let _serial = serial();
    // text already received survives the drop (and the RST that follows it)
    assert_eq!(reset_after_final_round().expect("text kept").text, "kept text");

    // nothing received: an error rather than a silent empty transcript
    let mock = serve(Options::default(), |c| {
        let _ = c.recv(Duration::from_secs(20));
    });
    let mut s = deepgram::Deepgram.start(&opts("nova-3"), &creds(&mock)).unwrap();
    s.push(&pcm(800, 100));
    std::thread::sleep(Duration::from_millis(300));
    s.push(&pcm(800, 100));
    let e = s.finish().expect_err("lost connection");
    assert_eq!(e.code, SttErrorCode::Unreachable);
}

#[test]
fn final_text_survives_a_reset_under_parallel_load() {
    let _serial = serial();
    let rounds: Vec<_> = (0..8).map(|_| std::thread::spawn(reset_after_final_round)).collect();
    for (i, r) in rounds.into_iter().enumerate() {
        let out = r.join().expect("round did not panic");
        assert_eq!(out.unwrap_or_else(|e| panic!("round {i}: {e:?}")).text, "kept text", "round {i}");
    }
}

// ------------------------------------------------------------------------------------------
// AssemblyAI

fn turn(text: &str, end: bool, formatted: bool, order: u64) -> String {
    json!({"type":"Turn","turn_order":order,"turn_is_formatted":formatted,"end_of_turn":end,"transcript":text}).to_string()
}

#[test]
fn assemblyai_token_mint_handshake_and_force_endpoint_sequence() {
    let log = new_log();
    let l2 = log.clone();
    let (tx, rx) = signal();
    let mock = serve(Options { http: (200, r#"{"token":"tok+/abc"}"#.into()), ..Options::default() }, move |c| {
        l2.lock().unwrap().push(format!("url:{}", c.req.target));
        l2.lock().unwrap().push(format!("hasauth:{}", c.req.header("authorization").is_some()));
        c.send(r#"{"type":"Begin","id":"sess-1","expires_at":1}"#);
        let mut said = false;
        while let Some(m) = c.recv(Duration::from_secs(5)) {
            match m {
                Message::Binary(b) => {
                    l2.lock().unwrap().push(format!("bin:{}", b.len()));
                    if !said {
                        said = true;
                        c.send(&turn("hello wor", false, false, 0));
                    }
                }
                Message::Text(t) => {
                    l2.lock().unwrap().push(format!("txt:{}", t.as_str()));
                    if t.as_str().contains("ForceEndpoint") {
                        c.send(&turn("hello world", true, false, 0));
                        c.send(&turn("Hello, world.", true, true, 0));
                    }
                    if t.as_str().contains("Terminate") {
                        c.send(r#"{"type":"Termination","audio_duration_seconds":1}"#);
                        break;
                    }
                }
                _ => {}
            }
        }
        fire(&tx);
    });
    let mut o = opts("universal-3-5-pro");
    o.hints = vec!["OpenTalk".into()];
    let mut s = assemblyai::AssemblyAi.start(&o, &creds(&mock)).unwrap();
    feed(&mut s, &pcm(1000, 700), 500);
    let t0 = Instant::now();
    let out = s.finish().unwrap();
    assert_eq!(out.text, "Hello, world.");
    assert!(t0.elapsed() < Duration::from_millis(900), "formatted end-of-turn ends the wait");
    wait(&rx);

    // the token mint is a plain GET with the raw key in Authorization
    let http: Vec<_> = mock.requests().into_iter().filter(|r| !r.is_ws).collect();
    assert_eq!(http.len(), 1);
    assert_eq!(http[0].target, "/v3/token?expires_in_seconds=60");
    assert_eq!(http[0].header("authorization"), Some(KEY));

    let l = lines(&log);
    assert_eq!(
        l[0],
        "url:/v3/ws?sample_rate=16000&encoding=pcm_s16le&format_turns=true&token=tok%2B%2Fabc&speech_model=universal-3-5-pro&keyterms_prompt=%5B%22OpenTalk%22%5D"
    );
    assert_eq!(l[1], "hasauth:false");
    // 1000 samples: one full 50 ms frame, then the 200-sample tail padded to a full 50 ms frame
    assert_eq!(&l[2..], &["bin:1600", "bin:1600", r#"txt:{"type":"ForceEndpoint"}"#, r#"txt:{"type":"Terminate"}"#]);
}

#[test]
fn assemblyai_token_failure_is_classified() {
    let mock = serve(Options { http: (401, r#"{"error":"Unauthorized"}"#.into()), ..Options::default() }, |_| {});
    let mut s = assemblyai::AssemblyAi.start(&opts("universal-3-5-pro"), &creds(&mock)).unwrap();
    s.push(&pcm(800, 1));
    let e = s.finish().expect_err("bad key");
    assert_eq!(e.code, SttErrorCode::AuthFailed);
    assert!(e.message.contains("AssemblyAI"));
    assert!(mock.ws_requests().is_empty(), "no socket without a token");
}

/// The account API key (prefix-less, so only a configured-secret match can see it) and the
/// temporary token AssemblyAI mints from it.
const AAI_KEY: &str = "as3f9a1c77b25e0d84aa6c1e";
const AAI_TOKEN: &str = "tmp.7c1d0e9a4b2f3Q+/z";

fn assert_no_leak(e: &SttError) {
    for text in [e.message.as_str(), e.detail.as_str(), &e.to_string(), &format!("{e:?}")] {
        for secret in [AAI_KEY, AAI_TOKEN, "tmp.7c1d0e9a4b2f3Q"] {
            assert!(!text.contains(secret), "{secret} leaked: {text}");
        }
    }
}

fn aai_creds(m: &Mock) -> Credentials {
    Credentials { api_key: Some(AAI_KEY.into()), base_url: Some(m.ws_url()), extra: Vec::new() }
}

#[test]
fn assemblyai_token_endpoint_error_echoing_the_account_key_is_clean() {
    for (status, body) in [
        (401, format!(r#"{{"error":"Invalid API key {AAI_KEY}"}}"#)),
        (500, format!("upstream failure for Authorization: {AAI_KEY}")),
    ] {
        let mock = serve(Options { http: (status, body), ..Options::default() }, |_| {});
        let mut s = assemblyai::AssemblyAi.start(&opts("universal-3-5-pro"), &aai_creds(&mock)).unwrap();
        s.push(&pcm(800, 1));
        let e = s.finish().expect_err("token mint refused");
        assert_eq!(e.status, Some(status));
        assert!(e.detail.contains("[redacted]"), "{}", e.detail);
        assert_no_leak(&e);
    }
}

#[test]
fn assemblyai_ws_error_frame_and_close_echoing_the_key_and_token_are_clean() {
    let mint = Options { http: (200, format!(r#"{{"token":"{AAI_TOKEN}"}}"#)), ..Options::default() };

    // an Error frame naming both the account key and the temporary token
    let mock = serve(mint.clone(), |c| {
        c.send(r#"{"type":"Begin","id":"s"}"#);
        while let Some(m) = c.recv(Duration::from_secs(20)) {
            if matches!(m, Message::Binary(_)) {
                c.send(&json!({"type":"Error","error":format!("bad session: key {AAI_KEY} token {AAI_TOKEN}")}).to_string());
            }
        }
    });
    let mut s = assemblyai::AssemblyAi.start(&opts("universal-3-5-pro"), &aai_creds(&mock)).unwrap();
    feed(&mut s, &pcm(3200, 500), 800);
    let e = s.finish().expect_err("provider error frame");
    assert_eq!(e.code, SttErrorCode::Other);
    assert!(e.detail.contains("[redacted]"), "{}", e.detail);
    assert_no_leak(&e);

    // an application close (3005) whose reason echoes them
    let mock = serve(mint.clone(), |c| {
        use tungstenite::protocol::CloseFrame;
        use tungstenite::protocol::frame::coding::CloseCode;
        c.send(r#"{"type":"Begin","id":"s"}"#);
        let _ = c.recv(Duration::from_secs(20));
        let reason = format!("session {AAI_TOKEN} of account {AAI_KEY} expired");
        let _ = c.ws.close(Some(CloseFrame { code: CloseCode::Library(3005), reason: reason.into() }));
        let _ = c.recv(Duration::from_secs(2));
    });
    let mut s = assemblyai::AssemblyAi.start(&opts("universal-3-5-pro"), &aai_creds(&mock)).unwrap();
    feed(&mut s, &pcm(3200, 500), 800);
    let e = s.finish().expect_err("closed with an application code");
    assert!(e.detail.contains("close 3005"), "{}", e.detail);
    assert_no_leak(&e);

    // an upgrade refusal (the token is in the URL, the key only in the mint request)
    let mock = serve(
        Options { reject: Some((401, Box::leak(format!("denied: {AAI_KEY} / {AAI_TOKEN}").into_boxed_str()))), ..mint },
        |_| {},
    );
    let mut s = assemblyai::AssemblyAi.start(&opts("universal-3-5-pro"), &aai_creds(&mock)).unwrap();
    s.push(&pcm(800, 1));
    let e = s.finish().expect_err("upgrade refused");
    assert_eq!(e.code, SttErrorCode::AuthFailed);
    assert_no_leak(&e);
}

#[test]
fn assemblyai_audio_waits_for_begin() {
    let (tx, rx) = signal();
    let first = Arc::new(Mutex::new(None::<Duration>));
    let f2 = first.clone();
    let mock = serve(Options { http: (200, r#"{"token":"t"}"#.into()), ..Options::default() }, move |c| {
        let t0 = Instant::now();
        std::thread::sleep(Duration::from_millis(400));
        // the client pushed audio long ago but must hold it until it sees Begin
        assert!(c.recv(Duration::from_millis(50)).is_none(), "no audio before Begin");
        c.send(r#"{"type":"Begin","id":"s"}"#);
        if c.recv(Duration::from_secs(2)).is_some() {
            *f2.lock().unwrap() = Some(t0.elapsed());
        }
        fire(&tx);
    });
    let mut s = assemblyai::AssemblyAi.start(&opts("universal-3-5-pro"), &creds(&mock)).unwrap();
    s.push(&pcm(1600, 1));
    wait(&rx);
    assert!(first.lock().unwrap().is_some());
    s.cancel();
}

// ------------------------------------------------------------------------------------------
// OpenAI Realtime

#[test]
fn openai_realtime_session_update_upsampled_audio_and_commit() {
    let log = new_log();
    let l2 = log.clone();
    let audio = Arc::new(Mutex::new(Vec::<Vec<i16>>::new()));
    let a2 = audio.clone();
    let (tx, rx) = signal();
    let mock = serve(Options::default(), move |c| {
        l2.lock().unwrap().push(format!("url:{}", c.req.target));
        l2.lock().unwrap().push(format!("auth:{}", c.req.header("authorization").unwrap_or("")));
        c.send(r#"{"type":"session.created","session":{}}"#);
        let upd = c.recv_json();
        l2.lock().unwrap().push(format!("update:{upd}"));
        c.send(r#"{"type":"session.updated","session":{}}"#);
        loop {
            let Some(m) = c.recv(Duration::from_secs(5)) else { break };
            let Message::Text(t) = m else { continue };
            let v: Value = serde_json::from_str(t.as_str()).unwrap();
            match v["type"].as_str().unwrap() {
                "input_audio_buffer.append" => {
                    a2.lock().unwrap().push(decode_pcm(v["audio"].as_str().unwrap()));
                    if a2.lock().unwrap().len() == 1 {
                        c.send(r#"{"type":"conversation.item.input_audio_transcription.delta","item_id":"i1","delta":"hel"}"#);
                    }
                }
                "input_audio_buffer.commit" => {
                    l2.lock().unwrap().push("commit".into());
                    c.send(r#"{"type":"input_audio_buffer.committed","item_id":"i1"}"#);
                    c.send(r#"{"type":"conversation.item.input_audio_transcription.completed","item_id":"i1","transcript":"hello there"}"#);
                    break;
                }
                other => panic!("unexpected {other}"),
            }
        }
        fire(&tx);
    });
    let mut o = opts("gpt-4o-transcribe");
    o.language = Some("en".into());
    let mut s = openai_realtime::OpenAiRealtime.start(&o, &creds(&mock)).unwrap();
    let samples = pcm(1600, 1200);
    feed(&mut s, &samples, 400);
    let t0 = Instant::now();
    let out = s.finish().unwrap();
    assert_eq!(out.text, "hello there");
    assert!(t0.elapsed() < Duration::from_millis(900), "committed + completed ends the wait early");
    wait(&rx);

    let l = lines(&log);
    assert_eq!(l[0], "url:/v1/realtime?intent=transcription");
    assert_eq!(l[1], format!("auth:Bearer {KEY}"));
    let upd: Value = serde_json::from_str(l[2].strip_prefix("update:").unwrap()).unwrap();
    assert_eq!(upd["type"], "session.update");
    assert_eq!(upd["session"]["type"], "transcription");
    let input = &upd["session"]["audio"]["input"];
    assert_eq!(input["format"], json!({"type":"audio/pcm","rate":24000}));
    assert_eq!(input["transcription"], json!({"model":"gpt-4o-transcribe","language":"en"}));
    assert_eq!(input["turn_detection"]["type"], "server_vad");
    assert_eq!(l[3], "commit");

    // two 50 ms frames, each upsampled 800 -> 1200 samples at 24 kHz
    let frames = audio.lock().unwrap().clone();
    assert_eq!(frames.len(), 2);
    assert!(frames.iter().all(|f| f.len() == 1200));
    assert_eq!(frames[0], util::upsample_16k_to_24k(&samples[..800]));
}

#[test]
fn openai_realtime_error_event_is_reported_when_there_is_no_text() {
    let mock = serve(Options::default(), |c| {
        c.send(r#"{"type":"session.created"}"#);
        let _ = c.recv_json();
        c.send(r#"{"type":"session.updated"}"#);
        loop {
            let Some(Message::Text(t)) = c.recv(Duration::from_secs(3)) else { break };
            if t.as_str().contains("commit") {
                c.send(&json!({"type":"error","error":{"code":"invalid_api_key","message":"Incorrect API key provided: test-key-123"}}).to_string());
            }
        }
    });
    let mut s = openai_realtime::OpenAiRealtime.start(&opts("gpt-4o-mini-transcribe"), &creds(&mock)).unwrap();
    feed(&mut s, &pcm(1600, 10), 800);
    let e = s.finish().expect_err("provider error");
    assert_eq!(e.code, SttErrorCode::Other);
    assert!(!format!("{e:?}").contains(KEY), "key redacted from error detail");
}

// ------------------------------------------------------------------------------------------
// Gemini Live

#[test]
fn gemini_live_setup_first_then_audio_and_stream_end() {
    let log = new_log();
    let l2 = log.clone();
    let (tx, rx) = signal();
    let mock = serve(Options::default(), move |c| {
        l2.lock().unwrap().push(format!("url:{}", c.req.target));
        // setup is the first (and, until setupComplete, only) message
        let setup = c.recv_json();
        l2.lock().unwrap().push(format!("setup:{setup}"));
        assert!(c.recv(Duration::from_millis(150)).is_none(), "no audio before setupComplete");
        c.send(r#"{"setupComplete":{}}"#);
        let mut n = 0;
        loop {
            let Some(m) = c.recv(Duration::from_secs(5)) else { break };
            let Message::Text(t) = m else { continue };
            let v: Value = serde_json::from_str(t.as_str()).unwrap();
            if v["realtimeInput"]["audio"].is_object() {
                n += 1;
                let a = &v["realtimeInput"]["audio"];
                assert_eq!(a["mimeType"], "audio/pcm;rate=16000");
                assert_eq!(decode_pcm(a["data"].as_str().unwrap()).len(), if n <= 2 { 800 } else { 300 });
                if n == 1 {
                    // Live answers in binary frames too
                    c.send_binary(r#"{"serverContent":{"interimInputTranscription":{"text":"good mor"}}}"#);
                }
            } else if v["realtimeInput"]["audioStreamEnd"] == true {
                l2.lock().unwrap().push(format!("end after {n} audio frames"));
                c.send(r#"{"serverContent":{"inputTranscription":{"text":"Good morning."},"generationComplete":true}}"#);
                break;
            }
        }
        fire(&tx);
    });
    let mut o = opts("gemini-3.5-transcribe-live");
    o.language = Some("en".into());
    o.hints = vec!["OpenTalk".into()];
    let mut s = gemini_live::GeminiLive.start(&o, &creds(&mock)).unwrap();
    feed(&mut s, &pcm(1900, 3000), 700);
    let t0 = Instant::now();
    let out = s.finish().unwrap();
    assert_eq!(out.text, "Good morning.");
    assert!(t0.elapsed() < Duration::from_millis(1500), "generationComplete ends the wait early, took {:?}", t0.elapsed());
    wait(&rx);
    let l = lines(&log);
    assert_eq!(l[0], format!("url:/ws/google.ai.generativelanguage.v1beta.GenerativeService.BidiGenerateContent?key={KEY}"));
    let setup: Value = serde_json::from_str(l[1].strip_prefix("setup:").unwrap()).unwrap();
    assert_eq!(setup["setup"]["model"], "models/gemini-3.5-transcribe-live");
    assert_eq!(setup["setup"]["generationConfig"]["responseModalities"], json!(["TEXT"]));
    assert_eq!(setup["setup"]["inputAudioTranscription"], json!({"languageCodes":["en"],"customVocabulary":["OpenTalk"]}));
    assert_eq!(l[2], "end after 3 audio frames");
}

#[test]
fn gemini_unfinalized_turn_uses_the_wider_ceiling_and_is_kept() {
    let mock = serve(Options::default(), |c| {
        let _ = c.recv_json();
        c.send(r#"{"setupComplete":{}}"#);
        let mut said = false;
        while let Some(m) = c.recv(Duration::from_secs(6)) {
            if let Message::Text(_) = m {
                if !said {
                    said = true;
                    c.send(r#"{"serverContent":{"interimInputTranscription":{"text":"never finalized"}}}"#);
                }
            }
        }
    });
    let mut s = gemini_live::GeminiLive.start(&opts("gemini-3.5-transcribe-live"), &creds(&mock)).unwrap();
    feed(&mut s, &pcm(1600, 3000), 800);
    std::thread::sleep(Duration::from_millis(100));
    let t0 = Instant::now();
    let out = s.finish().unwrap();
    let took = t0.elapsed();
    assert_eq!(out.text, "never finalized");
    assert!(took >= Duration::from_millis(2900) && took < Duration::from_millis(3700), "Gemini ceiling is 3000 ms, took {took:?}");
}

#[test]
fn gemini_rejected_key_closes_with_1008_after_upgrade() {
    let mock = serve(Options::default(), |c| {
        let _ = c.recv_json();
        let _ = c.ws.close(Some(tungstenite::protocol::CloseFrame { code: 1008u16.into(), reason: "API key not valid".into() }));
        let _ = c.ws.flush();
        std::thread::sleep(Duration::from_millis(200));
    });
    let mut s = gemini_live::GeminiLive.start(&opts("gemini-3.5-transcribe-live"), &creds(&mock)).unwrap();
    s.push(&pcm(800, 1));
    let e = s.finish().expect_err("bad key");
    assert_eq!(e.code, SttErrorCode::AuthFailed);
    assert!(!format!("{e:?}").contains(KEY));
}

#[test]
fn never_ready_provider_times_out_instead_of_hanging() {
    // a server that accepts the socket but never answers setup: bounded by the ready timeout
    let mock = serve(Options::default(), |c| while c.recv(Duration::from_secs(8)).is_some() {});
    let mut s = gemini_live::GeminiLive.start(&opts("gemini-3.5-transcribe-live"), &creds(&mock)).unwrap();
    s.push(&pcm(800, 1));
    let t0 = Instant::now();
    let e = s.finish().expect_err("no setupComplete");
    assert_eq!(e.code, SttErrorCode::Timeout);
    assert!(t0.elapsed() < Duration::from_secs(8));
}

// ------------------------------------------------------------------------------------------
// Desktop-only checks (ignored by default; run with `cargo test -- --ignored desktop_`)

/// wss:// against a local TLS server (schannel on both ends, self-signed certificate made
/// with PowerShell and removed from the user's store again). The server pauses between
/// messages for longer than the client's socket poll, so this also exercises resuming TLS
/// reads after read timeouts.
#[test]
#[ignore]
fn desktop_tls_loopback_roundtrip() {
    use std::io::Write;
    let dir = std::env::temp_dir().join(format!("opentalk-tls-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let pfx = dir.join("test.pfx");
    let script = format!(
        "$c = New-SelfSignedCertificate -DnsName 'localhost' -CertStoreLocation Cert:\\CurrentUser\\My -NotAfter (Get-Date).AddDays(1); \
         try {{ $pw = ConvertTo-SecureString -String 'pw' -Force -AsPlainText; Export-PfxCertificate -Cert $c -FilePath '{}' -Password $pw | Out-Null }} \
         finally {{ Remove-Item $c.PSPath }}",
        pfx.display()
    );
    let status = std::process::Command::new("powershell").args(["-NoProfile", "-NonInteractive", "-Command", &script]).status().expect("powershell");
    assert!(status.success() && pfx.exists(), "could not create a test certificate");
    let identity = native_tls::Identity::from_pkcs12(&std::fs::read(&pfx).unwrap(), "pw").expect("pkcs12");
    let _ = std::fs::remove_dir_all(&dir);
    let acceptor = native_tls::TlsAcceptor::new(identity).expect("acceptor");

    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = mpsc::channel::<Vec<String>>();
    std::thread::spawn(move || {
        let (tcp, _) = listener.accept().unwrap();
        let tls = acceptor.accept(tcp).expect("tls accept");
        let mut ws = tungstenite::accept(tls).expect("ws accept");
        let mut seen = Vec::new();
        let mut sent_partial = false;
        loop {
            let Ok(m) = ws.read() else { break };
            match m {
                Message::Binary(b) => {
                    seen.push(format!("bin:{}", b.len()));
                    if !sent_partial {
                        sent_partial = true;
                        std::thread::sleep(Duration::from_millis(100));
                        ws.send(Message::text(dg_results("secure", false, false))).unwrap();
                    }
                }
                Message::Text(t) => {
                    seen.push(format!("txt:{}", t.as_str()));
                    if t.as_str().contains("Finalize") {
                        std::thread::sleep(Duration::from_millis(120));
                        ws.send(Message::text(dg_results("secure channel", true, true))).unwrap();
                    }
                    if t.as_str().contains("CloseStream") {
                        break;
                    }
                }
                _ => {}
            }
        }
        let _ = ws.get_mut().flush();
        let _ = tx.send(seen);
    });

    session::TEST_INSECURE_TLS.store(true, std::sync::atomic::Ordering::Relaxed);
    let c = Credentials { api_key: Some(KEY.into()), base_url: Some(format!("wss://127.0.0.1:{}", addr.port())), extra: vec![] };
    let mut s = deepgram::Deepgram.start(&opts("nova-3"), &c).unwrap();
    feed(&mut s, &pcm(2400, 900), 800);
    std::thread::sleep(Duration::from_millis(300));
    let out = s.finish();
    session::TEST_INSECURE_TLS.store(false, std::sync::atomic::Ordering::Relaxed);
    assert_eq!(out.expect("transcript over TLS").text, "secure channel");
    let seen = rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(seen, ["bin:1600", "bin:1600", "bin:1600", r#"txt:{"type":"Finalize"}"#, r#"txt:{"type":"CloseStream"}"#]);
}

fn process_cpu_ms() -> f64 {
    use windows::Win32::Foundation::FILETIME;
    use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};
    let (mut c, mut e, mut k, mut u) = (FILETIME::default(), FILETIME::default(), FILETIME::default(), FILETIME::default());
    // SAFETY: all out-pointers are valid FILETIMEs for the duration of the call.
    unsafe { GetProcessTimes(GetCurrentProcess(), &mut c, &mut e, &mut k, &mut u).expect("GetProcessTimes") };
    let ticks = |f: FILETIME| ((f.dwHighDateTime as u64) << 32 | f.dwLowDateTime as u64) as f64 / 10_000.0;
    ticks(k) + ticks(u)
}

/// CPU cost of a live session pushing real-time audio (the whole test process, so the mock
/// server and the test thread are included: this is an upper bound for the session itself).
#[test]
#[ignore]
fn desktop_session_cpu_budget() {
    let mock = serve(Options::default(), |c| {
        while let Some(m) = c.recv(Duration::from_secs(10)) {
            if let Message::Text(t) = m {
                if t.as_str().contains("Finalize") {
                    c.send(&dg_results("done", true, true));
                }
            }
        }
    });
    let mut s = deepgram::Deepgram.start(&opts("nova-3"), &creds(&mock)).unwrap();
    s.push(&pcm(800, 100));
    std::thread::sleep(Duration::from_millis(500)); // connected
    let (cpu0, t0) = (process_cpu_ms(), Instant::now());
    let frame = pcm(800, 3000);
    for _ in 0..400 {
        s.push(&frame);
        std::thread::sleep(Duration::from_millis(50)); // capture pacing: 20 s of audio
    }
    let (cpu, wall) = (process_cpu_ms() - cpu0, t0.elapsed().as_secs_f64() * 1000.0);
    println!("recording 20 s: {cpu:.1} ms CPU over {wall:.0} ms wall = {:.2} % of one core (whole test process)", cpu / wall * 100.0);
    assert!(cpu / wall < 0.03, "session should stay far below the 1.5 % recording budget");
    assert_eq!(s.finish().unwrap().text, "done");
}
