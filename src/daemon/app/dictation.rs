//! One dictation, start to finish. Nothing here blocks the UI thread for more than a few
//! milliseconds: the hotkey handler only captures the target, shows the pill and spawns helpers.
//!
//! - start thread: `Recorder::start` (20 ms with a prepared stream, up to seconds cold or on a
//!   hung driver), then the optional media pause (up to 120 ms);
//! - icon thread: the active-app icon (`IShellItemImageFactory` can take tens of ms cold);
//! - worker thread: stop + VAD + trim, STT (stream finish or batch), cleanup, paste, history.
//!
//! Helpers report back through [`MAILBOX`] + `WM_NOTE`; every note carries the dictation's `seq`,
//! so a note for a dictation that was cancelled or stopped meanwhile is cleaned up, not applied.
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use super::host::{App, TIMER_CAP, WM_NOTE, post};
use super::latency::{self, Sample};
use super::machine::{Input, MAX_RECORDING_MS, Phase};
use super::pipeline;
use crate::core::config::Config;
use crate::core::history::{self, Entry, Status};
use crate::core::timing::{now_ms, unix_ms};
use crate::daemon::appicon::{self, AppIcon};
use crate::daemon::audio::{self, Recorder, vad};
use crate::daemon::paste::{self, PasteOptions, PasteOutcome, Target};
use crate::daemon::pill::PillMode;
use crate::daemon::{media, sound};
use crate::stt::{self, Audio, StreamingSession, SttError, SttErrorCode};
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

/// The dictation that owns the microphone (from the hotkey press until stop or cancel).
pub struct RecSession {
    seq: u64,
    /// None while the start thread is still bringing the stream up.
    recorder: Option<Recorder>,
    /// Stop was requested before the recorder came up: stop as soon as it does.
    stop_pending: bool,
    target: Option<Target>,
    app: AppInfo,
    feed: Option<Feed>,
    /// `now_ms()` bits of the first microphone packet (0 = none yet).
    first_audio: Arc<AtomicU64>,
    t_hotkey: f64,
    hotkey_pill: f64,
    media_paused: bool,
    /// The safety cap stopped this recording.
    pub capped: bool,
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

/// What a helper thread reports to the UI thread.
enum Note {
    RecStarted { seq: u64, result: Result<Recorder, String> },
    MediaPaused { seq: u64 },
    Icon { seq: u64, icon: AppIcon },
    JobDone { seq: u64, outcome: Outcome },
}

/// Notes waiting for the UI thread (`WM_NOTE` wakes it).
static MAILBOX: Mutex<Vec<Note>> = Mutex::new(Vec::new());

fn notify(hwnd: isize, note: Note) {
    MAILBOX.lock().unwrap_or_else(|e| e.into_inner()).push(note);
    post(hwnd, WM_NOTE, 0);
}

/// Where the worker's audio comes from.
enum Capture {
    /// A live recorder: the worker stops it (stop + VAD + trim run off the UI thread).
    Live { recorder: Recorder, first_audio: Arc<AtomicU64>, t_hotkey: f64 },
    /// Audio kept from a failed attempt ("Retry").
    Kept { audio: Audio, speech_ms: u64 },
}

struct Job {
    id: u64,
    cfg: Config,
    capture: Capture,
    stream: Option<Box<dyn StreamingSession>>,
    target: Option<Target>,
    app: AppInfo,
    cancel: Arc<AtomicBool>,
    t_stop: f64,
    hotkey_pill: f64,
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

fn app_info(target: Option<&Target>) -> AppInfo {
    target.map(|t| AppInfo { exe: Some(t.exe_name.clone()), name: Some(friendly_exe(&t.exe_name)) }).unwrap_or_default()
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

fn take_session(feed: Option<Feed>) -> Option<Box<dyn StreamingSession>> {
    feed.and_then(|f| f.lock().ok().and_then(|mut g| g.take()))
}

/// Stop a recorder nobody wants any more, off the UI thread (stopping waits for its capture loop).
fn discard_recorder(recorder: Recorder, cfg: &Config) {
    let (mic, warm) = (cfg.mic_device_id.clone(), cfg.mic_warm_hold_secs);
    let spawned = std::thread::Builder::new().name("rec-discard".into()).spawn(move || {
        recorder.cancel();
        super::micpolicy::after_recording(mic.as_deref(), warm);
    });
    if let Err(e) = spawned {
        log_warn!("cannot spawn a thread to stop the recorder: {e}");
    }
}

impl App {
    /// Begin a dictation (state machine already moved to Recording). `t_trigger` is when the event
    /// that starts recording arrived (key press, or the end of hold-mode arming), `t0` when the
    /// hotkey went down.
    pub(super) fn start_recording(&mut self, t_trigger: f64, t0: f64) {
        // The pill comes first (PLAN §9.1): nothing before it may block, not even a DPAPI decrypt.
        self.pill.follow_cursor_display();
        self.pill.set_mode(PillMode::Preparing);
        let hotkey_pill = now_ms() - t_trigger;
        // The paste target is the window that had focus when the hotkey went down (the pill never
        // takes focus, so showing it first does not change the answer).
        let target = paste::capture_foreground();
        let cfg = self.cfg.clone();
        if let Some((title, msg)) = preflight(&cfg) {
            self.machine.step(Input::StartFailed);
            self.set_idle();
            self.pill.show_error(&title, &msg, false);
            return;
        }
        self.seq += 1;
        let seq = self.seq;
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

        // Bring the stream up off the UI thread, then pause media (both can block).
        let (hwnd, level, device, pause) = (self.hwnd, self.level.clone(), cfg.mic_device_id.clone(), cfg.pause_media);
        let spawned = std::thread::Builder::new().name("rec-start".into()).spawn(move || {
            let result = Recorder::start(device.as_deref(), level, Some(sink));
            let started = result.is_ok();
            notify(hwnd, Note::RecStarted { seq, result });
            if started && pause && media::pause_if_playing() {
                notify(hwnd, Note::MediaPaused { seq });
            }
        });
        if let Err(e) = spawned {
            log_warn!("cannot spawn the recorder thread: {e}");
            if let Some(s) = take_session(feed) {
                s.cancel();
            }
            self.machine.step(Input::StartFailed);
            self.set_idle();
            self.pill.show_error("Something went wrong", "OpenTalk could not start recording.", false);
            return;
        }
        if cfg.show_app_bubble {
            if let Some(t) = target.clone() {
                let hwnd = self.hwnd;
                let _ = std::thread::Builder::new().name("app-icon".into()).spawn(move || {
                    if let Some(icon) = appicon::load(&t, APP_ICON_PX) {
                        notify(hwnd, Note::Icon { seq, icon });
                    }
                });
            }
        }
        self.set_timer(TIMER_CAP, MAX_RECORDING_MS);
        self.rec = Some(RecSession {
            seq,
            recorder: None,
            stop_pending: false,
            app: app_info(target.as_ref()),
            target,
            feed,
            first_audio,
            t_hotkey: t0,
            hotkey_pill,
            media_paused: false,
            capped: false,
        });
    }

    /// Everything the helper threads reported since the last `WM_NOTE`.
    pub(super) fn on_notes(&mut self) {
        let notes: Vec<Note> = std::mem::take(&mut *MAILBOX.lock().unwrap_or_else(|e| e.into_inner()));
        for note in notes {
            match note {
                Note::RecStarted { seq, result } => self.on_rec_started(seq, result),
                Note::MediaPaused { seq } => match self.rec.as_mut().filter(|r| r.seq == seq) {
                    Some(rec) => rec.media_paused = true,
                    // The dictation ended before the pause landed: undo it right away.
                    None => media::resume(),
                },
                Note::Icon { seq, icon } => {
                    let live = self.rec.as_ref().is_some_and(|r| r.seq == seq) || self.job.as_ref().is_some_and(|j| j.id == seq);
                    if live {
                        if let Some(rec) = self.rec.as_mut().filter(|r| r.seq == seq) {
                            rec.app.name = Some(icon.name.clone());
                        }
                        self.pill.set_app(Some(icon));
                    }
                }
                Note::JobDone { seq, outcome } => self.on_job_done(seq, outcome),
            }
        }
    }

    fn on_rec_started(&mut self, seq: u64, result: Result<Recorder, String>) {
        let Some(rec) = self.rec.as_mut().filter(|r| r.seq == seq) else {
            // Cancelled while the stream was coming up.
            if let Ok(r) = result {
                discard_recorder(r, &self.cfg);
            }
            return;
        };
        match result {
            Err(e) => {
                log_warn!("cannot start the microphone: {e}");
                let rec = self.rec.take();
                if let Some(s) = take_session(rec.and_then(|r| r.feed)) {
                    s.cancel();
                }
                self.kill_timer(TIMER_CAP);
                // A stop that was already requested moved the machine to Thinking; reset it.
                self.machine.step(Input::StartFailed);
                if self.machine.phase == Phase::Thinking {
                    self.machine.step(Input::JobDone);
                }
                crate::daemon::hotkey::set_esc_capture(self.machine.is_busy());
                self.set_idle();
                self.pill.show_error("Microphone unavailable", "Check that a microphone is connected and not in use, then try again.", false);
            }
            Ok(recorder) => {
                log_info!(
                    "recording #{seq} started (pill {:.1} ms, mic {:.1} ms, stream={}, target={})",
                    rec.hotkey_pill,
                    recorder.activation_ms(),
                    rec.feed.is_some(),
                    rec.app.exe.as_deref().unwrap_or("?")
                );
                rec.recorder = Some(recorder);
                if rec.stop_pending {
                    // The stop landed while the mic was opening: honour it now, with its cue
                    // (OpenWhispr plays the stop cue for a stop requested mid-start too).
                    if self.cfg.audio_cues {
                        sound::play_stop();
                    }
                    self.finish_stop();
                    return;
                }
                self.pill.set_mode(PillMode::Recording);
                // The start cue plays once the microphone is live, so it is never recorded late.
                if self.cfg.audio_cues {
                    sound::play_start();
                }
            }
        }
    }

    /// Stop capturing (state machine already moved to Thinking).
    pub(super) fn stop_recording(&mut self) {
        let Some(starting) = self.rec.as_ref().map(|r| r.recorder.is_none()) else {
            self.machine.step(Input::JobDone);
            self.set_idle();
            return;
        };
        self.kill_timer(TIMER_CAP);
        self.pill.set_mode(PillMode::Thinking);
        if starting {
            // Still starting: stop right after it comes up.
            if let Some(rec) = self.rec.as_mut() {
                rec.stop_pending = true;
            }
            return;
        }
        if self.cfg.audio_cues {
            sound::play_stop();
        }
        self.finish_stop();
    }

    /// Hand the live recorder to a worker that stops it and transcribes.
    fn finish_stop(&mut self) {
        let Some(rec) = self.rec.take() else { return };
        let RecSession { seq, recorder, target, app, feed, first_audio, t_hotkey, hotkey_pill, media_paused, capped, .. } = rec;
        let Some(recorder) = recorder else { return };
        if media_paused {
            media::resume();
        }
        self.capped = capped;
        self.spawn_job(Job {
            id: seq,
            cfg: self.cfg.clone(),
            capture: Capture::Live { recorder, first_audio, t_hotkey },
            stream: take_session(feed),
            target,
            app,
            cancel: Arc::new(AtomicBool::new(false)),
            t_stop: now_ms(),
            hotkey_pill,
        });
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
        self.seq += 1;
        log_info!("retrying the last failed recording as #{}", self.seq);
        self.capped = false;
        self.spawn_job(Job {
            id: self.seq,
            cfg: self.cfg.clone(),
            capture: Capture::Kept { audio: failed.audio, speech_ms: failed.speech_ms },
            stream: None,
            app: app_info(target.as_ref()),
            target,
            cancel: Arc::new(AtomicBool::new(false)),
            t_stop: now_ms(),
            hotkey_pill: -1.0,
        });
    }

    fn spawn_job(&mut self, job: Job) {
        let id = job.id;
        let hwnd = self.hwnd;
        self.job = Some(ActiveJob { id, cancel: job.cancel.clone() });
        let spawned = std::thread::Builder::new().name("dictation-worker".into()).spawn(move || {
            let live = matches!(job.capture, Capture::Live { .. });
            let (mic, warm) = (job.cfg.mic_device_id.clone(), job.cfg.mic_warm_hold_secs);
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run_job(job))).unwrap_or_else(|_| Outcome::Failed {
                title: "Something went wrong".into(),
                message: "OpenTalk hit an internal error while processing that recording.".into(),
                retry: None,
            });
            notify(hwnd, Note::JobDone { seq: id, outcome });
            if live {
                super::micpolicy::after_recording(mic.as_deref(), warm);
            }
        });
        if let Err(e) = spawned {
            log_warn!("cannot start the worker thread: {e}");
            self.job = None;
            self.machine.step(Input::JobDone);
            crate::daemon::hotkey::set_esc_capture(false);
            self.set_idle();
            self.pill.show_error("Something went wrong", "OpenTalk could not start processing that recording.", false);
        }
    }

    fn on_job_done(&mut self, seq: u64, outcome: Outcome) {
        if !self.job.as_ref().is_some_and(|j| j.id == seq) {
            log_info!("dropping the result of cancelled dictation #{seq}");
            return;
        }
        self.job = None;
        self.machine.step(Input::JobDone);
        crate::daemon::hotkey::set_esc_capture(self.machine.is_busy());
        self.apply_outcome(outcome);
    }

    fn apply_outcome(&mut self, outcome: Outcome) {
        self.set_idle();
        let capped = std::mem::take(&mut self.capped);
        match outcome {
            Outcome::Done { note } => {
                self.last_failed = None;
                match note {
                    Some(reason) => self.pill.show_error("Couldn't paste", &reason, false),
                    None if capped => self.pill.show_error(
                        "Recording limit reached",
                        &format!("OpenTalk stops a recording after {} minutes; everything up to then was transcribed.", MAX_RECORDING_MS / 60_000),
                        false,
                    ),
                    None => {}
                }
            }
            Outcome::Nothing => self.pill.show_error(NO_SPEECH_TITLE, NO_SPEECH_BODY, false),
            Outcome::Failed { title, message, retry } => {
                let can_retry = retry.is_some();
                // Only the newest failure can be retried: an older recording is never kept behind
                // a failure that did not keep its own audio (keep_failed_audio off).
                self.last_failed = retry;
                self.pill.show_error(&title, &message, can_retry);
            }
            Outcome::Cancelled => {}
        }
    }

    pub(super) fn cancel_recording(&mut self) {
        self.kill_timer(TIMER_CAP);
        if let Some(rec) = self.rec.take() {
            if let Some(session) = take_session(rec.feed) {
                session.cancel();
            }
            if rec.media_paused {
                media::resume();
            }
            // A recorder still starting is discarded when its note arrives (seq no longer matches).
            if let Some(r) = rec.recorder {
                discard_recorder(r, &self.cfg);
            }
            log_info!("recording #{} cancelled", rec.seq);
        }
        self.set_idle();
    }

    pub(super) fn cancel_job(&mut self) {
        // A stop that arrived while the microphone was still coming up leaves the session here
        // (`stop_pending`) with no job yet: drop it too, or its recorder would start the job late
        // and paste after the user cancelled.
        if self.rec.is_some() {
            self.cancel_recording();
        }
        if let Some(job) = self.job.take() {
            job.cancel.store(true, Ordering::Relaxed);
            log_info!("dictation #{} cancelled", job.id);
        }
        self.capped = false;
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

/// The worker: stop + VAD -> STT -> cleanup -> paste -> history. Runs off the UI thread.
fn run_job(job: Job) -> Outcome {
    let Job { id, cfg, capture, stream, target, app, cancel, t_stop, hotkey_pill } = job;
    let cancelled = || cancel.load(Ordering::Relaxed);

    let (audio, speech_ms, hotkey_audio, stream) = match capture {
        Capture::Live { recorder, first_audio, t_hotkey } => {
            let (audio, stats) = recorder.stop_with_stats();
            let first = f64::from_bits(first_audio.load(Ordering::Relaxed));
            let hotkey_audio = if first > 0.0 { first - t_hotkey } else { -1.0 };
            let info = vad::analyze(&audio);
            log_info!(
                "recording #{id} stopped: {} ms captured, {} ms speech, {} packets, device_lost={}",
                audio.duration_ms(),
                info.speech_ms,
                stats.packets,
                stats.device_lost
            );
            if cancelled() || pipeline::is_silent(&info) {
                if let Some(s) = stream {
                    s.cancel();
                }
                if cancelled() {
                    return Outcome::Cancelled;
                }
                if cfg.save_discarded {
                    log_history(&cfg, discarded_entry(&cfg, &app, info.speech_ms));
                }
                return Outcome::Nothing;
            }
            (vad::trim(&audio, 150), info.speech_ms, hotkey_audio, stream)
        }
        Capture::Kept { audio, speech_ms } => (audio, speech_ms, -1.0, stream),
    };

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
        let t = Target { exe_name: "code.exe".into(), ..Target::default() };
        let a = app_info(Some(&t));
        assert_eq!((a.exe.as_deref(), a.name.as_deref()), (Some("code.exe"), Some("Code")));
        assert!(app_info(None).exe.is_none());
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
