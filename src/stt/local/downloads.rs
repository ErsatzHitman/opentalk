//! Background installs of local models, shared by every open Settings page.
//!
//! A download runs on its own thread in the daemon process, so it continues when the tray popup
//! closes. Pages read [`status`] and register a waker to be called (at most ~8 times a second)
//! when something changed. A finished install remembers that the page asked to select it, so the
//! page can switch to the model when it next looks.
use super::catalog::{self, LocalModel};
use super::install::{self, Progress, Stage};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

pub type Waker = Arc<dyn Fn() + Send + Sync>;

#[derive(Clone, Debug, PartialEq)]
pub enum Status {
    /// Fetching the shared runtime (first model only).
    Runtime { done: u64, total: Option<u64> },
    Downloading { done: u64, total: Option<u64> },
    Unpacking,
    /// Installed during this session and waiting to be selected by the page that asked.
    Done,
    Failed(String),
}

impl Status {
    pub fn active(&self) -> bool {
        matches!(self, Status::Runtime { .. } | Status::Downloading { .. } | Status::Unpacking)
    }

    /// "Downloading 42 %", "Preparing the engine…", "Unpacking…".
    pub fn label(&self) -> String {
        let pct = |done: u64, total: Option<u64>| total.filter(|t| *t > 0).map(|t| (done * 100 / t).min(100));
        match self {
            Status::Runtime { .. } => "Preparing the local engine\u{2026}".into(),
            Status::Downloading { done, total } => match pct(*done, *total) {
                Some(p) => format!("Downloading {p} % \u{b7} {} of {}", catalog::size_label((*done / 1_000_000) as u32), catalog::size_label((total.unwrap_or(0) / 1_000_000) as u32)),
                None => format!("Downloading \u{b7} {}", catalog::size_label((*done / 1_000_000) as u32)),
            },
            Status::Unpacking => "Unpacking\u{2026}".into(),
            Status::Done => "Installed".into(),
            Status::Failed(e) => format!("Download failed: {e}"),
        }
    }
}

struct Job {
    status: Status,
    cancel: Arc<AtomicBool>,
}

#[derive(Default)]
struct State {
    jobs: HashMap<&'static str, Job>,
    wakers: Vec<(u64, Waker)>,
    next_waker: u64,
}

static STATE: Mutex<Option<State>> = Mutex::new(None);

fn with<R>(f: impl FnOnce(&mut State) -> R) -> R {
    let mut g = STATE.lock().unwrap_or_else(|p| p.into_inner());
    f(g.get_or_insert_with(State::default))
}

fn wake_all() {
    let wakers: Vec<Waker> = with(|s| s.wakers.iter().map(|w| w.1.clone()).collect());
    for w in wakers {
        w();
    }
}

/// Call `waker` whenever a download changes; returns a token for [`unsubscribe`].
pub fn subscribe(waker: Waker) -> u64 {
    with(|s| {
        s.next_waker += 1;
        let id = s.next_waker;
        s.wakers.push((id, waker));
        id
    })
}

pub fn unsubscribe(token: u64) {
    with(|s| s.wakers.retain(|w| w.0 != token));
}

/// What is happening to model `id` (None = nothing in progress or pending).
pub fn status(id: &str) -> Option<Status> {
    with(|s| s.jobs.get(id).map(|j| j.status.clone()))
}

/// Is any install running?
pub fn any_active() -> bool {
    with(|s| s.jobs.values().any(|j| j.status.active()))
}

/// Forget a finished or failed job (the page consumed it).
pub fn clear(id: &str) {
    with(|s| {
        if s.jobs.get(id).is_some_and(|j| !j.status.active()) {
            s.jobs.remove(id);
        }
    });
}

/// Ask a running install to stop; its files are removed.
pub fn cancel(id: &str) {
    with(|s| {
        if let Some(j) = s.jobs.get(id) {
            j.cancel.store(true, Ordering::Relaxed);
        }
    });
}

fn set(id: &'static str, status: Status) {
    with(|s| {
        if let Some(j) = s.jobs.get_mut(id) {
            j.status = status;
        }
    });
}

/// Start installing `m` (runtime first if needed). False when it is already running.
pub fn start(m: &'static LocalModel) -> bool {
    let cancel = Arc::new(AtomicBool::new(false));
    let fresh = with(|s| {
        if s.jobs.get(m.id).is_some_and(|j| j.status.active()) {
            return false;
        }
        s.jobs.insert(m.id, Job { status: Status::Downloading { done: 0, total: None }, cancel: cancel.clone() });
        true
    });
    if !fresh {
        return false;
    }
    wake_all();
    let spawned = std::thread::Builder::new().name(format!("install-{}", m.id)).spawn(move || run(m, &cancel));
    if spawned.is_err() {
        set(m.id, Status::Failed("could not start the download".into()));
        wake_all();
    }
    true
}

fn run(m: &'static LocalModel, cancel: &AtomicBool) {
    let last = Mutex::new(Instant::now() - std::time::Duration::from_secs(1));
    let on = |stage: Stage, done: u64, total: Option<u64>| {
        set(
            m.id,
            match stage {
                Stage::Runtime => Status::Runtime { done, total },
                Stage::Download => Status::Downloading { done, total },
                Stage::Unpack => Status::Unpacking,
            },
        );
        let mut l = last.lock().unwrap_or_else(|p| p.into_inner());
        if l.elapsed().as_millis() >= 125 || stage == Stage::Unpack {
            *l = Instant::now();
            drop(l);
            wake_all();
        }
    };
    let p = Progress { on: &on, cancel };
    let started = Instant::now();
    let result = install::install_runtime(&p).and_then(|()| install::install_model(m, &p));
    match result {
        Ok(_) => {
            crate::log_info!("installed local model {} in {:.0} s", m.id, started.elapsed().as_secs_f32());
            set(m.id, Status::Done);
        }
        Err(e) if e == install::CANCELLED => {
            crate::log_info!("install of {} cancelled", m.id);
            with(|s| s.jobs.remove(m.id));
        }
        Err(e) => {
            crate::log_warn!("install of {} failed: {e}", m.id);
            set(m.id, Status::Failed(e));
        }
    }
    wake_all();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels() {
        assert_eq!(Status::Downloading { done: 210_000_000, total: Some(465_000_000) }.label(), "Downloading 45 % \u{b7} 210 MB of 465 MB");
        assert_eq!(Status::Downloading { done: 5_000_000, total: None }.label(), "Downloading \u{b7} 5 MB");
        assert_eq!(Status::Unpacking.label(), "Unpacking\u{2026}");
        assert!(Status::Runtime { done: 0, total: None }.active());
        assert!(!Status::Done.active() && !Status::Failed("x".into()).active());
    }

    #[test]
    fn subscribers_are_woken_and_can_leave() {
        let hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let h = hits.clone();
        let token = subscribe(Arc::new(move || {
            h.fetch_add(1, Ordering::Relaxed);
        }));
        wake_all();
        assert!(hits.load(Ordering::Relaxed) >= 1);
        unsubscribe(token);
        let before = hits.load(Ordering::Relaxed);
        wake_all();
        assert_eq!(hits.load(Ordering::Relaxed), before);
    }
}
