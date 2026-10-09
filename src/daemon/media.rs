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

fn status_of(s: &Session) -> Option<Status> {
    s.GetPlaybackInfo().ok()?.PlaybackStatus().ok()
}

/// Pause every session reporting `Playing`; returns how many were paused by this call.
fn pause_playing(manager: &mut Option<Manager>, paused: &mut Vec<Session>, deadline: Instant) -> usize {
    if Instant::now() >= deadline {
        return 0;
    }
    let Some(sessions) = ensure_manager(manager).and_then(|m| m.GetSessions().ok()) else {
        return 0;
    };
    let mut count = 0;
    for s in sessions {
        // The caller has given up after the deadline; don't pause anything it won't resume.
        if Instant::now() >= deadline {
            break;
        }
        if status_of(&s) != Some(Status::Playing) {
            continue;
        }
        let can_pause = s.GetPlaybackInfo().and_then(|i| i.Controls()).and_then(|c| c.IsPauseEnabled()).unwrap_or(true);
        if !can_pause {
            continue;
        }
        match s.TryPauseAsync().and_then(|op| op.join()) {
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

fn resume_paused(paused: &mut Vec<Session>) {
    for s in paused.drain(..) {
        // Only sessions still paused: if the user pressed play (or closed the app) meanwhile,
        // leave them alone.
        if status_of(&s) != Some(Status::Paused) {
            continue;
        }
        if let Err(e) = s.TryPlayAsync().and_then(|op| op.join()) {
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
        resume();
        std::thread::sleep(Duration::from_millis(300));
        // Nothing left to resume: a second resume is a no-op.
        resume();
    }
}
