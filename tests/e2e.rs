//! End-to-end tests that drive the real executable.
//!
//! - `transcribe_file_*`: headless pipeline against the in-repo mock STT server (no network). The
//!   same mock answers `/chat/completions`, so the voice rewrite runs end to end without a provider.
//! - `live_*` (ignored): OpenRouter, `:free` models only, key via `OPENTALK_SECRET_OPENROUTER`.
//!   They print VERIFIED / UNVERIFIED / FAILED; `OPENTALK_LIVE_REQUIRE=1` turns UNVERIFIED into a failure.
//! - `desktop_e2e_notepad` (ignored): `tests/e2e.ps1` - daemon + injected WAV + paste into Notepad.
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};

#[allow(dead_code)]
#[path = "common/live.rs"]
mod live;
use live::{Live, report_live, try_model};

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

    /// Write a (partial) config.json; unspecified settings keep their defaults.
    fn config(&self, json: &str) {
        std::fs::write(self.0.join("config.json"), json).expect("write config");
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
        Mock::start_with(&[])
    }

    /// `extra`: more `--bench mock-stt` flags (`--text`, `--llm-text`, `--llm-status`, `--delay-ms`).
    fn start_with(extra: &[&str]) -> Mock {
        let mut child = Command::new(EXE)
            .args(["--bench", "mock-stt", "--port", "0"])
            .args(extra)
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

// ---- voice rewrite, end to end against the mock ---------------------------------------------

const WORDY: &str = "So I was thinking that maybe we could possibly move the meeting from Thursday to Friday afternoon if that works for everyone because the client is out on Thursday.";

/// `--transcribe-file` on tts_fillers.wav with the STT and the LLM both served by `mock`.
fn rewrite_run(home: &Home, mock: &Mock, extra: &[&str]) -> (i32, serde_json::Value) {
    let wav = fixture("tts_fillers.wav");
    let mut args = vec![wav.to_str().unwrap(), "--provider", "openai", "--model", "whisper-1", "--no-llm"];
    args.extend_from_slice(extra);
    transcribe(
        home,
        &args,
        &[
            ("OPENTALK_STT_BASE_URL", &mock.url),
            ("OPENTALK_LLM_BASE_URL", &mock.url),
            ("OPENTALK_SECRET_OPENAI", "e2e-dummy-key-123456"),
            ("OPENTALK_SECRET_OPENROUTER", "e2e-dummy-key-123456"),
        ],
    )
}

#[test]
fn transcribe_file_rewrites_when_the_phrase_ends_the_dictation() {
    let said = format!("{WORDY} Rewrite this concisely.");
    let mock = Mock::start_with(&["--text", &said]);
    let home = Home::new("rw-ok");
    let (code, v) = rewrite_run(&home, &mock, &[]);
    assert_eq!(code, 0, "{v}");
    let (raw, text) = (v["raw"].as_str().unwrap(), v["text"].as_str().unwrap());
    assert!(raw.ends_with("Rewrite this concisely."), "raw keeps the phrase: {raw}");
    assert_eq!((v["rewritten"].clone(), v["rewrite"].clone()), (true.into(), "done".into()), "{v}");
    assert!(v["rewrite_error"].is_null());
    assert!(!text.to_lowercase().contains("rewrite") && !text.to_lowercase().contains("concisely"), "{text}");
    // The mock's deterministic rewrite keeps the first 60% of the BODY, so it is clearly shorter.
    assert!(text.split_whitespace().count() < WORDY.split_whitespace().count() * 7 / 10, "{text}");
    assert!(text.starts_with("So I was thinking"), "{text}");
    assert!(v["original"].as_str().unwrap().ends_with("Thursday."), "original is the deterministic body: {v}");
    assert_eq!(v["ai_cleaned"], false, "a rewrite is not an AI cleanup");
}

#[test]
fn transcribe_file_uses_the_fixed_llm_reply_and_ignores_the_cleanup_toggle() {
    let said = format!("{WORDY} Rewrite this concisely.");
    let mock = Mock::start_with(&["--text", &said, "--llm-text", "Can we move the meeting from Thursday to Friday afternoon? The client is out on Thursday."]);
    let home = Home::new("rw-fixed");
    // `--no-llm` (AI cleanup off) is in rewrite_run's args: the rewrite still goes out.
    let (code, v) = rewrite_run(&home, &mock, &[]);
    assert_eq!(code, 0, "{v}");
    assert_eq!(v["text"], "Can we move the meeting from Thursday to Friday afternoon? The client is out on Thursday.");
    assert_eq!(v["rewritten"], true);
}

#[test]
fn transcribe_file_pastes_the_text_as_dictated_when_the_rewrite_fails() {
    let said = format!("{WORDY} Rewrite this concisely.");
    let mock = Mock::start_with(&["--text", &said, "--llm-status", "500"]);
    let home = Home::new("rw-500");
    let (code, v) = rewrite_run(&home, &mock, &[]);
    assert_eq!(code, 0, "a failed rewrite never fails the dictation: {v}");
    assert_eq!(v["text"], WORDY.trim_end_matches('.').to_string() + ".", "the body, phrase removed: {v}");
    assert_eq!((v["rewritten"].clone(), v["rewrite"].clone()), (false.into(), "failed".into()));
    assert!(v["rewrite_error"].as_str().unwrap().contains("500"), "{v}");
}

#[test]
fn transcribe_file_falls_back_on_a_refusal_or_a_longer_answer() {
    let said = format!("{WORDY} Rewrite this concisely.");
    for reply in ["I'm sorry, but I can't help with that.", "Sure."] {
        let mock = Mock::start_with(&["--text", &said, "--llm-text", reply]);
        let home = Home::new("rw-bad");
        let (code, v) = rewrite_run(&home, &mock, &[]);
        assert_eq!(code, 0, "{v}");
        assert_eq!((v["rewritten"].clone(), v["rewrite"].clone()), (false.into(), "failed".into()), "{reply}: {v}");
        assert!(v["rewrite_error"].as_str().unwrap().contains("InvalidOutput"), "{v}");
        assert!(v["text"].as_str().unwrap().starts_with("So I was thinking"), "{v}");
    }
}

#[test]
fn transcribe_file_times_out_a_slow_rewrite_and_pastes_the_original() {
    let said = format!("{WORDY} Rewrite this concisely.");
    let mock = Mock::start_with(&["--text", &said, "--delay-ms", "2000"]);
    let home = Home::new("rw-slow");
    home.config(r#"{"ai_cleanup":{"rewrite_timeout_ms":300}}"#);
    let started = std::time::Instant::now();
    let (code, v) = rewrite_run(&home, &mock, &[]);
    assert_eq!(code, 0, "{v}");
    assert_eq!((v["rewritten"].clone(), v["rewrite"].clone()), (false.into(), "failed".into()), "{v}");
    assert!(v["rewrite_error"].as_str().unwrap().contains("Timeout"), "{v}");
    assert!(v["text"].as_str().unwrap().starts_with("So I was thinking"), "{v}");
    // STT took 2 s; the rewrite gave up after ~0.3 s instead of waiting the 2 s for the mock.
    assert!(started.elapsed().as_millis() < 4_500, "{} ms", started.elapsed().as_millis());
}

#[test]
fn transcribe_file_without_the_phrase_is_unchanged() {
    let mock = Mock::start();
    let home = Home::new("rw-none");
    let (code, v) = rewrite_run(&home, &mock, &[]);
    assert_eq!(code, 0, "{v}");
    assert_eq!((v["rewritten"].clone(), v["rewrite"].clone()), (false.into(), "none".into()));
    assert!(v["text"].as_str().unwrap().contains("ship the new release on Friday"), "{v}");
}

#[test]
fn transcribe_file_no_rewrite_keeps_the_phrase_in_the_text() {
    let said = format!("{WORDY} Rewrite this concisely.");
    let mock = Mock::start_with(&["--text", &said]);
    let home = Home::new("rw-off");
    let (code, v) = rewrite_run(&home, &mock, &["--no-rewrite"]);
    assert_eq!(code, 0, "{v}");
    assert_eq!(v["rewrite"], "none");
    assert!(v["text"].as_str().unwrap().ends_with("Rewrite this concisely."), "{v}");
    // The same switch lives in config.json.
    let home = Home::new("rw-off-cfg");
    home.config(r#"{"ai_cleanup":{"rewrite_enabled":false}}"#);
    let (_, v) = rewrite_run(&home, &mock, &[]);
    assert_eq!(v["rewrite"], "none");
    // An explicitly empty phrase turns it off as well.
    let home = Home::new("rw-empty");
    home.config(r#"{"rewrite_trigger":""}"#);
    let (_, v) = rewrite_run(&home, &mock, &[]);
    assert_eq!(v["rewrite"], "none");
}

#[test]
fn transcribe_file_custom_phrase() {
    let said = format!("{WORDY} Make it short.");
    let mock = Mock::start_with(&["--text", &said]);
    let home = Home::new("rw-custom");
    let (_, v) = rewrite_run(&home, &mock, &["--rewrite-trigger", "make it short"]);
    assert_eq!(v["rewritten"], true, "{v}");
    assert!(!v["text"].as_str().unwrap().to_lowercase().contains("make it short"), "{v}");
    // The stock phrase no longer fires when another one is configured.
    let said = format!("{WORDY} Rewrite this concisely.");
    let mock = Mock::start_with(&["--text", &said]);
    let (_, v) = rewrite_run(&home, &mock, &["--rewrite-trigger", "make it short"]);
    assert_eq!(v["rewrite"], "none", "{v}");
}

#[test]
fn transcribe_file_only_the_phrase_pastes_nothing() {
    let mock = Mock::start_with(&["--text", "Rewrite this concisely."]);
    let home = Home::new("rw-only");
    let (code, v) = rewrite_run(&home, &mock, &[]);
    assert_eq!(code, 0, "{v}");
    assert_eq!((v["text"].clone(), v["rewrite"].clone(), v["rewritten"].clone()), ("".into(), "nothing".into(), false.into()), "{v}");
}

#[test]
fn transcribe_file_short_body_needs_no_llm() {
    // No LLM base override and no key: reaching the network would show up as a failed rewrite.
    let mock = Mock::start_with(&["--text", "Send it Friday. Rewrite this concisely."]);
    let home = Home::new("rw-short");
    let wav = fixture("tts_fillers.wav");
    let (code, v) = transcribe(
        &home,
        &[wav.to_str().unwrap(), "--provider", "openai", "--model", "whisper-1", "--no-llm"],
        &[("OPENTALK_STT_BASE_URL", &mock.url), ("OPENTALK_SECRET_OPENAI", "e2e-dummy-key-123456"), ("OPENTALK_LLM_BASE_URL", "http://127.0.0.1:9")],
    );
    assert_eq!(code, 0, "{v}");
    assert_eq!((v["text"].clone(), v["rewrite"].clone()), ("Send it Friday.".into(), "skipped".into()), "{v}");
}

#[test]
fn transcribe_file_without_a_key_reports_not_configured_and_keeps_the_text() {
    let said = format!("{WORDY} Rewrite this concisely.");
    let mock = Mock::start_with(&["--text", &said]);
    let home = Home::new("rw-nokey");
    let wav = fixture("tts_fillers.wav");
    let (code, v) = transcribe(
        &home,
        &[wav.to_str().unwrap(), "--provider", "openai", "--model", "whisper-1"],
        &[("OPENTALK_STT_BASE_URL", &mock.url), ("OPENTALK_SECRET_OPENAI", "e2e-dummy-key-123456"), ("OPENTALK_LLM_BASE_URL", &mock.url)],
    );
    assert_eq!(code, 0, "{v}");
    assert_eq!(v["rewrite"], "failed", "{v}");
    assert!(v["rewrite_error"].as_str().unwrap().contains("NotConfigured"), "{v}");
    assert!(v["text"].as_str().unwrap().starts_with("So I was thinking"), "{v}");
}

/// Live rewrite through the real executable: the STT is the mock, the LLM is OpenRouter with a
/// `:free` text model (never Groq, never a paid model). Without `OPENTALK_SECRET_OPENROUTER` or
/// network it is UNVERIFIED; `OPENTALK_LIVE_REQUIRE=1` turns that into a failure.
#[test]
#[ignore = "live: OpenRouter free text model (needs OPENTALK_SECRET_OPENROUTER)"]
fn live_rewrite_openrouter_free() {
    const TEST: &str = "live_rewrite_openrouter_free";
    let Ok(key) = std::env::var("OPENTALK_SECRET_OPENROUTER") else {
        return report_live(TEST, Live::Unverified("OPENTALK_SECRET_OPENROUTER not set".into()));
    };
    let models = std::env::var("OPENTALK_LIVE_MODEL")
        .unwrap_or_else(|_| "nvidia/nemotron-3-super-120b-a12b:free,google/gemma-4-31b-it:free,nvidia/nemotron-3.5-lightning:free".into());
    let said = format!("{WORDY} Rewrite this concisely.");
    let mock = Mock::start_with(&["--text", &said]);
    let wav = fixture("tts_fillers.wav");
    let mut notes = Vec::new();
    for model in models.split(',').map(str::trim).filter(|m| !m.is_empty()) {
        assert!(model.ends_with(":free"), "live tests may only use :free models");
        let home = Home::new("rw-live");
        home.config(&format!(r#"{{"ai_cleanup":{{"provider":"openrouter","models":{{"openrouter":"{model}"}},"rewrite_timeout_ms":30000}}}}"#));
        for attempt in 0..3u64 {
            let (code, v) = transcribe(
                &home,
                &[wav.to_str().unwrap(), "--provider", "openai", "--model", "whisper-1", "--no-llm"],
                &[("OPENTALK_STT_BASE_URL", &mock.url), ("OPENTALK_SECRET_OPENAI", "e2e-dummy-key-123456"), ("OPENTALK_SECRET_OPENROUTER", &key)],
            );
            assert_eq!(code, 0, "{v}");
            if v["rewritten"] == true {
                let text = v["text"].as_str().unwrap_or_default();
                let ok = text.split_whitespace().count() < WORDY.split_whitespace().count() && text.contains("Thursday") && text.contains("Friday");
                if ok {
                    return report_live(TEST, Live::Verified(format!("{model}: {text}")));
                }
                notes.push(format!("{model}: unexpected rewrite {text:?}"));
                break;
            }
            let err = v["rewrite_error"].as_str().unwrap_or("no error").to_string();
            if err.contains("401") || err.contains("403") || err.contains("NotConfigured") {
                return report_live(TEST, Live::Failed(format!("{model}: {err}")));
            }
            if err.contains("429") || err.contains("Timeout") {
                std::thread::sleep(std::time::Duration::from_secs(2 << attempt));
                continue;
            }
            notes.push(format!("{model}: {err}"));
            break;
        }
    }
    report_live(TEST, Live::Unverified(notes.join("; ")));
}

/// Live OpenRouter smoke test (`:free` models only; `OPENTALK_LIVE_MODEL` may list several,
/// comma separated). Prints `VERIFIED`, `UNVERIFIED` or `FAILED`: upstream limits (429/5xx,
/// gated models, a free endpoint that drops the audio, see PLAN section 10) are `UNVERIFIED`,
/// never a green pass with `OPENTALK_LIVE_REQUIRE=1`; a bad key or request is `FAILED`.
/// Only rate limits, capacity errors and timeouts are retried; a "No audio detected" or
/// "denied access" answer gets one attempt per model.
#[test]
#[ignore = "live: OpenRouter free model (needs OPENTALK_SECRET_OPENROUTER)"]
fn live_transcribe_file_openrouter_free() {
    const TEST: &str = "live_transcribe_file_openrouter_free";
    if std::env::var_os("OPENTALK_SECRET_OPENROUTER").is_none() {
        report_live(TEST, Live::Unverified("OPENTALK_SECRET_OPENROUTER not set".into()));
        return;
    }
    let models = std::env::var("OPENTALK_LIVE_MODEL").unwrap_or_else(|_| "nvidia/nemotron-3-nano-omni-30b-a3b-reasoning:free".into());
    let home = Home::new("live");
    let wav = fixture("tts_fillers.wav");
    let mut notes = Vec::new();
    for model in models.split(',').map(str::trim).filter(|m| !m.is_empty()) {
        assert!(model.ends_with(":free"), "live tests may only use :free models");
        let verdict = try_model(
            model,
            &["friday", "release"],
            &[0, 6, 12, 18],
            || {
                let (code, v) = transcribe(&home, &[wav.to_str().unwrap(), "--provider", "openrouter", "--model", model, "--no-llm"], &[]);
                match code {
                    0 => Ok(v["text"].as_str().unwrap_or_default().to_string()),
                    _ => Err(v["error"].as_str().unwrap_or_default().to_string()),
                }
            },
            std::thread::sleep,
        );
        match verdict {
            Live::Unverified(why) => notes.push(why),
            done => return report_live(TEST, done),
        }
    }
    report_live(TEST, Live::Unverified(notes.join("; ")));
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

/// One row of a process snapshot.
struct ProcRow {
    pid: u32,
    parent: u32,
    image: String,
}

/// Every running process (pid, parent pid, image name) from a Toolhelp32 snapshot.
fn process_table() -> Vec<ProcRow> {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Diagnostics::ToolHelp::{CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS};
    let mut rows = Vec::new();
    // SAFETY: the snapshot handle is closed; `entry` is a plain struct whose `dwSize` is set as the API requires.
    unsafe {
        let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else { return rows };
        let mut entry = PROCESSENTRY32W { dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
        let mut more = Process32FirstW(snap, &mut entry).is_ok();
        while more {
            let len = entry.szExeFile.iter().position(|&c| c == 0).unwrap_or(entry.szExeFile.len());
            rows.push(ProcRow { pid: entry.th32ProcessID, parent: entry.th32ParentProcessID, image: String::from_utf16_lossy(&entry.szExeFile[..len]) });
            more = Process32NextW(snap, &mut entry).is_ok();
        }
        let _ = CloseHandle(snap);
    }
    rows
}

/// Pids reachable from `root` through the parent-pid chain (`root` itself excluded).
fn descendants(table: &[ProcRow], root: u32) -> std::collections::HashSet<u32> {
    let mut found = std::collections::HashSet::new();
    let mut frontier = vec![root];
    while let Some(parent) = frontier.pop() {
        for row in table.iter().filter(|r| r.parent == parent && r.pid != root && r.pid != parent) {
            if found.insert(row.pid) {
                frontier.push(row.pid);
            }
        }
    }
    found
}

/// Run `cmd` with a hard time limit; true when it exits successfully in time. A hang (a lost
/// foreground, a stuck UI Automation call) used to block the whole suite and leave Notepad and
/// daemons behind: on timeout the process tree is killed and so is every descendant of the child
/// (found through a snapshot taken before the kill) whose image is in `sweep` (e.g. Notepad).
/// Processes the user or another test started are never touched, even when they match `sweep`.
fn run_limited(cmd: Command, limit: std::time::Duration, sweep: &[&str]) -> bool {
    run_limited_family(cmd, limit, sweep).0
}

/// `run_limited`, also returning the pids the child had started when the limit hit (empty when
/// it finished in time), so a test can check those very processes are gone.
fn run_limited_family(mut cmd: Command, limit: std::time::Duration, sweep: &[&str]) -> (bool, std::collections::HashSet<u32>) {
    let mut child = cmd.spawn().expect("spawn");
    match child.wait_timeout_ms(limit.as_millis() as u64) {
        Some(status) => (status.success(), Default::default()),
        None => {
            eprintln!("{cmd:?} exceeded {limit:?}; killing it and what it opened");
            let table = process_table();
            let family = descendants(&table, child.id());
            let _ = Command::new("taskkill").args(["/F", "/T", "/PID", &child.id().to_string()]).output();
            let _ = child.wait();
            for row in table.iter().filter(|r| family.contains(&r.pid)) {
                if sweep.iter().any(|img| img.eq_ignore_ascii_case(&row.image)) {
                    let _ = Command::new("taskkill").args(["/F", "/T", "/PID", &row.pid.to_string()]).output();
                }
            }
            (false, family)
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

/// The two `run_limited` tests below watch `PING.EXE` process ids, so they must not overlap.
static PING_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Kills the wrapped child when dropped, so a failed assertion cannot leak it.
struct KillOnDrop(Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn hung_ping_cmd() -> Command {
    let mut cmd = Command::new("cmd");
    cmd.args(["/C", "ping -n 60 127.0.0.1 >nul"]);
    cmd
}

#[test]
fn a_hung_child_is_killed_with_what_it_started() {
    let _serial = PING_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let t0 = std::time::Instant::now();
    let (ok, family) = run_limited_family(hung_ping_cmd(), std::time::Duration::from_millis(1500), &["PING.EXE"]);
    assert!(!ok);
    assert!(t0.elapsed() < std::time::Duration::from_secs(10), "the limit was not enforced");
    // Judge the child's own descendants (cmd's ping), not every PING.EXE on the machine: other
    // test runs and the user start pings of their own.
    assert!(!family.is_empty(), "the hung child had started nothing, so the test proves nothing");
    std::thread::sleep(std::time::Duration::from_millis(300));
    let alive: Vec<_> = process_table().into_iter().filter(|r| family.contains(&r.pid) && r.image.eq_ignore_ascii_case("PING.EXE")).map(|r| r.pid).collect();
    assert!(alive.is_empty(), "the hung child's ping outlived the limit: {alive:?}");
}

#[test]
fn the_timeout_sweep_spares_processes_the_child_did_not_start() {
    let _serial = PING_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    // An unrelated PING.EXE appears while the limited run is in flight (as a user's or another
    // test's process would); it is not a descendant of the hung child, so it must survive.
    let starter = std::thread::spawn(|| {
        std::thread::sleep(std::time::Duration::from_millis(500));
        let mut cmd = Command::new("ping");
        cmd.args(["-n", "60", "127.0.0.1"]).stdout(Stdio::null()).stderr(Stdio::null());
        cmd.spawn().map(KillOnDrop)
    });
    assert!(!run_limited(hung_ping_cmd(), std::time::Duration::from_millis(2000), &["PING.EXE"]));
    let mut unrelated = starter.join().expect("starter thread").expect("start the unrelated ping");
    std::thread::sleep(std::time::Duration::from_millis(300));
    assert!(matches!(unrelated.0.try_wait(), Ok(None)), "the sweep killed a process that is not a descendant of the hung child");
}

#[test]
fn descendants_follow_the_parent_chain_only() {
    let row = |pid, parent| ProcRow { pid, parent, image: String::new() };
    let table = [row(10, 1), row(11, 10), row(12, 11), row(13, 2), row(1, 11), row(10, 10)];
    let mut got: Vec<_> = descendants(&table, 10).into_iter().collect();
    got.sort_unstable();
    // 1 is a (bogus) child of 11 and 10 a child of itself: both cycle back to the root and are bounded.
    assert_eq!(got, [1, 11, 12]);
    assert!(descendants(&table, 99).is_empty());
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

/// Esc while the rewrite call is in flight (the pill is in Thinking): nothing is pasted or
/// stored, even after the slow mock LLM finally answers.
#[test]
#[ignore = "desktop: starts the real daemon (single-instance lock), injected WAV, local mock STT + slow mock LLM"]
fn desktop_cancel_during_rewrite() {
    const WM_OT_TOGGLE: u32 = 0x8000 + 4;
    const WM_OT_CANCEL: u32 = 0x8000 + 5;
    let said = format!("{WORDY} Rewrite this concisely.");
    let mock = Mock::start_with(&["--text", &said, "--llm-delay-ms", "5000"]);
    let home = Home::new("cancel-rewrite");
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
        .env("OPENTALK_LLM_BASE_URL", &mock.url)
        .env("OPENTALK_SECRET_OPENAI", "e2e-dummy-key-123456")
        .env("OPENTALK_SECRET_OPENROUTER", "e2e-dummy-key-123456")
        .spawn()
        .expect("start daemon");
    let log_path = home.0.join("logs").join("daemon.log");
    let read_log = || std::fs::read_to_string(&log_path).unwrap_or_default();
    let ready = (0..300).any(|_| {
        std::thread::sleep(std::time::Duration::from_millis(10));
        unsafe { windows::Win32::UI::WindowsAndMessaging::FindWindowW(windows::core::w!("OpenTalkDaemon"), None) }.is_ok()
    });
    let mut posted = ready && post_to_daemon(WM_OT_TOGGLE);
    std::thread::sleep(std::time::Duration::from_secs(5)); // the injected WAV plays
    posted = posted && post_to_daemon(WM_OT_TOGGLE);
    // Wait until the worker is inside the rewrite call, then press Esc.
    let in_rewrite = (0..200).any(|_| {
        std::thread::sleep(std::time::Duration::from_millis(50));
        read_log().contains("rewrite matched")
    });
    let cancelled = in_rewrite && post_to_daemon(WM_OT_CANCEL);
    std::thread::sleep(std::time::Duration::from_secs(8)); // past the mock's 5 s answer
    let history = std::fs::read_to_string(home.0.join("history.jsonl")).unwrap_or_default();
    let log = read_log();
    let _ = Command::new(EXE).args(["--bench", "quit"]).env("OPENTALK_HOME", &home.0).status();
    if daemon.wait_timeout_ms(5000).is_none() {
        let _ = daemon.kill();
        let _ = daemon.wait();
    }
    assert!(ready && posted, "daemon window never appeared (another daemon running?)");
    assert!(in_rewrite && cancelled, "the dictation never reached the rewrite call:\n{log}");
    assert!(log.contains("dictation #1 cancelled"), "Esc did not cancel the job:\n{log}");
    assert!(history.trim().is_empty(), "a cancelled rewrite wrote history: {history}");
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
