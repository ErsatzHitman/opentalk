//! One dictation, start to finish. Nothing here blocks the UI thread for more than a few
//! milliseconds: the hotkey handler only captures the target, shows the pill and spawns helpers.
//!
//! - start thread: `Recorder::start` (20 ms with a prepared stream, up to seconds cold or on a
//!   hung driver; abandoned after `START_TIMEOUT`), then the optional media pause (up to 120 ms);
//! - icon thread: the active-app icon (`IShellItemImageFactory` can take tens of ms cold);
//! - worker thread: stop + VAD + trim, STT (stream finish or batch), cleanup, paste, history.
//!
//! Helpers report back through [`MAILBOX`] + `WM_NOTE`; every note carries the dictation's `seq`,
//! so a note for a dictation that was cancelled or stopped meanwhile is cleaned up, not applied.
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use super::host::{App, TIMER_CAP, WM_NOTE, post};
use super::latency::{self, Sample};
use super::machine::{Input, MAX_RECORDING_MS, Phase};
use super::pipeline;
use crate::core::config::Config;
use crate::core::history::{self, Entry, Status};
use crate::core::timing::{now_ms, unix_ms};
use crate::daemon::appicon::{self, AppIcon};
use crate::daemon::foreground::{self, Change};
use crate::daemon::audio::{self, Recorder, vad};
use crate::daemon::paste::{self, PasteOptions, PasteOutcome, Target};
use crate::daemon::pill::PillMode;
use crate::daemon::{media, muteaudio, sound};
use crate::stt::{self, Audio, StreamingSession, SttError, SttErrorCode};
use crate::{log_debug, log_info, log_warn};

/// Shared slot for the streaming session: the capture thread feeds it, `stop` takes it.
type Feed = Arc<Mutex<Option<Box<dyn StreamingSession>>>>;

/// Icon size requested for the active-app bubble (the pill downsamples to its DPI).
const APP_ICON_PX: u32 = appicon::BUBBLE_ICON_PX;
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
    /// "Mute system audio" was requested for this recording and not yet released.
    mute_audio: bool,
    /// Answers once the unmute asked for at the stop has been done (see [`App::release_mute_rx`]).
    unmuted: Option<mpsc::Receiver<()>>,
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
    /// The async icon lookup for `path` finished (`None`: the app has no readable icon).
    Icon { seq: u64, path: String, icon: Option<AppIcon> },
    /// The foreground tracker published another app.
    Foreground,
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

/// The paste will put the user's clipboard back afterwards, so it needs a snapshot of it.
fn restores_clipboard(cfg: &Config) -> bool {
    cfg.auto_paste && !cfg.keep_in_clipboard
}

fn friendly_exe(exe: &str) -> String {
    let stem = exe.trim_end_matches(".exe").trim_end_matches(".EXE");
    let mut c = stem.chars();
    c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
}

fn app_info(target: Option<&Target>) -> AppInfo {
    target.map(|t| AppInfo { exe: Some(t.exe_name.clone()), name: Some(friendly_exe(&t.exe_name)) }).unwrap_or_default()
}

/// The paste goes to the app the bubble shows *now*: the window in front when the text is ready (or,
/// if that is the tray or a shell surface, the last real one), not the one the hotkey went down in.
/// Runs on the dictation worker, off the UI thread.
fn retarget(target: Option<Target>, app: AppInfo) -> (Option<Target>, AppInfo) {
    let now = foreground::resolve(paste::capture_foreground(), foreground::last().as_ref(), foreground::own_pid());
    match now {
        Some(t) if target.as_ref().is_none_or(|o| o.hwnd != t.hwnd) => {
            let mut a = app_info(Some(&t));
            if let Some(icon) = appicon::cached(&t, APP_ICON_PX) {
                a.name = Some(icon.name);
            }
            (Some(t), a)
        }
        _ => (target, app),
    }
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

/// How long the start thread waits for `Recorder::start` before giving up on the microphone.
const START_TIMEOUT: Duration = Duration::from_secs(5);
const START_TIMEOUT_MSG: &str = "microphone did not start within 5 s";

/// Hand-off between [`start_with_timeout`] and its helper thread.
struct StartSlot<T> {
    result: Option<Result<T, String>>,
    /// The waiter gave up: whatever the helper produces now is not delivered.
    abandoned: bool,
}

/// Run `starter` on a helper thread and wait at most `timeout` for it. On timeout the caller gets
/// `Err` at once; if `starter` succeeds later, its value goes to `discard` on the helper thread
/// (never to the caller), so a stream that comes up late is stopped instead of left running.
/// One helper thread per call, and it ends when `starter` returns.
fn start_with_timeout<T: Send + 'static>(
    timeout: Duration,
    starter: impl FnOnce() -> Result<T, String> + Send + 'static,
    discard: impl FnOnce(T) + Send + 'static,
) -> Result<T, String> {
    let slot = Arc::new((Mutex::new(StartSlot { result: None, abandoned: false }), Condvar::new()));
    let helper_slot = slot.clone();
    let spawned = std::thread::Builder::new().name("rec-open".into()).spawn(move || {
        let result = starter();
        let (lock, cv) = &*helper_slot;
        let mut g = lock.lock().unwrap_or_else(|e| e.into_inner());
        if g.abandoned {
            drop(g);
            if let Ok(late) = result {
                discard(late);
            }
        } else {
            g.result = Some(result);
            cv.notify_one();
        }
    });
    if let Err(e) = spawned {
        return Err(format!("cannot spawn the microphone thread: {e}"));
    }
    let (lock, cv) = &*slot;
    let g = lock.lock().unwrap_or_else(|e| e.into_inner());
    let (mut g, _) = cv.wait_timeout_while(g, timeout, |s| s.result.is_none()).unwrap_or_else(|e| e.into_inner());
    // A result that landed right at the deadline still counts.
    match g.result.take() {
        Some(result) => result,
        None => {
            g.abandoned = true;
            Err(START_TIMEOUT_MSG.to_string())
        }
    }
}

/// How a finished paste ends the job: the history note for a delivered paste (`Some` when it was
/// only copied), or `Err(Outcome::Cancelled)` when Esc stopped it (no history entry, no latency
/// sample).
fn paste_note(outcome: PasteOutcome) -> Result<Option<String>, Outcome> {
    match outcome {
        PasteOutcome::Pasted => Ok(None),
        PasteOutcome::CopiedOnly { reason } => Ok(Some(reason)),
        PasteOutcome::Cancelled => Err(Outcome::Cancelled),
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
        // takes focus, so showing it first does not change the answer), unless that is the tray or a
        // shell surface: then the app the user was last in (see `foreground::resolve`).
        let target = self.front_target();
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
                if let Some(feed) = &feed
                    && let Ok(mut slot) = feed.lock()
                    && let Some(session) = slot.as_mut()
                {
                    session.push(pcm);
                }
            })
        };
        self.level.store(0f32.to_bits(), Ordering::Relaxed);

        // Bring the stream up off the UI thread, then pause media (both can block).
        let (hwnd, level, device, pause) = (self.hwnd, self.level.clone(), cfg.mic_device_id.clone(), cfg.pause_media);
        // The mute waits for the start cue (played when the note below is handled), then follows
        // the media pause: pause media, then mute.
        let (mute, cue) = (cfg.mute_system_audio, if cfg.audio_cues { sound::start_cue_duration() } else { Duration::ZERO });
        let prefetch = restores_clipboard(&cfg);
        let warm = cfg.mic_warm_hold_secs;
        let spawned = std::thread::Builder::new().name("rec-start".into()).spawn(move || {
            // Read the user's clipboard now (on its own worker), so the paste after transcription
            // does not have to: delay-rendered owners (Excel ranges) take seconds to render.
            if prefetch {
                paste::prepare_clipboard_snapshot();
            }
            // A hung driver must not leave the pill in Thinking until Esc (a stop may be pending).
            let mic = device.clone();
            let result = start_with_timeout(
                START_TIMEOUT,
                move || Recorder::start(device.as_deref(), level, Some(sink)),
                // A recorder that comes up after the timeout is stopped right here, on the helper.
                move |late: Recorder| {
                    late.cancel();
                    super::micpolicy::after_recording(mic.as_deref(), warm);
                },
            );
            let started = result.is_ok();
            let cue_started = Instant::now();
            notify(hwnd, Note::RecStarted { seq, result });
            if started && pause && media::pause_if_playing() {
                notify(hwnd, Note::MediaPaused { seq });
            }
            if started && mute {
                muteaudio::begin(seq, cue_started, cue);
            }
        });
        if let Err(e) = spawned {
            log_warn!("cannot spawn the recorder thread: {e}");
            paste::discard_clipboard_snapshot();
            if let Some(s) = take_session(feed) {
                s.cancel();
            }
            self.machine.step(Input::StartFailed);
            self.set_idle();
            self.pill.show_error("Something went wrong", "OpenTalk could not start recording.", false);
            return;
        }
        let mut app = app_info(target.as_ref());
        if cfg.show_app_bubble {
            // Bench hook: `OPENTALK_BENCH_APP_EXE=<exe>` looks up that app's icon whatever is in front, or
            // when nothing is (bench/bench.ps1 uses it where no packaged app can be brought to the
            // foreground, e.g. a locked desktop that has no foreground window at all).
            let bench_exe = std::env::var("OPENTALK_BENCH_APP_EXE").ok().filter(|p| !p.is_empty());
            let icon_target = match (bench_exe, target.clone()) {
                (Some(p), t) => Some(Target { exe_path: p, pid: 0, ..t.unwrap_or_default() }),
                (None, t) => t,
            };
            foreground::follow_begin(icon_target.clone());
            if let Some(name) = self.show_app(seq, icon_target) {
                app.name = Some(name);
            }
        }
        self.set_timer(TIMER_CAP, MAX_RECORDING_MS);
        self.rec = Some(RecSession {
            seq,
            recorder: None,
            stop_pending: false,
            app,
            target,
            feed,
            first_audio,
            t_hotkey: t0,
            hotkey_pill,
            media_paused: false,
            mute_audio: cfg.mute_system_audio,
            unmuted: None,
            capped: false,
        });
    }

    /// Run the foreground tracker exactly while the app bubble is on.
    pub(super) fn sync_foreground(&self) {
        let hwnd = self.hwnd;
        foreground::configure(self.cfg.show_app_bubble, APP_ICON_PX, Box::new(move || notify(hwnd, Note::Foreground)));
    }

    /// The window a dictation started now would paste into: the one in front, or the last real one
    /// when the front is the tray or a transient shell surface. With the bubble off nothing is
    /// tracked and it is simply the front window.
    fn front_target(&self) -> Option<Target> {
        let fresh = paste::capture_foreground();
        if self.cfg.show_app_bubble { foreground::resolve(fresh, foreground::last().as_ref(), foreground::own_pid()) } else { fresh }
    }

    /// Put the app's icon in the bubble: at once from the cache (the tracker resolved it when the app
    /// came to the front), else from a helper thread (packaged apps read their manifest and PNGs from
    /// disk). Returns the app's friendly name when the cache had it.
    fn show_app(&mut self, seq: u64, target: Option<Target>) -> Option<String> {
        let t = target?;
        if let Some(icon) = appicon::cached(&t, APP_ICON_PX) {
            let name = icon.name.clone();
            self.pill.set_app(Some(icon));
            return Some(name);
        }
        let hwnd = self.hwnd;
        let _ = std::thread::Builder::new().name("app-icon".into()).spawn(move || {
            let started = now_ms();
            let exe = t.exe_path.rsplit(['\\', '/']).next().unwrap_or("").to_ascii_lowercase();
            let icon = appicon::load(&t, APP_ICON_PX);
            match &icon {
                Some(i) => log_debug!("app icon for {exe}: {:?} in {:.0} ms", i.name, now_ms() - started),
                None => log_debug!("app icon for {exe}: none in {:.0} ms", now_ms() - started),
            }
            notify(hwnd, Note::Icon { seq, path: t.exe_path, icon });
        });
        None
    }

    /// The tracker published another foreground app. While a dictation is recording or being
    /// transcribed the paste target and the bubble follow it (the text goes to the app the bubble shows).
    fn on_foreground(&mut self) {
        if !self.cfg.show_app_bubble {
            return;
        }
        let seq = match (&self.rec, &self.job) {
            (Some(r), _) => r.seq,
            (None, Some(j)) => j.id,
            _ => return,
        };
        let Some(seen) = foreground::last() else { return };
        let change = foreground::follow_observe(&seen);
        if change == Change::Same {
            return;
        }
        if let Some(rec) = self.rec.as_mut() {
            rec.target = Some(seen.clone());
            if change == Change::App {
                rec.app = app_info(Some(&seen));
            }
        }
        if change == Change::App {
            let name = self.show_app(seq, Some(seen));
            if let (Some(rec), Some(name)) = (self.rec.as_mut(), name) {
                rec.app.name = Some(name);
            }
        }
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
                Note::Icon { seq, path, icon } => {
                    let live = self.rec.as_ref().is_some_and(|r| r.seq == seq) || self.job.as_ref().is_some_and(|j| j.id == seq);
                    // The app may have changed again while this lookup ran: only the one the bubble is for counts.
                    if live && foreground::follow_shows(&path) {
                        if let (Some(rec), Some(i)) = (self.rec.as_mut().filter(|r| r.seq == seq), &icon) {
                            rec.app.name = Some(i.name.clone());
                        }
                        self.pill.set_app(icon);
                    }
                }
                Note::Foreground => self.on_foreground(),
                Note::JobDone { seq, outcome } => self.on_job_done(seq, outcome),
            }
        }
        self.sync_tray_popup();
    }

    fn on_rec_started(&mut self, seq: u64, result: Result<Recorder, String>) {
        let Some(rec) = self.rec.as_mut().filter(|r| r.seq == seq) else {
            // Cancelled while the stream was coming up.
            paste::discard_clipboard_snapshot();
            if let Ok(r) = result {
                discard_recorder(r, &self.cfg);
            }
            return;
        };
        match result {
            Err(e) => {
                log_warn!("cannot start the microphone: {e}");
                paste::discard_clipboard_snapshot();
                self.release_mute();
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
                    self.finish_stop(self.cfg.audio_cues);
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
        // Speakers back first: the stop cue must be heard, and media resumes after this. The request
        // is queued here (ordered against anything that follows); the wait for its answer, the cue
        // and the media resume happen on a worker (`after_unmute`), so the pill never waits for them.
        let unmuted = self.release_mute_rx();
        if let Some(rec) = self.rec.as_mut() {
            rec.unmuted = unmuted;
        }
        self.pill.set_mode(PillMode::Thinking);
        if starting {
            // Still starting: stop right after it comes up.
            if let Some(rec) = self.rec.as_mut() {
                rec.stop_pending = true;
            }
            return;
        }
        self.finish_stop(self.cfg.audio_cues);
    }

    /// Give the speakers back if "Mute system audio" took them for this recording. Idempotent. The
    /// request is queued at once; nothing waits for it on the UI thread.
    pub(super) fn release_mute(&mut self) {
        drop(self.release_mute_rx());
    }

    /// Queue the unmute (if this recording muted) and return the receiver that answers when it is
    /// done, for whatever must follow it (the stop cue, the media resume).
    fn release_mute_rx(&mut self) -> Option<mpsc::Receiver<()>> {
        let rec = self.rec.as_mut().filter(|r| r.mute_audio)?;
        rec.mute_audio = false;
        muteaudio::end_nowait(rec.seq)
    }

    /// Shutdown only: nothing runs after this, so wait for the unmute to land.
    fn release_mute_blocking(&mut self, wait: Duration) {
        if let Some(rec) = self.rec.as_mut().filter(|r| r.mute_audio) {
            rec.mute_audio = false;
            muteaudio::end(rec.seq, wait);
        }
    }

    /// Hand the live recorder to a worker that stops it and transcribes. After the unmute (if any):
    /// the stop cue (`cue`), then the media resume.
    fn finish_stop(&mut self, cue: bool) {
        let Some(rec) = self.rec.take() else { return };
        let RecSession { seq, recorder, target, app, feed, first_audio, t_hotkey, hotkey_pill, media_paused, unmuted, capped, .. } = rec;
        let Some(recorder) = recorder else { return };
        after_unmute(unmuted, move || {
            if cue {
                sound::play_stop();
            }
            if media_paused {
                media::resume();
            }
        });
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
        let target = self.front_target();
        self.pill.follow_cursor_display();
        self.pill.set_mode(PillMode::Thinking);
        self.seq += 1;
        log_info!("retrying the last failed recording as #{}", self.seq);
        self.capped = false;
        let mut app = app_info(target.as_ref());
        if self.cfg.show_app_bubble {
            foreground::follow_begin(target.clone());
            if let Some(name) = self.show_app(self.seq, target.clone()) {
                app.name = Some(name);
            }
        }
        self.spawn_job(Job {
            id: self.seq,
            cfg: self.cfg.clone(),
            capture: Capture::Kept { audio: failed.audio, speech_ms: failed.speech_ms },
            stream: None,
            app,
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
            // A retry has no recording start to hook into: read the clipboard while it transcribes.
            if !live && restores_clipboard(&job.cfg) {
                paste::prepare_clipboard_snapshot();
            }
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
            paste::discard_clipboard_snapshot();
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
        // A paste consumed the clipboard snapshot; any other outcome leaves it unused.
        paste::discard_clipboard_snapshot();
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
        paste::discard_clipboard_snapshot();
        let unmuted = self.release_mute_rx(); // media resumes after it
        if let Some(rec) = self.rec.take() {
            if let Some(session) = take_session(rec.feed) {
                session.cancel();
            }
            if rec.media_paused {
                after_unmute(unmuted, media::resume);
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
            paste::discard_clipboard_snapshot();
            job.cancel.store(true, Ordering::Relaxed);
            log_info!("dictation #{} cancelled", job.id);
        }
        self.capped = false;
        self.set_idle();
    }

    /// Shutdown: drop whatever is in flight.
    pub(super) fn cancel_everything(&mut self) {
        self.kill_timer(super::host::TIMER_ARM);
        // Quitting: nothing runs after this, so wait longer for the unmute to land.
        self.release_mute_blocking(muteaudio::QUIT_WAIT);
        self.cancel_recording();
        self.cancel_job();
    }
}

/// Run `then` once the unmute requested at a stop has been done, keeping the order the mute feature
/// guarantees (unmute, then the stop cue, then the media resume) without making the UI thread wait
/// for it. With nothing to wait for `then` runs right here (it only posts to worker threads);
/// otherwise a short-lived thread waits up to [`muteaudio::END_WAIT`] for the answer, then runs it.
fn after_unmute(unmuted: Option<mpsc::Receiver<()>>, then: impl FnOnce() + Send + 'static) {
    after_unmute_within(unmuted, muteaudio::END_WAIT, then);
}

fn after_unmute_within(unmuted: Option<mpsc::Receiver<()>>, wait: Duration, then: impl FnOnce() + Send + 'static) {
    let Some(rx) = unmuted else { return then() };
    // `then` must survive a failed spawn (the closure is moved into the thread otherwise)
    type Job = Box<dyn FnOnce() + Send>;
    let slot: Arc<Mutex<Option<Job>>> = Arc::new(Mutex::new(Some(Box::new(then))));
    let thread_slot = slot.clone();
    let spawned = std::thread::Builder::new().name("rec-stop-tail".into()).spawn(move || {
        if rx.recv_timeout(wait).is_err() {
            log_warn!("mute: unmute did not answer within {} ms; continuing", wait.as_millis());
        }
        let job = thread_slot.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(job) = job {
            job();
        }
    });
    if spawned.is_err() {
        let job = slot.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(job) = job {
            job();
        }
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
    if cfg.history_enabled
        && let Err(e) = history::append_new(entry)
    {
        log_warn!("history: {e}");
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
                "recording #{id} stopped: {} ms captured, {} ms speech, {} packets, device_lost={}; \
                 packet gap mean {:.1} max {:.1} ms, {} discontinuities; level gate open {:.0}% of the time, noise floor {:.0} dBFS",
                audio.duration_ms(),
                info.speech_ms,
                stats.packets,
                stats.device_lost,
                stats.mean_gap_ms,
                stats.max_gap_ms,
                stats.discontinuities,
                stats.gate_open_pct,
                stats.floor_dbfs
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

    let (target, app) = if cfg.auto_paste && cfg.show_app_bubble { retarget(target, app) } else { (target, app) };
    let t_paste = now_ms();
    let note = if cfg.auto_paste {
        let opts = PasteOptions { restore_clipboard: restores_clipboard(&cfg), smart_spacing: cfg.smart_spacing };
        // Esc during the paste step (the modifier wait, the focus return) stops the keystroke.
        match paste_note(paste::paste_text_cancellable(&cleaned.text, target.as_ref(), opts, &cancelled)) {
            Ok(note) => note,
            Err(outcome) => return outcome,
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

    /// The mute feature's ordering guarantee survives moving the wait off the UI thread: the unmute
    /// finishes, then the stop cue plays, then the media resumes; and the caller is not held up.
    #[test]
    fn stop_tail_keeps_unmute_then_cue_then_media_order_without_blocking() {
        let log: Arc<Mutex<Vec<&'static str>>> = Arc::new(Mutex::new(Vec::new()));
        let (ack, rx) = mpsc::channel();
        let l = log.clone();
        let worker = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(80)); // a slow COM unmute
            l.lock().unwrap().push("unmute");
            let _ = ack.send(());
        });
        let l = log.clone();
        let t0 = Instant::now();
        after_unmute_within(Some(rx), Duration::from_millis(500), move || {
            l.lock().unwrap().push("cue");
            l.lock().unwrap().push("media");
        });
        assert!(t0.elapsed() < Duration::from_millis(30), "the UI thread waited {:?}", t0.elapsed());
        worker.join().unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        while log.lock().unwrap().len() < 3 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(*log.lock().unwrap(), ["unmute", "cue", "media"]);
    }

    /// An unmute that never answers delays the cue by the wait and no longer (the worker finishes it).
    #[test]
    fn stop_tail_gives_up_waiting_after_the_budget_and_runs_directly_without_a_mute() {
        let (_ack, rx) = mpsc::channel::<()>();
        let (done, got) = mpsc::channel();
        let t0 = Instant::now();
        after_unmute_within(Some(rx), Duration::from_millis(60), move || {
            let _ = done.send(Instant::now());
        });
        let at = got.recv_timeout(Duration::from_secs(3)).expect("runs after the budget");
        let waited = at.duration_since(t0);
        assert!(waited >= Duration::from_millis(55) && waited < Duration::from_millis(1000), "{waited:?}");
        // nothing to wait for: runs right here, before the call returns
        let ran = Arc::new(AtomicBool::new(false));
        let r = ran.clone();
        after_unmute_within(None, Duration::from_secs(5), move || r.store(true, Ordering::SeqCst));
        assert!(ran.load(Ordering::SeqCst));
    }

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
        let mut cfg = Config { history_enabled: false, ..Config::default() };
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

    /// Stands in for a `Recorder`: counts how often it is dropped.
    struct Probe(Arc<std::sync::atomic::AtomicUsize>);
    impl Drop for Probe {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn wait_for(cond: impl Fn() -> bool) {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !cond() {
            assert!(std::time::Instant::now() < deadline, "condition never became true");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn hung_start_times_out() {
        // The starter hangs until the test lets it go (a bad driver), far past the 50 ms timeout.
        let (release, gate) = std::sync::mpsc::channel::<()>();
        let t = std::time::Instant::now();
        let r = start_with_timeout(
            Duration::from_millis(50),
            move || {
                let _ = gate.recv();
                Ok(())
            },
            |()| {},
        );
        assert_eq!(r, Err(START_TIMEOUT_MSG.to_string()));
        assert!(t.elapsed() >= Duration::from_millis(50), "waited for the timeout: {:?}", t.elapsed());
        drop(release); // lets the helper thread finish
    }

    #[test]
    fn late_start_is_dropped_never_delivered() {
        let dropped = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let discarded = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (d, dis) = (dropped.clone(), discarded.clone());
        let (release, gate) = std::sync::mpsc::channel::<()>();
        let r = start_with_timeout(
            Duration::from_millis(50),
            move || {
                let _ = gate.recv(); // comes up only after the caller gave up
                Ok(Probe(d))
            },
            move |late| {
                dis.fetch_add(1, Ordering::SeqCst);
                drop(late);
            },
        );
        assert!(r.is_err(), "timeout reports an error");
        assert_eq!(dropped.load(Ordering::SeqCst), 0, "still starting at the deadline");
        release.send(()).unwrap(); // the stream finally comes up
        wait_for(|| dropped.load(Ordering::SeqCst) == 1);
        assert_eq!(discarded.load(Ordering::SeqCst), 1, "the late value went through discard exactly once");
    }

    #[test]
    fn fast_start_is_delivered_and_not_discarded() {
        let dropped = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let discarded = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (d, dis) = (dropped.clone(), discarded.clone());
        let r = start_with_timeout(
            Duration::from_secs(5),
            move || Ok(Probe(d)),
            move |late| {
                dis.fetch_add(1, Ordering::SeqCst);
                drop(late);
            },
        );
        let Ok(probe) = r else { panic!("delivered") };
        assert_eq!(dropped.load(Ordering::SeqCst), 0, "the caller owns it");
        drop(probe);
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
        assert_eq!(discarded.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn start_error_is_passed_through() {
        let r = start_with_timeout(Duration::from_secs(5), || Err::<(), _>("no device".to_string()), |()| {});
        assert_eq!(r, Err("no device".to_string()));
    }

    #[test]
    fn cancelled_paste_maps_to_cancelled_outcome() {
        assert!(matches!(paste_note(PasteOutcome::Cancelled), Err(Outcome::Cancelled)));
        assert!(matches!(paste_note(PasteOutcome::Pasted), Ok(None)));
        match paste_note(PasteOutcome::CopiedOnly { reason: "r".into() }) {
            Ok(Some(r)) => assert_eq!(r, "r"),
            _ => panic!("expected the copied-only note"),
        }
    }
}
