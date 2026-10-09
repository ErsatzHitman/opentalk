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
