//! Live privacy policy shared with workers. Revocation and writes use the same lock, so once
//! a setting change or clear completes, an older job cannot save data using its stale config.
use crate::core::{config::Config, history::{self, Entry}};
use std::sync::{Arc, Mutex};

pub(super) struct Privacy {
    history: bool,
    failed_audio: bool,
    history_epoch: u64,
    audio_epoch: u64,
}

#[derive(Clone)]
pub(super) struct Ticket {
    policy: Arc<Mutex<Privacy>>,
    history_epoch: u64,
    audio_epoch: u64,
    history: bool,
    failed_audio: bool,
}

impl Privacy {
    pub fn new(cfg: &Config) -> Arc<Mutex<Self>> {
        Arc::new(Mutex::new(Self { history: cfg.history_enabled, failed_audio: cfg.keep_failed_audio, history_epoch: 0, audio_epoch: 0 }))
    }

    pub fn update(&mut self, cfg: &Config) {
        if self.history && !cfg.history_enabled { self.history_epoch += 1; }
        if self.failed_audio && !cfg.keep_failed_audio { self.audio_epoch += 1; }
        self.history = cfg.history_enabled;
        self.failed_audio = cfg.keep_failed_audio;
    }

    pub fn clear_history(&mut self) -> std::io::Result<()> {
        self.clear_with(history::clear)
    }

    fn clear_with(&mut self, clear: impl FnOnce() -> std::io::Result<()>) -> std::io::Result<()> {
        self.history_epoch += 1;
        clear()
    }

    pub fn ticket(policy: &Arc<Mutex<Self>>) -> Ticket {
        let p = policy.lock().unwrap_or_else(|e| e.into_inner());
        Ticket { policy: policy.clone(), history_epoch: p.history_epoch, audio_epoch: p.audio_epoch, history: p.history, failed_audio: p.failed_audio }
    }
}

impl Ticket {
    fn write_with(&self, write: impl FnOnce() -> std::io::Result<()>) -> std::io::Result<()> {
        let p = self.policy.lock().unwrap_or_else(|e| e.into_inner());
        if self.history && p.history && self.history_epoch == p.history_epoch { write()?; }
        Ok(())
    }

    pub fn append(&self, entry: Entry) -> std::io::Result<()> {
        self.write_with(|| history::append_new(entry).map(|_| ()))
    }

    pub fn keeps_audio(&self) -> bool {
        let p = self.policy.lock().unwrap_or_else(|e| e.into_inner());
        self.failed_audio && p.failed_audio && self.audio_epoch == p.audio_epoch
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[test]
    fn revocation_invalidates_old_jobs_even_after_reenabling() {
        let mut cfg = Config::default();
        let policy = Privacy::new(&cfg);
        let old = Privacy::ticket(&policy);
        cfg.history_enabled = false;
        cfg.keep_failed_audio = false;
        policy.lock().unwrap().update(&cfg);
        cfg.history_enabled = true;
        cfg.keep_failed_audio = true;
        policy.lock().unwrap().update(&cfg);
        let wrote = AtomicBool::new(false);
        old.write_with(|| { wrote.store(true, Ordering::SeqCst); Ok(()) }).unwrap();
        assert!(!wrote.load(Ordering::SeqCst));
        assert!(!old.keeps_audio());
        let fresh = Privacy::ticket(&policy);
        fresh.write_with(|| { wrote.store(true, Ordering::SeqCst); Ok(()) }).unwrap();
        assert!(wrote.load(Ordering::SeqCst));
        assert!(fresh.keeps_audio());
    }

    #[test]
    fn clear_epoch_prevents_inflight_jobs_from_repopulating_history() {
        let policy = Privacy::new(&Config::default());
        let old = Privacy::ticket(&policy);
        policy.lock().unwrap().clear_with(|| Ok(())).unwrap();
        old.write_with(|| panic!("old job must not write after a clear")).unwrap();
        assert!(old.keeps_audio(), "history clearing does not alter the independent audio setting");
    }

    #[test]
    fn disabling_waits_for_a_write_that_already_started() {
        let policy = Privacy::new(&Config::default());
        let ticket = Privacy::ticket(&policy);
        let (started, ready) = std::sync::mpsc::channel();
        let (release, wait) = std::sync::mpsc::channel();
        let writer = std::thread::spawn(move || ticket.write_with(|| { started.send(()).unwrap(); wait.recv().unwrap(); Ok(()) }));
        ready.recv().unwrap();
        assert!(policy.try_lock().is_err(), "revocation cannot acknowledge while an append is active");
        release.send(()).unwrap();
        writer.join().unwrap().unwrap();
        let cfg = Config { history_enabled: false, ..Config::default() };
        policy.lock().unwrap().update(&cfg);
        Privacy::ticket(&policy).write_with(|| panic!("history disabled")).unwrap();
    }
}
