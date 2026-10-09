//! One dictation, start to finish: capture the target, start the recorder (and a streaming
//! session when the provider streams), stop + VAD, then hand the audio to a worker thread that
//! transcribes, cleans, pastes and records history. The worker reports back with a posted message.
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use super::host::{App, TIMER_CAP, WM_JOB_DONE, post};
use super::latency::{self, Sample};
use super::machine::{Input, MAX_RECORDING_MS, Phase};
use super::pipeline;
use crate::core::config::Config;
use crate::core::history::{self, Entry, Status};
use crate::core::timing::{now_ms, unix_ms};
use crate::daemon::audio::{self, Recorder, vad};
use crate::daemon::paste::{self, PasteOptions, PasteOutcome, Target};
use crate::daemon::pill::PillMode;
use crate::daemon::{appicon, media, sound};
use crate::stt::{self, Audio, SttError, SttErrorCode, StreamingSession};
use crate::{log_info, log_warn};

/// Shared slot for the streaming session: the capture thread feeds it, `stop` takes it.
type Feed = Arc<Mutex<Option<Box<dyn StreamingSession>>>>;

/// Icon size requested for the active-app bubble (the pill downsamples to its DPI).
const APP_ICON_PX: u32 = 64;
const NO_SPEECH_TITLE: &str = "We didn't catch that";
const NO_SPEECH_BODY: &str = "Try speaking a little closer to the microphone.";

#[derive(Clone, Debug, Default)]
pub struct AppInfo {
    pub name: Option<String>,
    pub exe: Option<String>,
}

pub struct RecSession {
    recorder: Recorder,
    target: Option<Target>,
    app: AppInfo,
    feed: Option<Feed>,
    /// `now_ms()` bits of the first microphone packet (0 = none yet).
    first_audio: Arc<AtomicU64>,
    t_hotkey: f64,
    hotkey_pill: f64,
    seq: u64,
    media_paused: bool,
}

pub struct ActiveJob {
    pub id: u64,
    cancel: Arc<AtomicBool>,
}

/// Audio kept after a failed transcription so "Retry" can try again without re-recording.
pub struct Failed {
    audio: Audio,
    speech_ms: u64,
}

pub enum Outcome {
    /// Text delivered. `note` explains a degraded delivery (copied, not pasted).
    Done { note: Option<String> },
    /// Nothing usable was said (silence, only fillers, hallucination).
    Nothing,
    Failed { title: String, message: String, retry: Option<Failed> },
    Cancelled,
}

struct JobResult {
    id: u64,
    outcome: Outcome,
}

/// Finished jobs waiting for the UI thread (`WM_JOB_DONE` carries the id).
static MAILBOX: Mutex<Vec<JobResult>> = Mutex::new(Vec::new());

struct Job {
    id: u64,
    hwnd: isize,
    cfg: Config,
    audio: Audio,
    speech_ms: u64,
    stream: Option<Box<dyn StreamingSession>>,
    target: Option<Target>,
    app: AppInfo,
    cancel: Arc<AtomicBool>,
    t_stop: f64,
    hotkey_pill: f64,
    hotkey_audio: f64,
}

/// Why dictation cannot start at all (nothing to record for).
fn preflight(cfg: &Config) -> Option<(String, String)> {
    let id = cfg.stt.provider;
    let info = stt::registry::provider(id);
    let creds = stt::credentials(cfg);
    if id == crate::core::config::SttProviderId::Custom {
        return creds.base_url.is_none().then(|| ("Speech-to-text isn't set up".to_string(), "Enter your server address in Settings > Speech-to-Text.".to_string()));
    }
    creds.api_key.is_none().then(|| (format!("Add your {} API key", info.name), "Open Settings > Speech-to-Text to add it.".to_string()))
}

fn friendly_exe(exe: &str) -> String {
    let stem = exe.trim_end_matches(".exe").trim_end_matches(".EXE");
    let mut c = stem.chars();
    c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
}

fn error_title(code: SttErrorCode) -> &'static str {
    match code {
        SttErrorCode::AuthFailed => "Check your API key",
        SttErrorCode::QuotaExhausted => "Out of credits",
        SttErrorCode::RateLimited => "Rate limited",
        SttErrorCode::AccessDenied => "Access denied",
        SttErrorCode::ModelNotFound => "Model not found",
        SttErrorCode::PayloadTooLarge => "Recording too long",
        SttErrorCode::Timeout | SttErrorCode::Unavailable | SttErrorCode::Unreachable => "Connection problem",
        SttErrorCode::NotConfigured => "Speech-to-text isn't set up",
        _ => "Transcription failed",
    }
}

impl App {
    /// Begin a dictation (state machine already moved to Recording). `t0` is when the trigger arrived.
    pub(super) fn start_recording(&mut self, t0: f64) {
        let cfg = self.cfg.clone();
        if let Some((title, msg)) = preflight(&cfg) {
            self.machine.step(Input::StartFailed);
            self.set_idle();
            self.pill.show_error(&title, &msg, false);
            return;
        }
        let target = paste::capture_foreground();
        self.pill.follow_cursor_display();
        self.pill.set_mode(PillMode::Preparing);
        let hotkey_pill = now_ms() - t0;
        stt::prewarm(&cfg);

        let feed: Option<Feed> = match stt::start_stream(&cfg) {
            None => None,
            Some(Ok(session)) => Some(Arc::new(Mutex::new(Some(session)))),
            Some(Err(e)) => {
                log_warn!("streaming unavailable ({:?}); using batch: {}", e.code, e.detail);
                None
            }
        };
        let first_audio = Arc::new(AtomicU64::new(0));
        let sink: audio::PcmSink = {
            let first = first_audio.clone();
            let feed = feed.clone();
            Box::new(move |pcm| {
                if first.load(Ordering::Relaxed) == 0 {
                    first.store(now_ms().to_bits(), Ordering::Relaxed);
                }
                if let Some(feed) = &feed {
                    if let Ok(mut slot) = feed.lock() {
                        if let Some(session) = slot.as_mut() {
                            session.push(pcm);
                        }
                    }
                }
            })
        };
        self.level.store(0f32.to_bits(), Ordering::Relaxed);
        let recorder = match Recorder::start(cfg.mic_device_id.as_deref(), self.level.clone(), Some(sink)) {
            Ok(r) => r,
            Err(e) => {
                log_warn!("cannot start the microphone: {e}");
                if let Some(session) = feed.and_then(|f| f.lock().ok().and_then(|mut g| g.take())) {
                    session.cancel();
                }
                self.machine.step(Input::StartFailed);
                self.set_idle();
                self.pill.show_error("Microphone unavailable", "Check that a microphone is connected and not in use, then try again.", false);
                return;
            }
        };
        self.pill.set_mode(PillMode::Recording);
        if cfg.audio_cues {
            sound::play_start();
        }
        let media_paused = cfg.pause_media && media::pause_if_playing();
        self.set_timer(TIMER_CAP, MAX_RECORDING_MS);

        // The bubble fades in with the capsule expansion (>= 260 ms), so loading the icon here is free.
        let mut app = AppInfo::default();
        if let Some(t) = &target {
            app.exe = Some(t.exe_name.clone());
            app.name = Some(friendly_exe(&t.exe_name));
            if cfg.show_app_bubble {
                if let Some(icon) = appicon::load(t, APP_ICON_PX) {
                    app.name = Some(icon.name.clone());
                    self.pill.set_app(Some(icon));
                }
            }
        }
        self.seq += 1;
        log_info!(
            "recording #{} started (pill {:.1} ms, mic {:.1} ms, stream={}, target={})",
            self.seq,
            hotkey_pill,
            recorder.activation_ms(),
            feed.is_some(),
            app.exe.as_deref().unwrap_or("?")
        );
        self.rec = Some(RecSession {
            recorder,
            target,
            app,
            feed,
            first_audio,
            t_hotkey: t0,
            hotkey_pill,
            seq: self.seq,
            media_paused,
        });
    }

    /// Stop capturing and hand the audio to the worker (state machine already moved to Thinking).
    pub(super) fn stop_recording(&mut self) {
        let Some(rec) = self.rec.take() else {
            self.machine.step(Input::JobDone);
            self.set_idle();
            return;
        };
        let t_stop = now_ms();
        self.kill_timer(TIMER_CAP);
        self.pill.set_mode(PillMode::Thinking);
        if self.cfg.audio_cues {
            sound::play_stop();
        }
        let RecSession { recorder, target, app, feed, first_audio, t_hotkey, hotkey_pill, seq, media_paused } = rec;
        let (audio, stats) = recorder.stop_with_stats();
        if media_paused {
            media::resume();
        }
        let first = f64::from_bits(first_audio.load(Ordering::Relaxed));
        let hotkey_audio = if first > 0.0 { first - t_hotkey } else { -1.0 };
        let stream = feed.and_then(|f| f.lock().ok().and_then(|mut g| g.take()));
        let info = vad::analyze(&audio);
        log_info!(
            "recording #{seq} stopped: {} ms captured, {} ms speech, {} packets, device_lost={}",
            audio.duration_ms(),
            info.speech_ms,
            stats.packets,
            stats.device_lost
        );
        if pipeline::is_silent(&info) {
            if let Some(s) = stream {
                s.cancel();
            }
            self.nothing_said(&app, info.speech_ms);
            return;
        }
        let trimmed = vad::trim(&audio, 150);
        self.spawn_job(Job {
            id: seq,
            hwnd: self.hwnd,
            cfg: self.cfg.clone(),
            audio: trimmed,
            speech_ms: info.speech_ms,
            stream,
            target,
            app,
            cancel: Arc::new(AtomicBool::new(false)),
            t_stop,
            hotkey_pill,
            hotkey_audio,
        });
    }

    /// Silence: tell the user, optionally keep a "discarded" history row, and go back to idle.
    fn nothing_said(&mut self, app: &AppInfo, speech_ms: u64) {
        if self.cfg.save_discarded && self.cfg.history_enabled {
            let entry = discarded_entry(&self.cfg, app, speech_ms);
            if let Err(e) = history::append_new(entry) {
                log_warn!("history: {e}");
            }
        }
        self.machine.step(Input::JobDone);
        self.set_idle();
        self.pill.show_error(NO_SPEECH_TITLE, NO_SPEECH_BODY, false);
    }

    /// "Retry" on the last failed audio (pill card, tray or IPC).
    pub(super) fn retry_last(&mut self) {
        if self.machine.phase != Phase::Idle {
            return;
        }
        let Some(failed) = self.last_failed.take() else { return };
        self.machine.step(Input::RetryStart);
        crate::daemon::hotkey::set_esc_capture(true);
        let target = paste::capture_foreground();
        self.pill.follow_cursor_display();
        self.pill.set_mode(PillMode::Thinking);
        let mut app = AppInfo::default();
        if let Some(t) = &target {
            app.exe = Some(t.exe_name.clone());
            app.name = Some(friendly_exe(&t.exe_name));
        }
        self.seq += 1;
        log_info!("retrying the last failed recording as #{}", self.seq);
        let t_stop = now_ms();
        self.spawn_job(Job {
            id: self.seq,
            hwnd: self.hwnd,
            cfg: self.cfg.clone(),
            audio: failed.audio,
            speech_ms: failed.speech_ms,
            stream: None,
            target,
            app,
            cancel: Arc::new(AtomicBool::new(false)),
            t_stop,
            hotkey_pill: -1.0,
            hotkey_audio: -1.0,
        });
    }

    fn spawn_job(&mut self, job: Job) {
        let id = job.id;
        let hwnd = job.hwnd;
        self.job = Some(ActiveJob { id, cancel: job.cancel.clone() });
        let spawned = std::thread::Builder::new().name("dictation-worker".into()).spawn(move || {
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run_job(job))).unwrap_or_else(|_| Outcome::Failed {
                title: "Something went wrong".into(),
                message: "OpenTalk hit an internal error while processing that recording.".into(),
                retry: None,
            });
            MAILBOX.lock().unwrap_or_else(|e| e.into_inner()).push(JobResult { id, outcome });
            post(hwnd, WM_JOB_DONE, id as usize);
        });
        if let Err(e) = spawned {
            log_warn!("cannot start the worker thread: {e}");
            self.job = None;
            self.machine.step(Input::JobDone);
            self.set_idle();
            self.pill.show_error("Something went wrong", "OpenTalk could not start processing that recording.", false);
        }
    }

    /// The worker finished (`WM_JOB_DONE`).
    pub(super) fn on_job_done(&mut self, id: u64) {
        let results: Vec<JobResult> = std::mem::take(&mut *MAILBOX.lock().unwrap_or_else(|e| e.into_inner()));
        for r in results {
            if self.job.as_ref().is_some_and(|j| j.id == r.id) {
                self.job = None;
                self.machine.step(Input::JobDone);
                crate::daemon::hotkey::set_esc_capture(false);
                self.apply_outcome(r.outcome);
            } else {
                log_info!("dropping the result of cancelled dictation #{}", r.id);
            }
        }
        let _ = id;
    }

    fn apply_outcome(&mut self, outcome: Outcome) {
        self.set_idle();
        match outcome {
            Outcome::Done { note } => {
                self.last_failed = None;
                if let Some(reason) = note {
                    self.pill.show_error("Couldn't paste", &reason, false);
                }
            }
            Outcome::Nothing => self.pill.show_error(NO_SPEECH_TITLE, NO_SPEECH_BODY, false),
            Outcome::Failed { title, message, retry } => {
                let can_retry = retry.is_some();
                if retry.is_some() {
                    self.last_failed = retry;
                }
                self.pill.show_error(&title, &message, can_retry);
            }
            Outcome::Cancelled => {}
        }
    }

    pub(super) fn cancel_recording(&mut self) {
        self.kill_timer(TIMER_CAP);
        if let Some(rec) = self.rec.take() {
            if let Some(session) = rec.feed.and_then(|f| f.lock().ok().and_then(|mut g| g.take())) {
                session.cancel();
            }
            if rec.media_paused {
                media::resume();
            }
            rec.recorder.cancel();
            log_info!("recording #{} cancelled", rec.seq);
        }
        self.set_idle();
    }

    pub(super) fn cancel_job(&mut self) {
        if let Some(job) = self.job.take() {
            job.cancel.store(true, Ordering::Relaxed);
            log_info!("dictation #{} cancelled", job.id);
        }
        self.set_idle();
    }

    /// Shutdown: drop whatever is in flight.
    pub(super) fn cancel_everything(&mut self) {
        self.kill_timer(super::host::TIMER_ARM);
        self.cancel_recording();
        self.cancel_job();
    }
}

fn provider_name(cfg: &Config) -> String {
    format!("{:?}", cfg.stt.provider).to_ascii_lowercase()
}

fn base_entry(cfg: &Config, app: &AppInfo) -> Entry {
    Entry {
        ts: unix_ms(),
        app_name: app.name.clone(),
        app_exe: app.exe.clone(),
        provider: provider_name(cfg),
        model: cfg.stt_model(),
        ..Entry::default()
    }
}

fn discarded_entry(cfg: &Config, app: &AppInfo, speech_ms: u64) -> Entry {
    Entry { status: Status::Discarded, duration_ms: speech_ms, ..base_entry(cfg, app) }
}

fn log_history(cfg: &Config, entry: Entry) {
    if cfg.history_enabled {
        if let Err(e) = history::append_new(entry) {
            log_warn!("history: {e}");
        }
    }
}

/// The worker: STT -> cleanup -> paste -> history. Runs off the UI thread.
fn run_job(job: Job) -> Outcome {
    let Job { id, cfg, audio, speech_ms, stream, target, app, cancel, t_stop, hotkey_pill, hotkey_audio, .. } = job;
    let cancelled = || cancel.load(Ordering::Relaxed);
    let t_req = now_ms();
    let streamed = stream.is_some();

    let result = pipeline::transcribe(&cfg, &audio, stream);
    let stt_ms = now_ms() - t_req;
    if cancelled() {
        return Outcome::Cancelled;
    }
    let transcript = match result {
        Ok(t) => t,
        Err(e) if e.code == SttErrorCode::NoSpeech => {
            if cfg.save_discarded {
                log_history(&cfg, discarded_entry(&cfg, &app, speech_ms));
            }
            return Outcome::Nothing;
        }
        Err(e) => return failure(&cfg, &app, e, audio, speech_ms),
    };

    let cleaned = pipeline::clean(&cfg, &transcript, speech_ms);
    if cleaned.text.is_empty() {
        if cfg.save_discarded {
            log_history(&cfg, Entry { raw: cleaned.raw.clone(), ..discarded_entry(&cfg, &app, speech_ms) });
        }
        return Outcome::Nothing;
    }
    if cancelled() {
        return Outcome::Cancelled;
    }

    let t_paste = now_ms();
    let note = if cfg.auto_paste {
        let opts = PasteOptions { restore_clipboard: !cfg.keep_in_clipboard, smart_spacing: cfg.smart_spacing };
        match paste::paste_text(&cleaned.text, target.as_ref(), opts) {
            PasteOutcome::Pasted => None,
            PasteOutcome::CopiedOnly { reason } => Some(reason),
        }
    } else if paste::copy_to_clipboard(&cleaned.text) {
        None
    } else {
        Some("Text could not be placed on the clipboard".to_string())
    };
    let done = now_ms();
    let (paste_ms, total) = (done - t_paste, done - t_stop);

    log_history(
        &cfg,
        Entry {
            text: cleaned.text.clone(),
            raw: cleaned.raw.clone(),
            status: Status::Ok,
            duration_ms: speech_ms,
            latency_ms: total as u64,
            ai_cleaned: cleaned.ai_cleaned,
            ..base_entry(&cfg, &app)
        },
    );
    latency::record(Sample {
        seq: id,
        hotkey_pill,
        hotkey_audio,
        prep: t_req - t_stop,
        stt: stt_ms,
        cleanup: cleaned.cleanup_ms,
        paste: paste_ms,
        total,
        audio_ms: audio.duration_ms(),
        upload_bytes: if streamed { 0 } else { audio.samples.len() * 2 + 44 },
        streamed,
        provider: provider_name(&cfg),
        model: cfg.stt_model(),
    });
    Outcome::Done { note }
}

fn failure(cfg: &Config, app: &AppInfo, e: SttError, audio: Audio, speech_ms: u64) -> Outcome {
    log_warn!("transcription failed: {:?} {} [{}]", e.code, e.message, e.detail);
    log_history(cfg, Entry { status: Status::Failed, error: Some(e.message.clone()), duration_ms: speech_ms, ..base_entry(cfg, app) });
    Outcome::Failed {
        title: error_title(e.code).to_string(),
        message: e.message,
        retry: cfg.keep_failed_audio.then_some(Failed { audio, speech_ms }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn friendly_names() {
        assert_eq!(friendly_exe("notepad.exe"), "Notepad");
        assert_eq!(friendly_exe("chrome.EXE"), "Chrome");
        assert_eq!(friendly_exe(""), "");
    }

    #[test]
    fn titles_cover_the_common_codes() {
        assert_eq!(error_title(SttErrorCode::AuthFailed), "Check your API key");
        assert_eq!(error_title(SttErrorCode::Timeout), "Connection problem");
        assert_eq!(error_title(SttErrorCode::Other), "Transcription failed");
    }

    #[test]
    fn preflight_asks_for_a_key() {
        // No stored key for this provider and no env override -> the card names the provider.
        let mut cfg = Config::default();
        cfg.stt.provider = crate::core::config::SttProviderId::Xai;
        if std::env::var_os("OPENTALK_SECRET_XAI").is_none() && crate::core::secrets::get("xai").is_none() {
            let (title, _) = preflight(&cfg).expect("missing key");
            assert!(title.contains("xAI"), "{title}");
        }
        cfg.stt.provider = crate::core::config::SttProviderId::Custom;
        cfg.stt.custom_base_url = "http://127.0.0.1:1".into();
        assert!(preflight(&cfg).is_none());
        cfg.stt.custom_base_url.clear();
        if std::env::var_os(crate::stt::ENV_BASE_URL).is_none() {
            assert!(preflight(&cfg).is_some());
        }
    }

    #[test]
    fn failure_keeps_audio_only_when_asked() {
        let mut cfg = Config::default();
        cfg.history_enabled = false;
        let audio = Audio { samples: vec![1, 2, 3], sample_rate: 16000 };
        let e = SttError::new(SttErrorCode::Timeout, "slow");
        match failure(&cfg, &AppInfo::default(), e.clone(), audio.clone(), 500) {
            Outcome::Failed { title, retry, .. } => {
                assert_eq!(title, "Connection problem");
                assert_eq!(retry.map(|f| f.audio.samples.len()), Some(3));
            }
            _ => panic!("expected a failure"),
        }
        cfg.keep_failed_audio = false;
        match failure(&cfg, &AppInfo::default(), e, audio, 500) {
            Outcome::Failed { retry, .. } => assert!(retry.is_none()),
            _ => panic!("expected a failure"),
        }
    }
}
