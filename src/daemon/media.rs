//! "Pause media while dictating" (OpenWhispr pauseMediaOnDictation). TASK win-input implements:
//! pause only if a media session is actually playing (WinRT GlobalSystemMediaTransportControls
//! via the `windows` crate if practical, else skip), and resume only what we paused.
//!
//! All WinRT work runs on one lazily-started worker thread that caches the session manager, so
//! the hotkey path never waits more than [`PAUSE_BUDGET`]: if the worker cannot answer in time
//! the call returns `false` and the worker abandons the pause (it checks the deadline before
//! touching any session). Call [`warm`] at start-up to take the first-request cost off the
//! first dictation.

use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use windows::Media::Control::{
    GlobalSystemMediaTransportControlsSession as Session, GlobalSystemMediaTransportControlsSessionManager as Manager,
    GlobalSystemMediaTransportControlsSessionPlaybackStatus as Status,
};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};

/// How long `pause_if_playing` may block the caller (spec: must stay under 150 ms).
const PAUSE_BUDGET: Duration = Duration::from_millis(120);

enum Cmd {
    /// Create the session manager ahead of the first dictation.
    Warm,
    Pause { deadline: Instant, reply: Sender<bool> },
    Resume,
}

static WORKER: OnceLock<Option<Mutex<Sender<Cmd>>>> = OnceLock::new();

fn worker() -> Option<&'static Mutex<Sender<Cmd>>> {
    WORKER
        .get_or_init(|| {
            let (tx, rx) = mpsc::channel();
            match std::thread::Builder::new().name("media-ctl".into()).spawn(move || worker_main(rx)) {
                Ok(_) => Some(Mutex::new(tx)),
                Err(e) => {
                    crate::log_warn!("media: cannot start worker thread: {e}");
                    None
                }
            }
        })
        .as_ref()
}

fn send(cmd: Cmd) -> bool {
    let Some(w) = worker() else { return false };
    let tx = w.lock().unwrap_or_else(|p| p.into_inner());
    tx.send(cmd).is_ok()
}

/// Create the WinRT session manager in the background so the first pause is fast.
pub fn warm() {
    send(Cmd::Warm);
}

/// Pause playing media. Returns true if something was paused (and should be resumed).
pub fn pause_if_playing() -> bool {
    let (reply, rx) = mpsc::channel();
    let deadline = Instant::now() + PAUSE_BUDGET;
    if !send(Cmd::Pause { deadline, reply }) {
        return false;
    }
    match rx.recv_timeout(PAUSE_BUDGET) {
        Ok(paused) => paused,
        Err(_) => {
            crate::log_warn!("media: pause did not answer within {} ms; skipped", PAUSE_BUDGET.as_millis());
            false
        }
    }
}

/// Resume what `pause_if_playing` paused. Returns immediately; the worker does the work.
pub fn resume() {
    send(Cmd::Resume);
}

fn worker_main(rx: Receiver<Cmd>) {
    // SAFETY: plain COM init for this thread (MTA so IAsyncOperation::join can block safely).
    let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    let mut manager: Option<Manager> = None;
    let mut paused: Vec<Session> = Vec::new();
    while let Ok(cmd) = rx.recv() {
        match cmd {
            Cmd::Warm => {
                ensure_manager(&mut manager);
            }
            Cmd::Pause { deadline, reply } => {
                let n = pause_playing(&mut manager, &mut paused, deadline);
                let _ = reply.send(n > 0);
            }
            Cmd::Resume => resume_paused(&mut paused),
        }
    }
}

fn ensure_manager(slot: &mut Option<Manager>) -> Option<&Manager> {
    if slot.is_none() {
        match Manager::RequestAsync().and_then(|op| op.join()) {
            Ok(m) => *slot = Some(m),
            Err(e) => crate::log_warn!("media: session manager unavailable: {e}"),
        }
    }
    slot.as_ref()
}

/// The slice of a media session the pause/resume logic needs (a trait so it is unit-testable).
trait Ctl {
    fn status(&self) -> Option<Status>;
    fn can_pause(&self) -> bool;
    /// `Ok(true)` when the app accepted the command.
    fn pause(&self) -> windows::core::Result<bool>;
    fn play(&self) -> windows::core::Result<bool>;
}

impl Ctl for Session {
    fn status(&self) -> Option<Status> {
        self.GetPlaybackInfo().ok()?.PlaybackStatus().ok()
    }
    fn can_pause(&self) -> bool {
        self.GetPlaybackInfo().and_then(|i| i.Controls()).and_then(|c| c.IsPauseEnabled()).unwrap_or(true)
    }
    fn pause(&self) -> windows::core::Result<bool> {
        self.TryPauseAsync()?.join()
    }
    fn play(&self) -> windows::core::Result<bool> {
        self.TryPlayAsync()?.join()
    }
}

/// Pause every session reporting `Playing`; returns how many were paused by this call.
fn pause_playing(manager: &mut Option<Manager>, paused: &mut Vec<Session>, deadline: Instant) -> usize {
    if Instant::now() >= deadline {
        return 0;
    }
    let Some(sessions) = ensure_manager(manager).and_then(|m| m.GetSessions().ok()) else {
        return 0;
    };
    pause_sessions(sessions, paused, deadline)
}

fn pause_sessions<S: Ctl>(sessions: impl IntoIterator<Item = S>, paused: &mut Vec<S>, deadline: Instant) -> usize {
    let mut count = 0;
    for s in sessions {
        // The caller has given up after the deadline; don't pause anything it won't resume.
        if Instant::now() >= deadline {
            break;
        }
        if s.status() != Some(Status::Playing) || !s.can_pause() {
            continue;
        }
        match s.pause() {
            Ok(true) if Instant::now() >= deadline => {
                // The app took too long: the caller already answered "nothing paused" and will
                // never call `resume`, so undo this pause now instead of leaving media stopped.
                crate::log_warn!("media: pause finished after the deadline; resuming it");
                if let Err(e) = s.play() {
                    crate::log_warn!("media: late-pause undo failed: {e}");
                }
            }
            Ok(true) => {
                paused.push(s);
                count += 1;
            }
            Ok(false) => {}
            Err(e) => crate::log_warn!("media: pause failed: {e}"),
        }
    }
    count
}

fn resume_paused<S: Ctl>(paused: &mut Vec<S>) {
    for s in paused.drain(..) {
        // `Paused` is the normal case. `Playing` is allowed too: the status property lags the
        // pause by ~20 ms, so a quick tap would otherwise never resume, and `play` on a session
        // that is already playing (user pressed play) is a no-op. Anything else (stopped,
        // closed, app gone) was changed by the user: leave it alone.
        if !matches!(s.status(), Some(Status::Paused | Status::Playing)) {
            continue;
        }
        if let Err(e) = s.play() {
            crate::log_warn!("media: resume failed: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pause_budget_is_within_spec() {
        assert!(PAUSE_BUDGET < Duration::from_millis(150));
    }

    #[test]
    fn expired_deadline_pauses_nothing() {
        let mut manager = None;
        let mut paused = Vec::new();
        let n = pause_playing(&mut manager, &mut paused, Instant::now() - Duration::from_millis(1));
        assert_eq!(n, 0);
        assert!(paused.is_empty());
        assert!(manager.is_none(), "must not even touch WinRT after the deadline");
    }

    use std::cell::{Cell, RefCell};
    use std::rc::Rc;

    #[derive(Clone)]
    struct Fake {
        name: &'static str,
        status: Rc<Cell<Status>>,
        /// How long `pause` blocks, like a slow media app.
        pause_delay: Duration,
        can_pause: bool,
        log: Rc<RefCell<Vec<String>>>,
    }

    impl Fake {
        fn new(name: &'static str, status: Status, log: &Rc<RefCell<Vec<String>>>) -> Self {
            Self { name, status: Rc::new(Cell::new(status)), pause_delay: Duration::ZERO, can_pause: true, log: log.clone() }
        }
    }

    impl Ctl for Fake {
        fn status(&self) -> Option<Status> {
            Some(self.status.get())
        }
        fn can_pause(&self) -> bool {
            self.can_pause
        }
        fn pause(&self) -> windows::core::Result<bool> {
            std::thread::sleep(self.pause_delay);
            self.log.borrow_mut().push(format!("pause {}", self.name));
            self.status.set(Status::Paused);
            Ok(true)
        }
        fn play(&self) -> windows::core::Result<bool> {
            self.log.borrow_mut().push(format!("play {}", self.name));
            self.status.set(Status::Playing);
            Ok(true)
        }
    }

    fn far() -> Instant {
        Instant::now() + Duration::from_secs(60)
    }

    #[test]
    fn pauses_only_playing_sessions_and_resumes_only_those() {
        let log = Rc::default();
        let a = Fake::new("a", Status::Playing, &log);
        let b = Fake::new("b", Status::Paused, &log);
        let c = Fake::new("c", Status::Stopped, &log);
        let mut d = Fake::new("d", Status::Playing, &log);
        d.can_pause = false;
        let mut paused = Vec::new();
        let n = pause_sessions([a.clone(), b.clone(), c.clone(), d.clone()], &mut paused, far());
        assert_eq!(n, 1);
        assert_eq!(*log.borrow(), ["pause a"]);
        resume_paused(&mut paused);
        assert_eq!(*log.borrow(), ["pause a", "play a"]);
        assert_eq!(b.status.get(), Status::Paused, "never touched");
        assert_eq!(c.status.get(), Status::Stopped);
        assert_eq!(d.status.get(), Status::Playing);
        assert!(paused.is_empty());
    }

    #[test]
    fn resume_skips_sessions_the_user_stopped_or_closed() {
        let log = Rc::default();
        let a = Fake::new("a", Status::Playing, &log);
        let b = Fake::new("b", Status::Playing, &log);
        let mut paused = Vec::new();
        assert_eq!(pause_sessions([a.clone(), b.clone()], &mut paused, far()), 2);
        a.status.set(Status::Stopped); // user stopped it meanwhile
        b.status.set(Status::Closed); // app went away
        resume_paused(&mut paused);
        assert_eq!(*log.borrow(), ["pause a", "pause b"], "must not start stopped media");
    }

    #[test]
    fn resume_works_while_status_is_still_stale_playing() {
        // WinRT keeps reporting Playing for ~20 ms after TryPauseAsync returns.
        let log = Rc::default();
        let a = Fake::new("a", Status::Playing, &log);
        let mut paused = Vec::new();
        assert_eq!(pause_sessions([a.clone()], &mut paused, far()), 1);
        a.status.set(Status::Playing);
        resume_paused(&mut paused);
        assert_eq!(*log.borrow(), ["pause a", "play a"]);
    }

    #[test]
    fn late_pause_is_undone_because_the_caller_will_not_resume() {
        let log = Rc::default();
        let mut slow = Fake::new("slow", Status::Playing, &log);
        slow.pause_delay = Duration::from_millis(60);
        let mut paused = Vec::new();
        let n = pause_sessions([slow.clone()], &mut paused, Instant::now() + Duration::from_millis(20));
        assert_eq!(n, 0, "caller got `false`");
        assert!(paused.is_empty());
        assert_eq!(*log.borrow(), ["pause slow", "play slow"]);
        assert_eq!(slow.status.get(), Status::Playing, "media left playing");
    }

    #[test]
    fn deadline_stops_before_the_next_session() {
        let log = Rc::default();
        let mut slow = Fake::new("slow", Status::Playing, &log);
        slow.pause_delay = Duration::from_millis(60);
        let next = Fake::new("next", Status::Playing, &log);
        let mut paused = Vec::new();
        pause_sessions([slow, next.clone()], &mut paused, Instant::now() + Duration::from_millis(20));
        assert_eq!(next.status.get(), Status::Playing);
        assert!(!log.borrow().iter().any(|l| l == "pause next"));
    }


    fn playing_apps() -> Vec<String> {
        // SAFETY: plain COM init for the test thread.
        let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        let Ok(m) = Manager::RequestAsync().and_then(|op| op.join()) else { return Vec::new() };
        let Ok(list) = m.GetSessions() else { return Vec::new() };
        list.into_iter()
            .filter(|s| Ctl::status(s) == Some(Status::Playing))
            .filter_map(|s| s.SourceAppUserModelId().ok().map(|h| h.to_string()))
            .collect()
    }

    /// Whatever is playing is paused and, even for a quick resume (the status property lags the pause by ~20 ms),
    /// playing again afterwards. Passes trivially when nothing is playing.
    #[test]
    #[ignore = "desktop: pauses and resumes whatever media is playing"]
    fn desktop_media_roundtrip_restores_playback() {
        warm();
        std::thread::sleep(Duration::from_millis(500));
        let before = playing_apps();
        println!("playing before: {before:?}");
        for gap_ms in [60u64, 400] {
            let paused = pause_if_playing();
            assert_eq!(paused, !before.is_empty(), "pause result vs {before:?}");
            std::thread::sleep(Duration::from_millis(gap_ms));
            if paused {
                assert!(playing_apps().iter().all(|a| !before.contains(a)), "still playing after pause");
            }
            resume();
            std::thread::sleep(Duration::from_millis(500));
            assert_eq!(playing_apps(), before, "playback not restored (gap {gap_ms} ms)");
        }
    }

    /// Never asserts that something was paused (nothing may be playing); checks timing and that
    /// resume is safe to call with nothing paused.
    #[test]
    #[ignore = "desktop: talks to the system media transport controls"]
    fn desktop_media_roundtrip_is_fast() {
        warm();
        std::thread::sleep(Duration::from_millis(500));
        let t = Instant::now();
        let paused = pause_if_playing();
        let took = t.elapsed();
        println!("pause_if_playing -> {paused} in {took:?}");
        assert!(took < Duration::from_millis(150), "blocked {took:?}");
        std::thread::sleep(Duration::from_millis(60)); // a real dictation lasts longer than the status lag
        resume();
        std::thread::sleep(Duration::from_millis(300));
        // Nothing left to resume: a second resume is a no-op.
        resume();
    }
}
