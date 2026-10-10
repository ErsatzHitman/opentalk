//! End-to-end tests that drive the real executable.
//!
//! - `transcribe_file_*`: headless pipeline against the in-repo mock STT server (no network).
//! - `live_*` (ignored): OpenRouter, `:free` models only, key via `OPENTALK_SECRET_OPENROUTER`.
//!   They print VERIFIED / UNVERIFIED / FAILED; `OPENTALK_LIVE_REQUIRE=1` turns UNVERIFIED into a failure.
//! - `desktop_e2e_notepad` (ignored): `tests/e2e.ps1` - daemon + injected WAV + paste into Notepad.
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};

const EXE: &str = env!("CARGO_BIN_EXE_opentalk");

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name)
}

/// A scratch `OPENTALK_HOME` that is removed on drop.
struct Home(PathBuf);

impl Home {
    fn new(tag: &str) -> Home {
        let dir = std::env::temp_dir().join(format!("opentalk-e2e-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp home");
        Home(dir)
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// `opentalk --bench mock-stt`, killed on drop.
struct Mock {
    child: Child,
    url: String,
}

impl Mock {
    fn start() -> Mock {
        let mut child = Command::new(EXE)
            .args(["--bench", "mock-stt", "--port", "0"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("start mock");
        let mut line = String::new();
        BufReader::new(child.stdout.as_mut().expect("piped stdout")).read_line(&mut line).expect("mock banner");
        let url = line.trim().strip_prefix("listening ").unwrap_or_else(|| panic!("unexpected banner: {line:?}")).to_string();
        Mock { child, url }
    }
}

impl Drop for Mock {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn transcribe(home: &Home, args: &[&str], env: &[(&str, &str)]) -> (i32, serde_json::Value) {
    let mut cmd = Command::new(EXE);
    cmd.arg("--transcribe-file").args(args).env("OPENTALK_HOME", &home.0).stdout(Stdio::piped()).stderr(Stdio::piped());
    for (k, v) in env {
        cmd.env(k, v);
    }
    let out = cmd.output().expect("run opentalk");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let json = stdout.lines().rev().find(|l| l.trim_start().starts_with('{')).unwrap_or("{}");
    (out.status.code().unwrap_or(-1), serde_json::from_str(json).unwrap_or_else(|e| panic!("bad JSON {json:?}: {e}")))
}

#[test]
fn transcribe_file_runs_the_whole_pipeline_against_a_mock() {
    let mock = Mock::start();
    let home = Home::new("tf");
    let wav = fixture("tts_fillers.wav");
    let (code, v) = transcribe(
        &home,
        &[wav.to_str().unwrap(), "--provider", "openai", "--model", "whisper-1", "--no-llm"],
        &[("OPENTALK_STT_BASE_URL", &mock.url), ("OPENTALK_SECRET_OPENAI", "e2e-dummy-key-123456")],
    );
    assert_eq!(code, 0, "{v}");
    assert!(v["raw"].as_str().unwrap().starts_with("Um, so I think"), "{v}");
    let text = v["text"].as_str().unwrap();
    assert!(text.contains("ship the new release on Friday"), "{text}");
    let low = text.to_lowercase();
    for filler in ["um,", " uh,", "you know", "the the"] {
        assert!(!low.contains(filler), "'{filler}' survived: {text}");
    }
    assert_eq!(v["provider"], "openai");
    assert_eq!(v["model"], "whisper-1");
    assert_eq!(v["ai_cleaned"], false);
    for k in ["stt_ms", "cleanup_ms", "total_ms"] {
        assert!(v[k].is_number(), "{k} missing in {v}");
    }
}

#[test]
fn transcribe_file_reports_failures_as_json_with_a_nonzero_exit() {
    let home = Home::new("tf-err");
    let (code, v) = transcribe(&home, &["definitely-missing.wav"], &[]);
    assert_eq!(code, 1);
    assert!(v["error"].as_str().unwrap().contains("definitely-missing.wav"), "{v}");

    let (code, v) = transcribe(&home, &[fixture("tts_clean.wav").to_str().unwrap(), "--provider", "nope"], &[]);
    assert_eq!(code, 1);
    assert!(v["error"].as_str().unwrap().contains("unknown provider"), "{v}");
}

#[test]
fn silence_is_rejected_without_a_request() {
    let home = Home::new("tf-silence");
    // The base URL points nowhere: a network attempt would surface as a different error.
    let (code, v) = transcribe(
        &home,
        &[fixture("silence_2s.wav").to_str().unwrap(), "--provider", "openai"],
        &[("OPENTALK_STT_BASE_URL", "http://127.0.0.1:9"), ("OPENTALK_SECRET_OPENAI", "e2e-dummy-key-123456")],
    );
    assert_eq!(code, 1);
    assert!(v["error"].as_str().unwrap().contains("no speech"), "{v}");
}

#[test]
fn bench_summary_of_an_empty_log_is_valid_json() {
    let home = Home::new("bench");
    let out = Command::new(EXE).args(["--bench", "latency"]).env("OPENTALK_HOME", &home.0).output().expect("run");
    let v: serde_json::Value = serde_json::from_str(String::from_utf8_lossy(&out.stdout).trim()).expect("json");
    assert_eq!(v["count"], 0);
}

/// Outcome of a live STT run. A live test is only a verification when a model returned a
/// transcript of the spoken audio; everything short of that is `Unverified`, never a quiet pass.
#[derive(Debug, PartialEq)]
enum Live {
    /// A free model transcribed the audio (the transcript contains the spoken keywords).
    Verified(String),
    /// No free model produced a transcript (rate limits, gated models, audio dropped upstream).
    Unverified(String),
    /// Our side is wrong or the key is bad (auth, unknown model, bad request): always a failure.
    Failed(String),
}

/// Whether an `--transcribe-file` error message is an upstream limitation of the free tier
/// (rate limit, capacity, timeout, gated model, a reply that dropped the audio) rather than a
/// fault of the request or the key.
fn is_upstream_limit(msg: &str) -> bool {
    ["rate-limited", "having problems", "took too long", "denied access", "No audio detected"].iter().any(|k| msg.contains(k))
}

/// Maps a finished live run to a test result. `Unverified` passes unless `require` is set
/// (`OPENTALK_LIVE_REQUIRE=1`); `Failed` never passes.
fn live_result(outcome: &Live, require: bool) -> Result<(), String> {
    match outcome {
        Live::Verified(_) => Ok(()),
        Live::Unverified(_) if !require => Ok(()),
        Live::Unverified(why) => Err(format!("OPENTALK_LIVE_REQUIRE=1 and STT was not verified: {why}")),
        Live::Failed(why) => Err(format!("live STT failed: {why}")),
    }
}

/// Prints a `VERIFIED` / `UNVERIFIED` / `FAILED` line and panics when `live_result` says so.
fn report_live(test: &str, outcome: Live) {
    match &outcome {
        Live::Verified(t) => eprintln!("{test}: VERIFIED live transcript: {t:?}"),
        Live::Unverified(why) => eprintln!("{test}: UNVERIFIED - no free model produced a transcript of the audio: {why}"),
        Live::Failed(why) => eprintln!("{test}: FAILED - {why}"),
    }
    let require = std::env::var("OPENTALK_LIVE_REQUIRE").is_ok_and(|v| v == "1");
    if let Err(e) = live_result(&outcome, require) {
        panic!("{test}: {e}");
    }
}

#[test]
fn live_verdicts_distinguish_verified_unverified_and_failed() {
    let (v, u, f) = (Live::Verified("t".into()), Live::Unverified("x".into()), Live::Failed("y".into()));
    assert!(live_result(&v, false).is_ok() && live_result(&v, true).is_ok());
    assert!(live_result(&u, false).is_ok());
    assert!(live_result(&u, true).unwrap_err().contains("not verified"));
    assert!(live_result(&f, false).is_err() && live_result(&f, true).is_err());
    assert!(is_upstream_limit("OpenRouter rate-limited the request. Wait a moment and try again."));
    assert!(is_upstream_limit("OpenRouter denied access. Your key may not include this model."));
    assert!(is_upstream_limit("No audio detected"));
    assert!(!is_upstream_limit("OpenRouter rejected your API key."));
    assert!(!is_upstream_limit("OpenRouter doesn't recognize the model “x”."));
}

/// Live OpenRouter smoke test (`:free` models only; `OPENTALK_LIVE_MODEL` may list several,
/// comma separated). Prints `VERIFIED`, `UNVERIFIED` or `FAILED`: upstream limits (429/5xx,
/// gated models, a free endpoint that drops the audio) are `UNVERIFIED`, never a green pass
/// with `OPENTALK_LIVE_REQUIRE=1`; a bad key or request is `FAILED`.
#[test]
#[ignore = "live: OpenRouter free model (needs OPENTALK_SECRET_OPENROUTER)"]
fn live_transcribe_file_openrouter_free() {
    if std::env::var_os("OPENTALK_SECRET_OPENROUTER").is_none() {
        report_live("live_transcribe_file_openrouter_free", Live::Unverified("OPENTALK_SECRET_OPENROUTER not set".into()));
        return;
    }
    let models = std::env::var("OPENTALK_LIVE_MODEL").unwrap_or_else(|_| "nvidia/nemotron-3-nano-omni-30b-a3b-reasoning:free".into());
    let home = Home::new("live");
    let wav = fixture("tts_fillers.wav");
    let mut notes = Vec::new();
    for model in models.split(',').map(str::trim).filter(|m| !m.is_empty()) {
        assert!(model.ends_with(":free"), "live tests may only use :free models");
        for attempt in 0..4u64 {
            let (code, v) = transcribe(&home, &[wav.to_str().unwrap(), "--provider", "openrouter", "--model", model, "--no-llm"], &[]);
            if code == 0 {
                let text = v["text"].as_str().unwrap_or_default().to_lowercase();
                if text.contains("friday") || text.contains("release") {
                    return report_live("live_transcribe_file_openrouter_free", Live::Verified(text));
                }
                notes.push(format!("{model}: reply without the spoken words ({text:?})"));
                break;
            }
            let msg = v["error"].as_str().unwrap_or_default().to_string();
            if !is_upstream_limit(&msg) {
                return report_live("live_transcribe_file_openrouter_free", Live::Failed(format!("{model}: {msg}")));
            }
            eprintln!("{model} attempt {attempt}: {msg}");
            if attempt == 3 {
                notes.push(format!("{model}: {msg}"));
            } else {
                std::thread::sleep(std::time::Duration::from_secs(6 * (attempt + 1)));
            }
        }
    }
    report_live("live_transcribe_file_openrouter_free", Live::Unverified(notes.join("; ")));
}

#[test]
#[ignore = "desktop: opens Notepad, injects a WAV, pastes into it (tests/e2e.ps1)"]
fn desktop_e2e_notepad() {
    let script = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/e2e.ps1");
    let status = Command::new("powershell")
        .args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"])
        .arg(script)
        .args(["-Exe", EXE, "-Stt", "mock"])
        .status()
        .expect("run powershell");
    assert!(status.success(), "tests/e2e.ps1 failed");
}

#[test]
#[ignore = "desktop: like desktop_e2e_notepad, with the Deepgram live protocol from the local mock (tests/e2e.ps1 -Stt mock-stream)"]
fn desktop_e2e_notepad_streaming() {
    let script = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/e2e.ps1");
    let status = Command::new("powershell")
        .args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"])
        .arg(script)
        .args(["-Exe", EXE, "-Stt", "mock-stream"])
        .status()
        .expect("run powershell");
    assert!(status.success(), "tests/e2e.ps1 -Stt mock-stream failed");
}

/// Post one IPC message (`core::ipc`: WM_APP + 4 toggle, + 5 cancel) straight to the daemon window,
/// so several land in its queue back to back.
fn post_to_daemon(msg: u32) -> bool {
    use windows::Win32::Foundation::{LPARAM, WPARAM};
    use windows::Win32::UI::WindowsAndMessaging::{FindWindowW, PostMessageW};
    // SAFETY: plain window lookup and post; a missing window just fails.
    unsafe {
        let Ok(hwnd) = FindWindowW(windows::core::w!("OpenTalkDaemon"), None) else { return false };
        PostMessageW(Some(hwnd), msg, WPARAM(0), LPARAM(0)).is_ok()
    }
}

/// Stop then Esc/cancel while the microphone is still coming up must drop the dictation: before
/// the fix the recorder arrived after the cancel, started the job anyway and reported (and, with
/// speech, pasted) a result the user had cancelled.
#[test]
#[ignore = "desktop: starts the real daemon (single-instance lock), injected WAV, local mock STT"]
fn desktop_cancel_while_the_mic_starts_drops_the_dictation() {
    const WM_OT_TOGGLE: u32 = 0x8000 + 4;
    const WM_OT_CANCEL: u32 = 0x8000 + 5;
    let mock = Mock::start();
    let home = Home::new("cancel-pending");
    let cfg = serde_json::json!({
        "audio_cues": false, "auto_paste": false, "keep_in_clipboard": true, "history_enabled": true,
        "save_discarded": true, "show_app_bubble": false, "start_minimized": true, "stt": { "provider": "openai" }
    });
    std::fs::write(home.0.join("config.json"), cfg.to_string()).expect("config");
    let mut daemon = Command::new(EXE)
        .arg("--hidden")
        .env("OPENTALK_HOME", &home.0)
        .env("OPENTALK_INJECT_WAV", fixture("tts_fillers.wav"))
        .env("OPENTALK_STT_BASE_URL", &mock.url)
        .env("OPENTALK_SECRET_OPENAI", "e2e-dummy-key-123456")
        .spawn()
        .expect("start daemon");
    let ready = (0..300).any(|_| {
        std::thread::sleep(std::time::Duration::from_millis(10));
        unsafe { windows::Win32::UI::WindowsAndMessaging::FindWindowW(windows::core::w!("OpenTalkDaemon"), None) }.is_ok()
    });
    let posted = ready && post_to_daemon(WM_OT_TOGGLE) && post_to_daemon(WM_OT_TOGGLE) && post_to_daemon(WM_OT_CANCEL);
    std::thread::sleep(std::time::Duration::from_secs(3));
    let history = std::fs::read_to_string(home.0.join("history.jsonl")).unwrap_or_default();
    let log = std::fs::read_to_string(home.0.join("logs").join("daemon.log")).unwrap_or_default();
    let _ = Command::new(EXE).args(["--bench", "quit"]).env("OPENTALK_HOME", &home.0).status();
    if daemon.wait_timeout_ms(5000).is_none() {
        let _ = daemon.kill();
        let _ = daemon.wait();
    }
    assert!(ready && posted, "daemon window never appeared (another daemon running?)");
    assert!(log.contains("recording #1 cancelled"), "the pending recording was not cancelled:\n{log}");
    assert!(!log.contains("recording #1 stopped"), "a cancelled recording reached the worker:\n{log}");
    assert!(history.trim().is_empty(), "a cancelled dictation wrote history: {history}");
}

trait WaitTimeout {
    fn wait_timeout_ms(&mut self, ms: u64) -> Option<std::process::ExitStatus>;
}

impl WaitTimeout for Child {
    fn wait_timeout_ms(&mut self, ms: u64) -> Option<std::process::ExitStatus> {
        let t0 = std::time::Instant::now();
        while t0.elapsed().as_millis() < u128::from(ms) {
            if let Ok(Some(s)) = self.try_wait() {
                return Some(s);
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        None
    }
}
