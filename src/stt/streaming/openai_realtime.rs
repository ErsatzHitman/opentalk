//! OpenAI Realtime transcription (`wss://api.openai.com/v1/realtime?intent=transcription`),
//! mirroring openaiRealtimeStreaming.js: after `session.created` the client sends
//! `session.update` (transcription model + server VAD, 24 kHz PCM), audio goes out as
//! base64 `input_audio_buffer.append` events (16 kHz capture is linearly upsampled), the stop
//! sends `input_audio_buffer.commit`, and `conversation.item.input_audio_transcription.*`
//! events carry the text.
use super::session::{self, ConnectSpec, Protocol};
use super::util;
use super::{DEFAULT_OPENAI_ORIGIN, ws_url};
use crate::core::config::SttProviderId;
use crate::stt::{Credentials, StreamingProvider, StreamingSession, SttError, SttErrorCode, SttOptions};
use base64::Engine;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use tungstenite::Message;

const NAME: &str = "OpenAI";
const DEFAULT_MODEL: &str = "gpt-4o-mini-transcribe";
/// Sample rate declared to the server (OpenAI rejects PCM session rates below 24 kHz).
const SESSION_RATE: u32 = 24_000;
/// Raised from the API default so mic ambient noise does not open turns (OpenWhispr #630).
const VAD_THRESHOLD: f64 = 0.6;

pub struct OpenAiRealtime;

impl StreamingProvider for OpenAiRealtime {
    fn id(&self) -> SttProviderId {
        SttProviderId::Openai
    }

    fn start(&self, opts: &SttOptions, creds: &Credentials) -> Result<Box<dyn StreamingSession>, SttError> {
        let key = creds.api_key.as_deref().map(str::trim).filter(|k| !k.is_empty()).ok_or_else(|| util::not_configured(NAME))?;
        let model = if opts.model.trim().is_empty() { DEFAULT_MODEL } else { opts.model.trim() };
        session::spawn(Box::new(RealtimeProto {
            url: ws_url(creds, DEFAULT_OPENAI_ORIGIN, "/v1/realtime?intent=transcription"),
            key: key.to_string(),
            session_update: session_update(model, util::explicit_language(opts.language.as_deref())),
            ..RealtimeProto::default()
        }))
    }
}

/// The `session.update` event configuring a transcription-only session.
pub(crate) fn session_update(model: &str, language: Option<&str>) -> Value {
    let mut transcription = json!({ "model": model });
    if let Some(l) = language {
        transcription["language"] = json!(util::base_language(l));
    }
    json!({
        "type": "session.update",
        "session": {
            "type": "transcription",
            "audio": { "input": {
                "format": { "type": "audio/pcm", "rate": SESSION_RATE },
                "transcription": transcription,
                "turn_detection": {
                    "type": "server_vad",
                    "threshold": VAD_THRESHOLD,
                    "silence_duration_ms": 600,
                    "prefix_padding_ms": 500
                }
            } }
        }
    })
}

#[derive(Default)]
struct RealtimeProto {
    url: String,
    key: String,
    session_update: Value,
    update_sent: bool,
    ready: bool,
    /// Item ids in the order the server committed them (final text follows this order).
    order: Vec<String>,
    texts: HashMap<String, String>,
    partials: HashMap<String, String>,
    /// Committed items whose transcription has not completed or failed yet.
    pending: HashSet<String>,
    finalize_sent: bool,
    commit_resolved: bool,
    err: Option<SttError>,
}

impl RealtimeProto {
    fn note_item(&mut self, id: &str) {
        if !self.order.iter().any(|i| i == id) {
            self.order.push(id.to_string());
        }
    }
}

impl Protocol for RealtimeProto {
    fn name(&self) -> &'static str {
        NAME
    }

    fn connect_spec(&mut self) -> Result<ConnectSpec, SttError> {
        Ok(ConnectSpec { url: self.url.clone(), headers: vec![("Authorization".into(), format!("Bearer {}", self.key))] })
    }

    fn ready(&self) -> bool {
        self.ready
    }

    fn encode(&mut self, frame: &[i16]) -> Message {
        let bytes = util::pcm_to_le_bytes(&util::upsample_16k_to_24k(frame));
        let audio = base64::engine::general_purpose::STANDARD.encode(bytes);
        Message::text(json!({ "type": "input_audio_buffer.append", "audio": audio }).to_string())
    }

    fn on_message(&mut self, text: &str, out: &mut Vec<Message>) -> bool {
        let Ok(v) = serde_json::from_str::<Value>(text) else { return false };
        let item = v["item_id"].as_str().unwrap_or("").to_string();
        match v["type"].as_str() {
            Some("session.created") => {
                if !self.update_sent {
                    self.update_sent = true;
                    out.push(Message::text(self.session_update.to_string()));
                }
                false
            }
            Some("session.updated") => {
                self.ready = true;
                true
            }
            Some("input_audio_buffer.committed") => {
                if !item.is_empty() {
                    self.note_item(&item);
                    self.pending.insert(item);
                }
                if self.finalize_sent {
                    self.commit_resolved = true;
                }
                true
            }
            Some("conversation.item.input_audio_transcription.delta") => {
                let delta = v["delta"].as_str().unwrap_or("");
                if delta.is_empty() {
                    return false;
                }
                self.note_item(&item);
                self.partials.entry(item).or_default().push_str(delta);
                true
            }
            Some("conversation.item.input_audio_transcription.completed") => {
                let transcript = v["transcript"].as_str().unwrap_or("").trim();
                self.note_item(&item);
                self.partials.remove(&item);
                self.pending.remove(&item);
                if !transcript.is_empty() {
                    self.texts.insert(item, transcript.to_string());
                }
                true
            }
            Some("conversation.item.input_audio_transcription.failed") => {
                self.partials.remove(&item);
                self.pending.remove(&item);
                true
            }
            Some("error") => {
                let code = v["error"]["code"].as_str().unwrap_or("");
                let msg = v["error"]["message"].as_str().unwrap_or("OpenAI error");
                let empty_commit = code == "input_audio_buffer_commit_empty" || msg.contains("buffer too small") || msg.contains("commit_empty");
                if empty_commit {
                    // Server VAD already committed everything: nothing left to wait for but items in flight.
                    if self.finalize_sent {
                        self.commit_resolved = true;
                    }
                    return true;
                }
                self.err = Some(util::err(SttErrorCode::Other, "OpenAI Realtime reported an error.", util::scrub(msg, &self.key)));
                if self.finalize_sent {
                    self.commit_resolved = true;
                }
                true
            }
            _ => false,
        }
    }

    fn on_close(&mut self, code: u16, reason: &str) {
        if code != 1000 && code != 1005 && self.err.is_none() {
            self.err = Some(util::err(
                SttErrorCode::Unreachable,
                "OpenAI closed the connection.",
                format!("close {code}: {}", util::scrub(reason, &self.key)),
            ));
        }
    }

    fn finalize(&mut self) -> Vec<Message> {
        self.finalize_sent = true;
        vec![Message::text(r#"{"type":"input_audio_buffer.commit"}"#)]
    }

    fn done(&self) -> bool {
        self.finalize_sent && self.commit_resolved && self.pending.is_empty()
    }

    fn partial_pending(&self) -> bool {
        !self.partials.is_empty() || !self.pending.is_empty()
    }

    fn text(&self) -> String {
        // Per item: its final text, else what has streamed so far, so an item still in flight
        // at the ceiling does not lose its words just because an earlier item completed.
        self.order
            .iter()
            .filter_map(|id| self.texts.get(id).map(String::as_str).or_else(|| self.partials.get(id).map(|s| s.trim())))
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn error(&self) -> Option<SttError> {
        self.err.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_update_shape() {
        let v = session_update("gpt-4o-transcribe", Some("de-DE"));
        assert_eq!(v["type"], "session.update");
        assert_eq!(v["session"]["type"], "transcription");
        let input = &v["session"]["audio"]["input"];
        assert_eq!(input["format"], json!({ "type": "audio/pcm", "rate": 24000 }));
        assert_eq!(input["transcription"], json!({ "model": "gpt-4o-transcribe", "language": "de" }));
        assert_eq!(input["turn_detection"]["type"], "server_vad");
        assert!(session_update("m", None)["session"]["audio"]["input"]["transcription"].get("language").is_none());
    }

    #[test]
    fn item_events_assemble_in_commit_order() {
        let mut p = RealtimeProto { session_update: session_update("m", None), ..RealtimeProto::default() };
        let mut out = Vec::new();
        p.on_message(r#"{"type":"session.created"}"#, &mut out);
        assert_eq!(out.len(), 1);
        p.on_message(r#"{"type":"session.created"}"#, &mut out);
        assert_eq!(out.len(), 1, "session.update is sent once");
        assert!(!p.ready());
        p.on_message(r#"{"type":"session.updated"}"#, &mut out);
        assert!(p.ready());

        p.on_message(r#"{"type":"input_audio_buffer.committed","item_id":"a"}"#, &mut out);
        p.on_message(r#"{"type":"input_audio_buffer.committed","item_id":"b"}"#, &mut out);
        p.on_message(r#"{"type":"conversation.item.input_audio_transcription.delta","item_id":"b","delta":"seco"}"#, &mut out);
        assert!(p.partial_pending());
        // b completes before a: text still follows commit order
        p.on_message(r#"{"type":"conversation.item.input_audio_transcription.completed","item_id":"b","transcript":" second "}"#, &mut out);
        p.on_message(r#"{"type":"conversation.item.input_audio_transcription.completed","item_id":"a","transcript":"first"}"#, &mut out);
        assert_eq!(p.text(), "first second");
        assert!(!p.partial_pending());
    }

    #[test]
    fn done_needs_commit_ack_and_empty_pending() {
        let mut p = RealtimeProto::default();
        let mut out = Vec::new();
        p.on_message(r#"{"type":"input_audio_buffer.committed","item_id":"a"}"#, &mut out);
        p.finalize();
        assert!(!p.done());
        p.on_message(r#"{"type":"error","error":{"code":"input_audio_buffer_commit_empty","message":"x"}}"#, &mut out);
        assert!(!p.done(), "item a is still being transcribed");
        assert!(p.error().is_none(), "an empty commit is not a failure");
        p.on_message(r#"{"type":"conversation.item.input_audio_transcription.failed","item_id":"a"}"#, &mut out);
        assert!(p.done());
    }

    #[test]
    fn partial_is_the_fallback() {
        let mut p = RealtimeProto::default();
        let mut out = Vec::new();
        p.on_message(r#"{"type":"conversation.item.input_audio_transcription.delta","item_id":"a","delta":"hel"}"#, &mut out);
        p.on_message(r#"{"type":"conversation.item.input_audio_transcription.delta","item_id":"a","delta":"lo"}"#, &mut out);
        assert_eq!(p.text(), "hello");
    }

    #[test]
    fn unfinished_item_keeps_its_partial_after_earlier_finals() {
        let mut p = RealtimeProto::default();
        let mut out = Vec::new();
        p.on_message(r#"{"type":"input_audio_buffer.committed","item_id":"a"}"#, &mut out);
        p.on_message(r#"{"type":"input_audio_buffer.committed","item_id":"b"}"#, &mut out);
        p.on_message(r#"{"type":"conversation.item.input_audio_transcription.completed","item_id":"a","transcript":"first part"}"#, &mut out);
        p.on_message(r#"{"type":"conversation.item.input_audio_transcription.delta","item_id":"b","delta":"second"}"#, &mut out);
        p.on_message(r#"{"type":"conversation.item.input_audio_transcription.delta","item_id":"b","delta":" part"}"#, &mut out);
        assert_eq!(p.text(), "first part second part");
    }
}
