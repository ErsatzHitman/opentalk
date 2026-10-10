//! Gemini Live transcription (`BidiGenerateContent`), mirroring geminiLiveStreaming.js: a
//! `setup` message first (audio before `setupComplete` is discarded by the server), 16 kHz PCM
//! as base64 `realtimeInput.audio`, `audioStreamEnd` on stop, `interimInputTranscription`
//! partials that are revised (not appended) and `inputTranscription` finals.
use super::session::{self, ConnectSpec, Protocol};
use super::util;
use super::{DEFAULT_GEMINI_ORIGIN, ws_url};
use crate::core::config::SttProviderId;
use crate::stt::{Credentials, StreamingProvider, StreamingSession, SttError, SttErrorCode, SttOptions};
use base64::Engine;
use serde_json::{Value, json};
use std::time::Duration;
use tungstenite::Message;

const NAME: &str = "Gemini";
const DEFAULT_MODEL: &str = "gemini-3.5-transcribe-live";
const WS_PATH: &str = "/ws/google.ai.generativelanguage.v1beta.GenerativeService.BidiGenerateContent";
/// The final transcript lands ~500 ms after `audioStreamEnd`, ~2 s at the p95 tail.
const CEILING: Duration = Duration::from_millis(3000);
/// Google documents a 1000-phrase ceiling but only promises quality up to 100.
const MAX_VOCABULARY: usize = 100;
/// Trailing room noise measures 0.003-0.010 RMS, speech frames sit above ~0.0105: only a frame
/// at this level is a new utterance rather than the tail of a turn the server already closed.
const TURN_REOPEN_RMS: f32 = 0.012;

pub struct GeminiLive;

impl StreamingProvider for GeminiLive {
    fn id(&self) -> SttProviderId {
        SttProviderId::Gemini
    }

    fn start(&self, opts: &SttOptions, creds: &Credentials) -> Result<Box<dyn StreamingSession>, SttError> {
        let key = creds.api_key.as_deref().map(str::trim).filter(|k| !k.is_empty()).ok_or_else(|| util::not_configured(NAME))?;
        let path = format!("{WS_PATH}?key={}", util::percent_encode(key));
        session::spawn(Box::new(LiveProto {
            url: ws_url(creds, DEFAULT_GEMINI_ORIGIN, &path),
            key: key.to_string(),
            setup: setup_message(&opts.model, util::explicit_language(opts.language.as_deref()), &opts.hints),
            ..LiveProto::default()
        }))
    }
}

/// The first message of a Live session.
pub(crate) fn setup_message(model: &str, language: Option<&str>, hints: &[String]) -> Value {
    // Anything but a Gemini id (a stale setting from another provider) selects the default.
    let model = if model.trim().starts_with("gemini-") { model.trim() } else { DEFAULT_MODEL };
    let mut transcription = json!({});
    // `languageCodes` alone costs punctuation and casing, and an invalid locale is accepted
    // silently, so only an explicitly chosen language is sent.
    if let Some(l) = language {
        transcription["languageCodes"] = json!([l]);
    }
    let vocabulary = util::clean_hints(hints, MAX_VOCABULARY);
    if !vocabulary.is_empty() {
        transcription["customVocabulary"] = json!(vocabulary);
    }
    json!({
        "setup": {
            "model": format!("models/{model}"),
            "generationConfig": { "responseModalities": ["TEXT"] },
            "inputAudioTranscription": transcription
        }
    })
}

#[derive(Default)]
struct LiveProto {
    url: String,
    key: String,
    setup: Value,
    ready: bool,
    segments: Vec<String>,
    /// Latest revision of the turn in progress.
    open_partial: String,
    finalize_sent: bool,
    turn_ended: bool,
    err: Option<SttError>,
}

impl LiveProto {
    /// The partial sometimes restates the finalized turns first, run together without spaces
    /// ("...October.Before then"); only what follows is new.
    fn open_turn(&self) -> String {
        let mut text = self.open_partial.as_str();
        for seg in &self.segments {
            let rest = text.trim_start();
            match rest.strip_prefix(seg.as_str()) {
                Some(after) => text = after,
                None => break,
            }
        }
        text.trim().to_string()
    }
}

impl Protocol for LiveProto {
    fn name(&self) -> &'static str {
        NAME
    }

    fn connect_spec(&mut self) -> Result<ConnectSpec, SttError> {
        Ok(ConnectSpec { url: self.url.clone(), headers: Vec::new() })
    }

    fn on_open(&mut self) -> Vec<Message> {
        vec![Message::text(self.setup.to_string())]
    }

    fn credentials(&self) -> Vec<String> {
        vec![self.key.clone()]
    }

    fn ready(&self) -> bool {
        self.ready
    }

    fn encode(&mut self, frame: &[i16]) -> Message {
        if self.turn_ended && util::rms(frame) >= TURN_REOPEN_RMS {
            self.turn_ended = false;
        }
        let data = base64::engine::general_purpose::STANDARD.encode(util::pcm_to_le_bytes(frame));
        Message::text(json!({ "realtimeInput": { "audio": { "data": data, "mimeType": "audio/pcm;rate=16000" } } }).to_string())
    }

    fn on_message(&mut self, text: &str, _out: &mut Vec<Message>) -> bool {
        let Ok(v) = serde_json::from_str::<Value>(text) else { return false };
        let mut activity = false;
        if v.get("setupComplete").is_some() {
            self.ready = true;
            activity = true;
        }
        let content = &v["serverContent"];
        if !content.is_object() {
            return activity;
        }
        // Partials are revised ("The quick brown" -> "the quick brown fox"): replace, never append.
        if let Some(p) = content["interimInputTranscription"]["text"].as_str().filter(|p| !p.is_empty()) {
            self.turn_ended = false;
            self.open_partial = p.to_string();
            activity = true;
        }
        if let Some(f) = content["inputTranscription"]["text"].as_str().map(str::trim).filter(|f| !f.is_empty()) {
            self.segments.push(f.to_string());
            self.open_partial.clear();
            activity = true;
        }
        // The server closes a turn on trailing silence, often before the hotkey is released;
        // speech (a loud frame or a new partial) reopens it, so a later stop waits again.
        if content["generationComplete"].as_bool() == Some(true) {
            self.turn_ended = true;
            activity = true;
        }
        activity
    }

    fn on_close(&mut self, code: u16, reason: &str) {
        if code == 1000 || code == 1005 || self.err.is_some() {
            return;
        }
        // Every Live failure is a close frame after a successful upgrade. 1007 is a malformed
        // credential, 1008 one Google no longer knows; 1011 is an expired/over-used token or a
        // server fault. The reason is truncated by the server, so classify on the code alone.
        let (kind, msg) = match code {
            1007 | 1008 => (SttErrorCode::AuthFailed, "Gemini rejected your API key."),
            1011 => (SttErrorCode::Unavailable, "Gemini Live is temporarily unavailable."),
            _ => (SttErrorCode::Unreachable, "Gemini closed the connection."),
        };
        self.err = Some(util::err(kind, msg, format!("close {code}: {}", util::scrub(reason, &self.credentials()))));
    }

    fn finalize(&mut self) -> Vec<Message> {
        self.finalize_sent = true;
        vec![Message::text(r#"{"realtimeInput":{"audioStreamEnd":true}}"#)]
    }

    fn done(&self) -> bool {
        self.finalize_sent && self.turn_ended
    }

    fn partial_pending(&self) -> bool {
        !self.open_turn().is_empty()
    }

    fn ceiling(&self) -> Duration {
        CEILING
    }

    /// Finalized turns plus the turn the server never finalized, so nothing since the last
    /// pause is lost.
    fn text(&self) -> String {
        let open = self.open_turn();
        self.segments.iter().map(String::as_str).chain((!open.is_empty()).then_some(open.as_str())).collect::<Vec<_>>().join(" ")
    }

    fn error(&self) -> Option<SttError> {
        self.err.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn setup_shape() {
        let v = setup_message("gemini-3.5-transcribe-live", Some("fr"), &["Rust".into(), "Rust".into(), "Whispr".into()]);
        assert_eq!(v["setup"]["model"], "models/gemini-3.5-transcribe-live");
        assert_eq!(v["setup"]["generationConfig"], json!({ "responseModalities": ["TEXT"] }));
        assert_eq!(v["setup"]["inputAudioTranscription"], json!({ "languageCodes": ["fr"], "customVocabulary": ["Rust", "Whispr"] }));
        // a stale non-Gemini model id falls back to the default, and no language/vocabulary stays empty
        let v = setup_message("gpt-4o-transcribe", None, &[]);
        assert_eq!(v["setup"]["model"], "models/gemini-3.5-transcribe-live");
        assert_eq!(v["setup"]["inputAudioTranscription"], json!({}));
    }

    #[test]
    fn partials_are_replaced_and_open_turn_kept() {
        let mut p = LiveProto::default();
        let mut o = Vec::new();
        assert!(!p.ready());
        p.on_message(r#"{"setupComplete":{}}"#, &mut o);
        assert!(p.ready());
        p.on_message(r#"{"serverContent":{"interimInputTranscription":{"text":"The quick brown"}}}"#, &mut o);
        p.on_message(r#"{"serverContent":{"interimInputTranscription":{"text":"the quick brown fox"}}}"#, &mut o);
        assert_eq!(p.text(), "the quick brown fox");
        assert!(p.partial_pending());
        p.on_message(r#"{"serverContent":{"inputTranscription":{"text":" The quick brown fox. "},"generationComplete":true}}"#, &mut o);
        assert!(!p.partial_pending());
        // the next partial restates the finalized turn glued to the new words
        p.on_message(r#"{"serverContent":{"interimInputTranscription":{"text":"The quick brown fox.Jumps over"}}}"#, &mut o);
        assert_eq!(p.text(), "The quick brown fox. Jumps over");
    }

    #[test]
    fn done_after_generation_complete_unless_speech_reopens() {
        let mut p = LiveProto::default();
        let mut o = Vec::new();
        p.on_message(r#"{"serverContent":{"inputTranscription":{"text":"hi"},"generationComplete":true}}"#, &mut o);
        p.finalize();
        assert!(p.done(), "turn already ended: nothing to wait for");

        let mut p = LiveProto::default();
        p.on_message(r#"{"serverContent":{"inputTranscription":{"text":"hi"},"generationComplete":true}}"#, &mut o);
        p.encode(&[0i16; 800]);
        assert!(p.turn_ended, "quiet frames do not reopen the turn");
        p.encode(&[8000i16; 800]);
        assert!(!p.turn_ended, "a loud frame reopens it");
        p.finalize();
        assert!(!p.done());
        p.on_message(r#"{"serverContent":{"inputTranscription":{"text":"again"},"generationComplete":true}}"#, &mut o);
        assert!(p.done());
        assert_eq!(p.text(), "hi again");
    }

    #[test]
    fn close_codes_classify() {
        let mut p = LiveProto::default();
        p.on_close(1007, "bad key");
        assert_eq!(p.error().unwrap().code, SttErrorCode::AuthFailed);
        let mut p = LiveProto::default();
        p.on_close(1011, "token used");
        assert_eq!(p.error().unwrap().code, SttErrorCode::Unavailable);
    }
}
