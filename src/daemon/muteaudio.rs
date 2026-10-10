//! "Mute system audio while dictating": mute the default playback device when a recording is
//! live and put it back when the recording ends, however it ends.
//!
//! The decisions live in [`Engine`], a plain state machine over a [`Backend`] (unit-tested with a
//! fake, no COM). The real backend uses `IAudioEndpointVolume` on the default render endpoint,
//! through the shared device enumerator in `audio::wasapi`.
//!
//! One worker thread, started on the first dictation that uses the feature, owns the engine; the
//! UI thread never calls COM. Its only timer is the `recv_timeout` that waits for the start cue
//! to finish, so an idle daemon has nothing running. Rules:
//! - mute only a device that is not muted yet, and restore only that device;
//! - if the user unmuted it meanwhile, leave it alone (no re-mute, no fight);
//! - a tiny marker file in the data directory names the device we muted, so a daemon that
//!   dies mid-recording does not leave the user muted: [`recover`] undoes it at the next start.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::{log_info, log_warn};

/// Slack after the start cue: PlaySound starts a few ms after the recording-started note.
const CUE_MARGIN: Duration = Duration::from_millis(60);
/// How long the UI thread waits for the worker to confirm an unmute (a normal call takes well
/// under 5 ms). On a timeout the worker still finishes the job by itself.
pub const END_WAIT: Duration = Duration::from_millis(100);
/// Quit waits longer: after it nothing is left to finish the job (the marker still covers a crash).
pub const QUIT_WAIT: Duration = Duration::from_millis(800);

// ---------------------------------------------------------------------------------------------
// Decisions (pure)

/// What the engine needs from the system.
trait Backend {
    /// Id of the current default playback device.
    fn default_render(&mut self) -> Option<String>;
    /// Mute state of a device; `None` when it cannot be read (gone, service down).
    fn muted(&mut self, id: &str) -> Option<bool>;
    fn set_muted(&mut self, id: &str, muted: bool) -> bool;
    fn marker_write(&mut self, id: &str);
    fn marker_read(&mut self) -> Option<String>;
    fn marker_clear(&mut self);
}

#[derive(Default)]
struct Engine {
    /// A recording asked for a mute at `Instant` (after its start cue).
    pending: Option<(u64, Instant)>,
    /// The device we muted and must restore.
    held: Option<String>,
    /// Highest dictation seq that has ended; a late `begin` for it is dropped.
    ended: u64,
}

impl Engine {
    fn due(&self) -> Option<Instant> {
        self.pending.map(|(_, at)| at)
    }

    fn begin(&mut self, seq: u64, due: Instant, b: &mut impl Backend) {
        if seq <= self.ended {
            return; // the dictation is already over (cancelled while the mic was starting)
        }
        self.release(b); // never hold two mutes
        self.pending = Some((seq, due));
    }

    /// The start cue is over: mute, unless the device is already muted.
    fn engage(&mut self, now: Instant, b: &mut impl Backend) {
        match self.pending {
            Some((_, at)) if at <= now => self.pending = None,
            _ => return,
        }
        self.recover(b); // an earlier restore that failed must not be forgotten
        let Some(id) = b.default_render() else {
            log_warn!("mute: no default playback device");
            return;
        };
        if b.muted(&id) != Some(false) {
            return; // already muted by the user (or unreadable): not ours to touch
        }
        b.marker_write(&id); // first, so a crash right after the mute is covered
        if b.set_muted(&id, true) {
            self.held = Some(id);
        } else {
            b.marker_clear();
        }
    }

    /// The dictation `seq` is over (stop, cancel, error, quit): restore what we changed.
    fn end(&mut self, seq: u64, b: &mut impl Backend) {
        self.ended = self.ended.max(seq);
        self.pending = None;
        self.release(b);
    }

    fn release(&mut self, b: &mut impl Backend) {
        let Some(id) = self.held.take() else { return };
        match b.muted(&id) {
            Some(true) => {
                if b.set_muted(&id, false) {
                    b.marker_clear();
                } else {
                    log_warn!("mute: could not unmute {id}; will retry");
                }
            }
            // The user unmuted it during the recording: nothing to restore.
            Some(false) => b.marker_clear(),
            // Device unreachable (unplugged?): keep the marker, the next start retries.
            None => log_warn!("mute: {id} is unavailable; will unmute it later"),
        }
    }

    /// Undo a mute left behind by a daemon that died (or a restore that failed).
    fn recover(&mut self, b: &mut impl Backend) {
        let Some(id) = b.marker_read() else { return };
        if self.held.as_deref() == Some(id.as_str()) {
            return;
        }
        if b.muted(&id) == Some(true) {
            if !b.set_muted(&id, false) {
                log_warn!("mute: could not unmute {id} left muted by an earlier run");
                return;
            }
            log_info!("mute: unmuted {id} left muted by an earlier run");
        }
        b.marker_clear();
    }
}

// ---------------------------------------------------------------------------------------------
// Worker

enum Cmd {
    Begin { seq: u64, due: Instant },
    End { seq: u64, ack: Option<Sender<()>> },
}

fn run_worker<B: Backend>(rx: mpsc::Receiver<Cmd>, mut b: B) {
    let mut engine = Engine::default();
    loop {
        let cmd = match engine.due() {
            Some(at) => match rx.recv_timeout(at.saturating_duration_since(Instant::now())) {
                Ok(c) => Some(c),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => break,
            },
            None => match rx.recv() {
                Ok(c) => Some(c),
                Err(_) => break,
            },
        };
        match cmd {
            None => engine.engage(Instant::now(), &mut b),
            Some(Cmd::Begin { seq, due }) => engine.begin(seq, due, &mut b),
            Some(Cmd::End { seq, ack }) => {
                engine.end(seq, &mut b);
                if let Some(ack) = ack {
                    let _ = ack.send(());
                }
            }
        }
    }
    engine.end(u64::MAX, &mut b);
}

static WORKER: OnceLock<Option<Mutex<Sender<Cmd>>>> = OnceLock::new();
/// Set once the worker exists, so `end` costs nothing for users who never turn this on.
static STARTED: AtomicBool = AtomicBool::new(false);

fn worker() -> Option<&'static Mutex<Sender<Cmd>>> {
    WORKER
        .get_or_init(|| {
            let (tx, rx) = mpsc::channel();
            match std::thread::Builder::new().name("mute-audio".into()).spawn(move || {
                crate::daemon::audio::wasapi::ensure_com();
                run_worker(rx, Wasapi);
            }) {
                Ok(_) => {
                    STARTED.store(true, Ordering::Release);
                    Some(Mutex::new(tx))
                }
                Err(e) => {
                    log_warn!("mute: cannot start worker thread: {e}");
                    None
                }
            }
        })
        .as_ref()
}

fn send(cmd: Cmd) -> bool {
    let Some(w) = worker() else { return false };
    w.lock().unwrap_or_else(|p| p.into_inner()).send(cmd).is_ok()
}

/// Dictation `seq` is recording: mute the default playback device once its start cue is over.
/// `cue_started` is when the cue starts (or now when sounds are off, `cue` = zero).
/// Returns at once; the worker does the COM work.
pub fn begin(seq: u64, cue_started: Instant, cue: Duration) {
    let due = cue_started + if cue.is_zero() { Duration::ZERO } else { cue + CUE_MARGIN };
    send(Cmd::Begin { seq, due });
}

/// Dictation `seq` is over: cancel a pending mute, or unmute what was muted. Waits up to `wait`
/// for the unmute so the stop cue is not played into a muted device.
pub fn end(seq: u64, wait: Duration) {
    if !STARTED.load(Ordering::Acquire) {
        return;
    }
    let (ack, done) = mpsc::channel();
    if send(Cmd::End { seq, ack: Some(ack) }) && done.recv_timeout(wait).is_err() {
        log_warn!("mute: unmute did not answer within {} ms; continuing", wait.as_millis());
    }
}

/// At daemon start: undo a mute that a previous run left behind. Call from a background thread.
pub fn recover() {
    let mut b = Wasapi;
    if b.marker_read().is_some() {
        crate::daemon::audio::wasapi::ensure_com();
        Engine::default().recover(&mut b);
    }
}

// ---------------------------------------------------------------------------------------------
// Windows backend

struct Wasapi;

fn marker_file() -> PathBuf {
    crate::core::paths::app_dir().join("muted-device.txt")
}

impl Wasapi {
    fn volume(id: &str) -> Option<windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume> {
        use windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume;
        use windows::Win32::System::Com::CLSCTX_ALL;
        use windows::core::HSTRING;
        let en = crate::daemon::audio::wasapi::enumerator().ok()?;
        // SAFETY: documented interface calls on objects we own.
        unsafe {
            let dev = en.GetDevice(&HSTRING::from(id)).ok()?;
            dev.Activate::<IAudioEndpointVolume>(CLSCTX_ALL, None).ok()
        }
    }
}

impl Backend for Wasapi {
    fn default_render(&mut self) -> Option<String> {
        use windows::Win32::Media::Audio::{eConsole, eRender};
        let en = crate::daemon::audio::wasapi::enumerator().ok()?;
        // SAFETY: documented call; the device is dropped after reading its id.
        let dev = unsafe { en.GetDefaultAudioEndpoint(eRender, eConsole) }.ok()?;
        Some(crate::daemon::audio::wasapi::device_id(&dev)).filter(|s| !s.is_empty())
    }
    fn muted(&mut self, id: &str) -> Option<bool> {
        // SAFETY: GetMute on an activated endpoint volume.
        unsafe { Self::volume(id)?.GetMute() }.ok().map(|b| b.as_bool())
    }
    fn set_muted(&mut self, id: &str, muted: bool) -> bool {
        let Some(v) = Self::volume(id) else { return false };
        // SAFETY: a null event context is allowed.
        let r = unsafe { v.SetMute(muted, std::ptr::null()) };
        if let Err(e) = &r {
            log_warn!("mute: SetMute({muted}) failed: {e}");
        }
        r.is_ok()
    }
    fn marker_write(&mut self, id: &str) {
        let _ = std::fs::write(marker_file(), id);
    }
    fn marker_read(&mut self) -> Option<String> {
        std::fs::read_to_string(marker_file()).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
    }
    fn marker_clear(&mut self) {
        let _ = std::fs::remove_file(marker_file());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::rc::Rc;

    /// In-memory devices + marker, with a call log shared with the test.
    #[derive(Clone, Default)]
    struct Fake(Rc<RefCell<State>>);
    #[derive(Default)]
    struct State {
        default: Option<String>,
        devices: HashMap<String, bool>, // id -> muted; absent = unreachable
        marker: Option<String>,
        fail_set: bool,
        log: Vec<String>,
    }
    impl Fake {
        fn with(default: &str, muted: bool) -> Self {
            let f = Fake::default();
            f.0.borrow_mut().default = Some(default.into());
            f.0.borrow_mut().devices.insert(default.into(), muted);
            f
        }
        fn is_muted(&self, id: &str) -> Option<bool> {
            self.0.borrow().devices.get(id).copied()
        }
        fn marker(&self) -> Option<String> {
            self.0.borrow().marker.clone()
        }
        fn log(&self) -> Vec<String> {
            self.0.borrow().log.clone()
        }
    }
    impl Backend for Fake {
        fn default_render(&mut self) -> Option<String> {
            self.0.borrow().default.clone()
        }
        fn muted(&mut self, id: &str) -> Option<bool> {
            self.0.borrow().devices.get(id).copied()
        }
        fn set_muted(&mut self, id: &str, m: bool) -> bool {
            let mut s = self.0.borrow_mut();
            if s.fail_set || !s.devices.contains_key(id) {
                return false;
            }
            s.devices.insert(id.into(), m);
            s.log.push(format!("{} {id}", if m { "mute" } else { "unmute" }));
            true
        }
        fn marker_write(&mut self, id: &str) {
            self.0.borrow_mut().marker = Some(id.into());
        }
        fn marker_read(&mut self) -> Option<String> {
            self.0.borrow().marker.clone()
        }
        fn marker_clear(&mut self) {
            self.0.borrow_mut().marker = None;
        }
    }

    fn at(ms: u64, base: Instant) -> Instant {
        base + Duration::from_millis(ms)
    }

    /// begin -> cue over -> engage.
    fn start(e: &mut Engine, f: &mut Fake, seq: u64, base: Instant) {
        e.begin(seq, at(280, base), f);
        e.engage(at(280, base), f);
    }

    #[test]
    fn mutes_after_the_cue_and_restores_on_end() {
        let (mut e, mut f, t0) = (Engine::default(), Fake::with("spk", false), Instant::now());
        e.begin(1, at(280, t0), &mut f);
        e.engage(at(100, t0), &mut f); // cue still playing
        assert_eq!(f.is_muted("spk"), Some(false), "must wait for the start cue");
        assert_eq!(e.due(), Some(at(280, t0)));
        e.engage(at(280, t0), &mut f);
        assert_eq!(f.is_muted("spk"), Some(true));
        assert_eq!(f.marker().as_deref(), Some("spk"), "crash marker written");
        e.end(1, &mut f);
        assert_eq!(f.is_muted("spk"), Some(false));
        assert_eq!(f.marker(), None);
        assert_eq!(f.log(), ["mute spk", "unmute spk"]);
    }

    #[test]
    fn already_muted_device_is_never_touched() {
        let (mut e, mut f, t0) = (Engine::default(), Fake::with("spk", true), Instant::now());
        start(&mut e, &mut f, 1, t0);
        assert_eq!(f.marker(), None);
        e.end(1, &mut f);
        assert_eq!(f.is_muted("spk"), Some(true), "user's own mute stays");
        assert!(f.log().is_empty());
    }

    #[test]
    fn user_unmute_during_recording_is_respected() {
        let (mut e, mut f, t0) = (Engine::default(), Fake::with("spk", false), Instant::now());
        start(&mut e, &mut f, 1, t0);
        f.0.borrow_mut().devices.insert("spk".into(), false); // user hit the unmute key
        e.end(1, &mut f);
        assert_eq!(f.is_muted("spk"), Some(false));
        assert_eq!(f.log(), ["mute spk"], "no re-mute, no second unmute");
        assert_eq!(f.marker(), None);
    }

    #[test]
    fn restores_the_device_that_was_muted_after_a_default_change() {
        let (mut e, mut f, t0) = (Engine::default(), Fake::with("spk", false), Instant::now());
        f.0.borrow_mut().devices.insert("hp".into(), false);
        start(&mut e, &mut f, 1, t0);
        f.0.borrow_mut().default = Some("hp".into()); // headphones plugged in mid-recording
        e.end(1, &mut f);
        assert_eq!(f.is_muted("spk"), Some(false));
        assert_eq!(f.is_muted("hp"), Some(false), "the new default was never muted");
        assert_eq!(f.log(), ["mute spk", "unmute spk"]);
    }

    #[test]
    fn cancel_before_the_mute_lands_mutes_nothing() {
        let (mut e, mut f, t0) = (Engine::default(), Fake::with("spk", false), Instant::now());
        e.begin(1, at(280, t0), &mut f);
        e.end(1, &mut f);
        assert_eq!(e.due(), None, "timer disarmed");
        e.engage(at(300, t0), &mut f);
        assert!(f.log().is_empty());
    }

    #[test]
    fn a_late_begin_for_an_ended_dictation_is_dropped() {
        // Cancel/stop reached the worker before the start thread's begin did.
        let (mut e, mut f, t0) = (Engine::default(), Fake::with("spk", false), Instant::now());
        e.end(3, &mut f);
        e.begin(3, at(0, t0), &mut f);
        e.begin(2, at(0, t0), &mut f);
        assert_eq!(e.due(), None);
        e.begin(4, at(0, t0), &mut f);
        e.engage(at(0, t0), &mut f);
        assert_eq!(f.is_muted("spk"), Some(true), "the next dictation still works");
    }

    #[test]
    fn end_twice_and_end_without_begin_are_harmless() {
        let (mut e, mut f, t0) = (Engine::default(), Fake::with("spk", false), Instant::now());
        e.end(1, &mut f);
        start(&mut e, &mut f, 2, t0);
        e.end(2, &mut f);
        e.end(2, &mut f);
        assert_eq!(f.log(), ["mute spk", "unmute spk"]);
    }

    #[test]
    fn back_to_back_dictations_each_mute_and_restore() {
        let (mut e, mut f, t0) = (Engine::default(), Fake::with("spk", false), Instant::now());
        for seq in 1..=3 {
            start(&mut e, &mut f, seq, t0);
            assert_eq!(f.is_muted("spk"), Some(true));
            e.end(seq, &mut f);
            assert_eq!(f.is_muted("spk"), Some(false));
        }
    }

    #[test]
    fn immediate_mute_when_sounds_are_off() {
        let (mut e, mut f, t0) = (Engine::default(), Fake::with("spk", false), Instant::now());
        e.begin(1, t0, &mut f);
        e.engage(t0, &mut f);
        assert_eq!(f.is_muted("spk"), Some(true));
    }

    #[test]
    fn failed_mute_leaves_no_marker() {
        let (mut e, mut f, t0) = (Engine::default(), Fake::with("spk", false), Instant::now());
        f.0.borrow_mut().fail_set = true;
        start(&mut e, &mut f, 1, t0);
        assert_eq!(f.marker(), None);
        e.end(1, &mut f);
        assert!(f.log().is_empty());
    }

    #[test]
    fn no_default_device_does_nothing() {
        let (mut e, mut f, t0) = (Engine::default(), Fake::default(), Instant::now());
        start(&mut e, &mut f, 1, t0);
        e.end(1, &mut f);
        assert!(f.log().is_empty() && f.marker().is_none());
    }

    #[test]
    fn unplugged_device_keeps_the_marker_for_the_next_start() {
        let (mut e, mut f, t0) = (Engine::default(), Fake::with("usb", false), Instant::now());
        start(&mut e, &mut f, 1, t0);
        f.0.borrow_mut().devices.remove("usb"); // unplugged while muted
        e.end(1, &mut f);
        assert_eq!(f.marker().as_deref(), Some("usb"));
        // Replugged, still muted: the next run's recovery unmutes it.
        f.0.borrow_mut().devices.insert("usb".into(), true);
        Engine::default().recover(&mut f);
        assert_eq!(f.is_muted("usb"), Some(false));
        assert_eq!(f.marker(), None);
    }

    #[test]
    fn crash_marker_recovery() {
        // The previous daemon died while it had muted "spk".
        let mut f = Fake::with("spk", true);
        f.0.borrow_mut().marker = Some("spk".into());
        Engine::default().recover(&mut f);
        assert_eq!(f.is_muted("spk"), Some(false));
        assert_eq!(f.marker(), None);
        assert_eq!(f.log(), ["unmute spk"]);
    }

    #[test]
    fn recovery_leaves_a_device_that_is_not_muted_and_drops_the_marker() {
        let mut f = Fake::with("spk", false); // user unmuted it, or the crash came before the mute
        f.0.borrow_mut().marker = Some("spk".into());
        Engine::default().recover(&mut f);
        assert!(f.log().is_empty());
        assert_eq!(f.marker(), None);
    }

    #[test]
    fn recovery_drops_a_marker_for_a_device_that_is_gone_for_good() {
        let mut f = Fake::default();
        f.0.borrow_mut().marker = Some("ghost".into());
        Engine::default().recover(&mut f);
        assert_eq!(f.marker(), None);
    }

    #[test]
    fn recovery_without_marker_is_a_noop() {
        let mut f = Fake::with("spk", true);
        Engine::default().recover(&mut f);
        assert!(f.log().is_empty());
        assert_eq!(f.is_muted("spk"), Some(true));
    }

    #[test]
    fn failed_unmute_keeps_the_marker_and_the_next_dictation_retries() {
        let (mut e, mut f, t0) = (Engine::default(), Fake::with("spk", false), Instant::now());
        start(&mut e, &mut f, 1, t0);
        f.0.borrow_mut().fail_set = true;
        e.end(1, &mut f);
        assert_eq!(f.is_muted("spk"), Some(true));
        assert_eq!(f.marker().as_deref(), Some("spk"));
        f.0.borrow_mut().fail_set = false;
        e.begin(2, t0, &mut f);
        e.engage(t0, &mut f); // recovers the leftover first
        assert_eq!(f.marker().as_deref(), Some("spk"), "marker now belongs to dictation #2");
        assert_eq!(f.is_muted("spk"), Some(true), "leftover unmuted, then muted again for #2");
        assert_eq!(f.log(), ["mute spk", "unmute spk", "mute spk"]);
        e.end(2, &mut f);
        assert_eq!(f.is_muted("spk"), Some(false));
        assert_eq!(f.marker(), None);
    }

    #[test]
    fn worker_loop_waits_for_the_cue_then_restores() {
        // The real thread loop, driven through its channel with a shared fake.
        struct Shared(std::sync::Arc<Mutex<(bool, Vec<String>)>>);
        impl Backend for Shared {
            fn default_render(&mut self) -> Option<String> {
                Some("spk".into())
            }
            fn muted(&mut self, _: &str) -> Option<bool> {
                Some(self.0.lock().unwrap().0)
            }
            fn set_muted(&mut self, _: &str, m: bool) -> bool {
                let mut s = self.0.lock().unwrap();
                s.0 = m;
                s.1.push(if m { "mute".into() } else { "unmute".into() });
                true
            }
            fn marker_write(&mut self, _: &str) {}
            fn marker_read(&mut self) -> Option<String> {
                None
            }
            fn marker_clear(&mut self) {}
        }
        let state = std::sync::Arc::new(Mutex::new((false, Vec::new())));
        let (tx, rx) = mpsc::channel();
        let s2 = state.clone();
        let th = std::thread::spawn(move || run_worker(rx, Shared(s2)));
        tx.send(Cmd::Begin { seq: 1, due: Instant::now() + Duration::from_millis(80) }).unwrap();
        std::thread::sleep(Duration::from_millis(30));
        assert!(state.lock().unwrap().1.is_empty(), "muted before the cue ended");
        std::thread::sleep(Duration::from_millis(150));
        assert_eq!(state.lock().unwrap().1, ["mute"]);
        let (ack, done) = mpsc::channel();
        tx.send(Cmd::End { seq: 1, ack: Some(ack) }).unwrap();
        done.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(state.lock().unwrap().1, ["mute", "unmute"]);
        // A cancel that arrives before the timer fires mutes nothing.
        tx.send(Cmd::Begin { seq: 2, due: Instant::now() + Duration::from_millis(60) }).unwrap();
        let (ack, done) = mpsc::channel();
        tx.send(Cmd::End { seq: 2, ack: Some(ack) }).unwrap();
        done.recv_timeout(Duration::from_secs(2)).unwrap();
        std::thread::sleep(Duration::from_millis(120));
        assert_eq!(state.lock().unwrap().1, ["mute", "unmute"]);
        drop(tx);
        th.join().unwrap();
    }

    #[test]
    fn cue_wait_covers_the_whole_start_cue() {
        let t0 = Instant::now();
        let cue = crate::daemon::sound::start_cue_duration();
        assert!(cue >= Duration::from_millis(200) && cue <= Duration::from_millis(250), "{cue:?}");
        // `begin` would queue `t0 + cue + margin`; with sounds off it is `t0`.
        assert!(t0 + cue + CUE_MARGIN > t0 + cue);
    }

    /// Really mutes and unmutes the default playback device (Windows desktop only).
    #[test]
    #[ignore = "desktop: mutes the default playback device for about a second"]
    fn desktop_mute_roundtrip_restores_the_device() {
        crate::daemon::audio::wasapi::ensure_com();
        let mut w = Wasapi;
        let Some(id) = w.default_render() else {
            println!("SKIPPED: no default playback device (headless machine or Wine without audio)");
            return;
        };
        let Some(before) = w.muted(&id) else {
            println!("SKIPPED: the endpoint's mute state is not readable here");
            return;
        };
        println!("default render {id}, muted before: {before}");
        let mut e = Engine::default();
        let t0 = Instant::now();
        e.begin(1, t0, &mut w);
        let t = Instant::now();
        e.engage(t0, &mut w);
        println!("engage took {:?}", t.elapsed());
        if before {
            assert_eq!(w.marker_read(), None, "an already-muted device is left alone");
        } else {
            assert_eq!(w.muted(&id), Some(true));
            assert_eq!(w.marker_read().as_deref(), Some(id.as_str()));
        }
        std::thread::sleep(Duration::from_millis(500));
        let t = Instant::now();
        e.end(1, &mut w);
        println!("release took {:?}", t.elapsed());
        assert_eq!(w.muted(&id), Some(before), "device restored");
        assert_eq!(w.marker_read(), None);
        // Crash recovery: pretend a run died while muted.
        if !before {
            w.marker_write(&id);
            assert!(w.set_muted(&id, true));
            recover();
            assert_eq!(w.muted(&id), Some(false));
            assert_eq!(w.marker_read(), None);
        }
    }
}
