//! Local speech-to-text: models that run on this PC with the sherpa-onnx runtime, no API key, and
//! no audio leaving the machine. `stt::transcribe`, `stt::start_stream` and `stt::prewarm` route
//! here when `cfg.stt.engine` is `Local`.
pub mod catalog;
pub mod downloads;
pub mod install;
pub mod pe;
pub mod segment;
pub mod server;

use crate::core::config::{Config, SttEngine};
use crate::stt::{Audio, SttError, SttErrorCode, StreamingSession, Transcript};
use catalog::LocalModel;
use install::Manifest;

pub fn is_local(cfg: &Config) -> bool {
    cfg.stt.engine == SttEngine::Local
}

/// The chosen model and its files, or why there is none to use.
pub fn selected(cfg: &Config) -> Result<(&'static LocalModel, Manifest), SttError> {
    let id = cfg.stt.local_model.trim();
    let Some(m) = catalog::get(id) else {
        return Err(SttError::new(SttErrorCode::NotConfigured, "Choose a local model in Settings > Speech-to-Text.").with_detail(format!("local model {id:?}")));
    };
    match install::installed(m.id) {
        Some(manifest) => Ok((m, manifest)),
        None if downloads::status(m.id).is_some_and(|s| s.active()) => {
            Err(SttError::new(SttErrorCode::NotConfigured, format!("{} is still downloading.", m.name)).with_detail("downloading"))
        }
        None => Err(SttError::new(SttErrorCode::NotConfigured, format!("{} is not downloaded. Download it in Settings > Speech-to-Text.", m.name)).with_detail("not installed")),
    }
}

/// "Parakeet TDT 0.6B v3 (local)": the provider name history and logs show.
pub fn display_name(cfg: &Config) -> String {
    catalog::get(&cfg.stt.local_model).map_or_else(|| "Local model".to_string(), |m| format!("{} (local)", m.name))
}

fn join(parts: Vec<String>) -> String {
    parts.into_iter().map(|p| p.trim().to_string()).filter(|p| !p.is_empty()).collect::<Vec<_>>().join(" ")
}

/// Transcribe a finished recording with the local model.
pub fn transcribe(cfg: &Config, audio: &Audio) -> Result<Transcript, SttError> {
    let (m, manifest) = selected(cfg)?;
    transcribe_with(m, &manifest, cfg, audio)
}

fn transcribe_with(m: &LocalModel, manifest: &Manifest, cfg: &Config, audio: &Audio) -> Result<Transcript, SttError> {
    if audio.samples.is_empty() {
        return Err(crate::stt::errors::no_speech());
    }
    let started = crate::core::timing::now_ms();
    let port = server::ensure(m, manifest, &cfg.language)?;
    let text = if m.kind.streams() {
        // the streaming model takes a finished recording as one stream
        let mut s: Box<dyn StreamingSession> = Box::new(server::OnlineSession::open(port)?);
        for chunk in audio.samples.chunks(3200) {
            s.push(chunk);
        }
        s.finish()?.text
    } else {
        let mut parts = Vec::new();
        for r in segment::split(&audio.samples, audio.sample_rate, m.kind.max_segment_s()) {
            parts.push(server::decode_offline(port, &audio.samples[r], audio.sample_rate)?);
        }
        join(parts)
    };
    crate::log_debug!("stt local {} -> {} chars in {:.0} ms", m.id, text.len(), crate::core::timing::now_ms() - started);
    let text = text.trim();
    if text.is_empty() {
        return Err(crate::stt::errors::no_speech());
    }
    Ok(Transcript { text: text.to_string(), detected_language: None })
}

/// A live session for the streaming model (None: the model transcribes after the recording).
pub fn start_stream(cfg: &Config) -> Option<Result<Box<dyn StreamingSession>, SttError>> {
    let (m, manifest) = match selected(cfg) {
        Ok(v) => v,
        Err(e) => return Some(Err(e)),
    };
    if !m.kind.streams() {
        return None;
    }
    Some(server::ensure(m, &manifest, &cfg.language).and_then(server::OnlineSession::open).map(|s| Box::new(s) as Box<dyn StreamingSession>))
}

/// Load the chosen model in the background (at start-up, after a settings change and at the
/// hotkey), so the dictation does not wait for it. Stops the server when the engine is not local.
pub fn prewarm(cfg: &Config) {
    if !is_local(cfg) {
        if server::running().is_some() {
            std::thread::spawn(server::stop);
        }
        return;
    }
    let Ok((m, manifest)) = selected(cfg) else { return };
    let want = server::Key { model: m.id.to_string(), language: server::whisper_language(m, &cfg.language) };
    if server::running().as_ref() == Some(&want) {
        return;
    }
    let language = cfg.language.clone();
    let _ = std::thread::Builder::new().name("local-stt-load".into()).spawn(move || {
        if let Err(e) = server::ensure(m, &manifest, &language) {
            crate::log_warn!("could not load the local model {}: {} ({})", m.id, e.message, e.detail);
        }
    });
}

/// App exit: stop the model server.
pub fn shutdown() {
    server::stop();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selection_errors_say_what_to_do() {
        let mut cfg = Config::default();
        cfg.stt.engine = SttEngine::Local;
        let e = selected(&cfg).unwrap_err();
        assert_eq!(e.code, SttErrorCode::NotConfigured);
        assert!(e.message.contains("Choose a local model"), "{}", e.message);
        cfg.stt.local_model = "moonshine-tiny".into();
        let e = transcribe(&cfg, &Audio { samples: vec![0; 1600], sample_rate: 16_000 }).unwrap_err();
        assert!(e.message.contains("Moonshine Tiny is not downloaded"), "{}", e.message);
        assert_eq!(display_name(&cfg), "Moonshine Tiny (local)");
        assert!(matches!(start_stream(&cfg), Some(Err(_))), "a missing model is an error, not a silent batch fallback");
    }

    /// The whole engine on a real Windows runtime: download (from a local mirror), hash check,
    /// unpack, import patch, model install, server start, the offline protocol, splitting a long
    /// dictation and joining the pieces. `OPENTALK_TEST_LOCAL_MIRROR` is an http:// folder with the
    /// two runtime packages and `ot-test-ctc.tar.bz2` (a synthetic CTC model that spells "hello
    /// world" for any audio, built by `tools/make_test_ctc.py`); `OPENTALK_TAR` may point at a tar
    /// when Windows' own is missing (Wine).
    #[test]
    #[ignore = "needs OPENTALK_TEST_LOCAL_MIRROR (see the doc comment)"]
    fn local_engine_end_to_end() {
        let Some(mirror) = std::env::var("OPENTALK_TEST_LOCAL_MIRROR").ok() else { return };
        let root = std::env::temp_dir().join(format!("ot-local-e2e-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        // SAFETY: an ignored test run on its own.
        unsafe {
            std::env::set_var("OPENTALK_LOCAL_DIR", &root);
            std::env::set_var("OPENTALK_LOCAL_MIRROR", &mirror);
        }
        let cancel = std::sync::atomic::AtomicBool::new(false);
        let seen = std::sync::Mutex::new(Vec::new());
        let on = |s: install::Stage, d: u64, t: Option<u64>| seen.lock().unwrap().push((s, d, t));
        let p = install::Progress { on: &on, cancel: &cancel };
        install::install_runtime(&p).expect("runtime installs");
        assert!(install::runtime_ready());
        let exe = std::fs::read(install::runtime_dir().join(install::OFFLINE_SERVER)).unwrap();
        let imports = pe::imports(&exe).unwrap();
        assert!(imports.iter().any(|n| n == install::ORT_PRIVATE_NAME) && !imports.iter().any(|n| n.eq_ignore_ascii_case("onnxruntime.dll")), "{imports:?}");
        let m: &'static LocalModel = &catalog::TEST_MODEL;
        let manifest = install::install_model(m, &p).expect("model installs");
        assert_eq!(install::installed(m.id), Some(manifest.clone()));
        assert!(seen.lock().unwrap().iter().any(|e| e.0 == install::Stage::Download), "progress was reported");
        // again through the background manager the Settings page uses
        install::remove(m.id).unwrap();
        let woken = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let w = woken.clone();
        let token = downloads::subscribe(std::sync::Arc::new(move || {
            w.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }));
        assert!(downloads::start(m));
        let t0 = std::time::Instant::now();
        while downloads::status(m.id).is_some_and(|s| s.active()) && t0.elapsed().as_secs() < 60 {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert_eq!(downloads::status(m.id), Some(downloads::Status::Done));
        assert!(woken.load(std::sync::atomic::Ordering::Relaxed) >= 2, "the page was woken");
        downloads::unsubscribe(token);
        downloads::clear(m.id);
        let manifest = install::installed(m.id).expect("installed by the manager");

        let cfg = Config::default();
        let tone = |secs: usize| Audio { samples: (0..16_000 * secs).map(|i| ((i as f32 * 0.07).sin() * 3000.0) as i16).collect(), sample_rate: 16_000 };
        let t = transcribe_with(m, &manifest, &cfg, &tone(2)).expect("transcribes");
        assert_eq!(t.text, "hello world");
        // 50 s is split in three pieces (20 s at most each): one "hello world" per piece
        let t = transcribe_with(m, &manifest, &cfg, &tone(50)).expect("transcribes a long dictation");
        assert_eq!(t.text, "hello world hello world hello world");
        let first = server::running().expect("the server stays warm");
        transcribe_with(m, &manifest, &cfg, &tone(1)).unwrap();
        assert_eq!(server::running(), Some(first), "and is reused");
        server::stop();
        assert_eq!(server::running(), None);
        install::remove(m.id).unwrap();
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn pieces_are_joined_with_single_spaces() {
        assert_eq!(join(vec![" hello ".into(), String::new(), "world.".into()]), "hello world.");
    }
}
