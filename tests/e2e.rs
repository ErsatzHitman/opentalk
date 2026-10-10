//! End-to-end tests that drive the real executable.
//!
//! - `transcribe_file_*`: headless pipeline against the in-repo mock STT server (no network).
//! - `live_*` (ignored): OpenRouter, `:free` models only, key via `OPENTALK_SECRET_OPENROUTER`.
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

/// Live OpenRouter smoke test. Upstream rate limits, capacity errors and models that are gated
/// or do not take audio are reported, not failed: only a successful reply is checked.
#[test]
#[ignore = "live: OpenRouter free model (needs OPENTALK_SECRET_OPENROUTER)"]
fn live_transcribe_file_openrouter_free() {
    if std::env::var_os("OPENTALK_SECRET_OPENROUTER").is_none() {
        eprintln!("OPENTALK_SECRET_OPENROUTER not set; skipping");
        return;
    }
    let model = std::env::var("OPENTALK_LIVE_MODEL").unwrap_or_else(|_| "nvidia/nemotron-3-nano-omni-30b-a3b-reasoning:free".into());
    assert!(model.ends_with(":free"), "live tests may only use :free models");
    let home = Home::new("live");
    let wav = fixture("tts_fillers.wav");
    let mut last = serde_json::Value::Null;
    for attempt in 0..4 {
        let (code, v) = transcribe(&home, &[wav.to_str().unwrap(), "--provider", "openrouter", "--model", &model, "--no-llm"], &[]);
        if code == 0 {
            let text = v["text"].as_str().unwrap_or_default().to_lowercase();
            eprintln!("live transcript: {text}");
            assert!(text.contains("friday") || text.contains("release"), "unexpected transcript: {v}");
            return;
        }
        eprintln!("attempt {attempt}: {v}");
        last = v;
        std::thread::sleep(std::time::Duration::from_secs(6 * (attempt + 1)));
    }
    eprintln!("no live transcript (upstream limits / gated / no audio support): {last}");
}

/// True when the interactive desktop can be driven: the session is not locked (the Win10/11 lock
/// screen runs on the normal desktop, so `OpenInputDesktop` alone still succeeds there) and
/// `OpenInputDesktop` works (it fails on the secure desktop: LogonUI, UAC). Nothing injected reaches
/// an app otherwise.
fn input_desktop_available() -> bool {
    use windows::Win32::System::RemoteDesktop::{
        WTS_CURRENT_SERVER_HANDLE, WTS_CURRENT_SESSION, WTSFreeMemory, WTSINFOEXW, WTSQuerySessionInformationW,
        WTSSessionInfoEx,
    };
    use windows::Win32::System::StationsAndDesktops::{CloseDesktop, DESKTOP_CONTROL_FLAGS, DESKTOP_SWITCHDESKTOP, OpenInputDesktop};
    let mut buf = windows::core::PWSTR::null();
    let mut len = 0u32;
    // SAFETY: the WTS buffer is read once (unaligned, size-checked) and freed; the desktop handle is closed.
    unsafe {
        if WTSQuerySessionInformationW(Some(WTS_CURRENT_SERVER_HANDLE), WTS_CURRENT_SESSION, WTSSessionInfoEx, &mut buf, &mut len).is_ok() {
            let mut locked = false;
            if !buf.is_null() && len as usize >= std::mem::size_of::<WTSINFOEXW>() {
                let info = std::ptr::read_unaligned(buf.0 as *const WTSINFOEXW);
                locked = info.Level == 1 && info.Data.WTSInfoExLevel1.SessionFlags == 0; // WTS_SESSIONSTATE_LOCK
            }
            WTSFreeMemory(buf.0 as *mut std::ffi::c_void);
            if locked {
                return false;
            }
        }
        match OpenInputDesktop(DESKTOP_CONTROL_FLAGS(0), false, DESKTOP_SWITCHDESKTOP) {
            Ok(d) => {
                let _ = CloseDesktop(d);
                true
            }
            Err(_) => false,
        }
    }
}

/// Returns false (after saying why) when an input-dependent desktop test cannot run.
fn require_input_desktop(test: &str) -> bool {
    let ok = input_desktop_available();
    if !ok {
        eprintln!("SKIPPED {test}: no usable input desktop (workstation locked, or OpenInputDesktop failed)");
    }
    ok
}

/// Process ids of every running `image` (e.g. `notepad.exe`).
fn pids_of(image: &str) -> std::collections::HashSet<u32> {
    let out = Command::new("tasklist").args(["/FO", "CSV", "/NH", "/FI", &format!("IMAGENAME eq {image}")]).output();
    let text = out.map(|o| String::from_utf8_lossy(&o.stdout).into_owned()).unwrap_or_default();
    text.lines().filter_map(|l| l.split("\",\"").nth(1)?.parse().ok()).collect()
}

/// Run `cmd` with a hard time limit; true when it exits successfully in time. A hang (a lost
/// foreground, a stuck UI Automation call) used to block the whole suite and leave Notepad and
/// daemons behind: on timeout the process tree is killed and every `sweep` image (e.g. Notepad) that
/// appeared since the start is ended too.
fn run_limited(mut cmd: Command, limit: std::time::Duration, sweep: &[&str]) -> bool {
    let before: Vec<_> = sweep.iter().map(|img| pids_of(img)).collect();
    let mut child = cmd.spawn().expect("spawn");
    match child.wait_timeout_ms(limit.as_millis() as u64) {
        Some(status) => status.success(),
        None => {
            eprintln!("{cmd:?} exceeded {limit:?}; killing it and what it opened");
            let _ = Command::new("taskkill").args(["/F", "/T", "/PID", &child.id().to_string()]).output();
            let _ = child.wait();
            for (img, before) in sweep.iter().zip(&before) {
                for pid in pids_of(img).difference(before) {
                    let _ = Command::new("taskkill").args(["/F", "/T", "/PID", &pid.to_string()]).output();
                }
            }
            false
        }
    }
}

/// Run `tests/e2e.ps1` for the given STT mode under `limit`.
fn run_e2e_script(stt: &str, limit: std::time::Duration) -> bool {
    let script = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/e2e.ps1");
    let image = std::path::Path::new(EXE).file_name().and_then(|n| n.to_str()).unwrap_or("opentalk.exe").to_owned();
    let mut cmd = Command::new("powershell");
    cmd.args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"]).arg(script).args(["-Exe", EXE, "-Stt", stt]);
    run_limited(cmd, limit, &["notepad.exe", &image])
}

#[test]
fn a_hung_child_is_killed_with_what_it_started() {
    let mut cmd = Command::new("cmd");
    cmd.args(["/C", "ping -n 60 127.0.0.1 >nul"]);
    let before = pids_of("PING.EXE");
    let t0 = std::time::Instant::now();
    assert!(!run_limited(cmd, std::time::Duration::from_millis(1500), &["PING.EXE"]));
    assert!(t0.elapsed() < std::time::Duration::from_secs(10), "the limit was not enforced");
    std::thread::sleep(std::time::Duration::from_millis(300));
    let leftovers: Vec<_> = pids_of("PING.EXE").difference(&before).copied().collect();
    assert!(leftovers.is_empty(), "the hung child's ping outlived the limit: {leftovers:?}");
}

/// Upper bound for one e2e script run (it normally takes ~15-25 s).
const E2E_LIMIT: std::time::Duration = std::time::Duration::from_secs(180);

#[test]
#[ignore = "desktop: opens Notepad, injects a WAV, pastes into it (tests/e2e.ps1)"]
fn desktop_e2e_notepad() {
    if !require_input_desktop("desktop_e2e_notepad") {
        return;
    }
    assert!(run_e2e_script("mock", E2E_LIMIT), "tests/e2e.ps1 failed");
}

#[test]
#[ignore = "desktop: like desktop_e2e_notepad, with the Deepgram live protocol from the local mock (tests/e2e.ps1 -Stt mock-stream)"]
fn desktop_e2e_notepad_streaming() {
    if !require_input_desktop("desktop_e2e_notepad_streaming") {
        return;
    }
    assert!(run_e2e_script("mock-stream", E2E_LIMIT), "tests/e2e.ps1 -Stt mock-stream failed");
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
