//! AssemblyAI Universal Streaming v3 (`wss://streaming.assemblyai.com/v3/ws`), mirroring
//! assemblyAiStreaming.js and realtimeTokenProviders.js: a 60 s temporary token is minted
//! with the API key (`GET /v3/token`), audio goes out as binary 50 ms frames, `Turn` messages
//! carry partials and (raw, then formatted) end-of-turn finals, `ForceEndpoint` flushes on
//! stop and `Terminate` ends the session.
use super::session::{self, ConnectSpec, FRAME_SAMPLES, Protocol};
use super::util;
use super::{DEFAULT_ASSEMBLYAI_ORIGIN, http_url, ws_url};
use crate::core::config::SttProviderId;
use crate::core::http;
use crate::stt::{Credentials, StreamingProvider, StreamingSession, SttError, SttErrorCode, SttOptions};
use serde_json::Value;
use std::time::Duration;
use tungstenite::Message;

const NAME: &str = "AssemblyAI";
const TOKEN_TIMEOUT: Duration = Duration::from_secs(8);
const MAX_KEYTERMS: usize = 100;

pub struct AssemblyAi;

impl StreamingProvider for AssemblyAi {
    fn id(&self) -> SttProviderId {
        SttProviderId::Assemblyai
    }

    fn start(&self, opts: &SttOptions, creds: &Credentials) -> Result<Box<dyn StreamingSession>, SttError> {
        let key = creds.api_key.as_deref().map(str::trim).filter(|k| !k.is_empty()).ok_or_else(|| util::not_configured(NAME))?;
        session::spawn(Box::new(AssemblyProto {
            key: key.to_string(),
            token_url: http_url(creds, DEFAULT_ASSEMBLYAI_ORIGIN, "/v3/token?expires_in_seconds=60"),
            ws_base: ws_url(creds, DEFAULT_ASSEMBLYAI_ORIGIN, "/v3/ws"),
            model: opts.model.trim().to_string(),
            keyterms: util::clean_hints(&opts.hints, MAX_KEYTERMS),
            ..AssemblyProto::default()
        }))
    }
}

/// Query string of the v3 socket (everything after `/v3/ws?`).
fn ws_query(token: &str, model: &str, keyterms: &[String]) -> String {
    let mut q = format!("sample_rate=16000&encoding=pcm_s16le&format_turns=true&token={}", util::percent_encode(token));
    if !model.is_empty() {
        q.push_str("&speech_model=");
        q.push_str(&util::percent_encode(model));
    }
    if !keyterms.is_empty() {
        let json = serde_json::to_string(keyterms).unwrap_or_else(|_| "[]".into());
        q.push_str("&keyterms_prompt=");
        q.push_str(&util::percent_encode(&json));
    }
    q
}

struct Turn {
    order: Option<u64>,
    normalized: String,
    text: String,
}

#[derive(Default)]
struct AssemblyProto {
    key: String,
    token_url: String,
    ws_base: String,
    model: String,
    keyterms: Vec<String>,
    began: bool,
    turns: Vec<Turn>,
    partial: String,
    finalize_sent: bool,
    flushed: bool,
    terminated: bool,
    err: Option<SttError>,
}

fn normalize(text: &str) -> String {
    let mapped: String = text.to_lowercase().chars().map(|c| if c.is_alphanumeric() || c.is_whitespace() { c } else { ' ' }).collect();
    mapped.split_whitespace().collect::<Vec<_>>().join(" ")
}

impl AssemblyProto {
    fn mint_token(&self) -> Result<String, SttError> {
        let resp = http::get(&self.token_url, &[("Authorization", self.key.as_str())], TOKEN_TIMEOUT).map_err(|e| match e {
            http::HttpError::Timeout => util::err(SttErrorCode::Timeout, "AssemblyAI did not answer the token request.", ""),
            http::HttpError::Network(m) => util::err(SttErrorCode::Unreachable, "Could not reach AssemblyAI.", util::scrub(&m, &self.key)),
        })?;
        if !resp.ok() {
            let detail = resp.json().ok().and_then(|v| v["error"].as_str().map(String::from)).unwrap_or(resp.body.clone());
            return Err(util::status_error(NAME, resp.status, &detail, &self.key));
        }
        resp.json()
            .ok()
            .and_then(|v| v["token"].as_str().map(String::from))
            .filter(|t| !t.is_empty())
            .ok_or_else(|| util::err(SttErrorCode::Other, "AssemblyAI did not return a streaming token.", "no token in response"))
    }

    fn on_turn(&mut self, v: &Value) -> bool {
        let transcript = v["transcript"].as_str().unwrap_or("").trim();
        if transcript.is_empty() {
            return false;
        }
        if v["end_of_turn"].as_bool() != Some(true) {
            let changed = self.partial != transcript;
            self.partial = transcript.to_string();
            return changed;
        }
        let formatted = v["turn_is_formatted"].as_bool() == Some(true);
        let order = v["turn_order"].as_u64();
        let normalized = normalize(transcript);
        if normalized.is_empty() {
            return false;
        }
        self.partial.clear();
        if self.finalize_sent && formatted {
            self.flushed = true;
        }
        // The server can emit the same turn raw and then formatted: keep one, prefer formatted.
        let same = self
            .turns
            .iter()
            .rposition(|t| order.is_some() && t.order == order)
            .or_else(|| self.turns.len().checked_sub(1).filter(|&i| self.turns[i].normalized == normalized));
        match same {
            Some(i) => {
                if formatted && self.turns[i].text != transcript {
                    self.turns[i].text = transcript.to_string();
                    self.turns[i].normalized = normalized;
                    true
                } else {
                    false
                }
            }
            None => {
                self.turns.push(Turn { order, normalized, text: transcript.to_string() });
                true
            }
        }
    }
}

impl Protocol for AssemblyProto {
    fn name(&self) -> &'static str {
        NAME
    }

    fn connect_spec(&mut self) -> Result<ConnectSpec, SttError> {
        let token = self.mint_token()?;
        Ok(ConnectSpec { url: format!("{}?{}", self.ws_base, ws_query(&token, &self.model, &self.keyterms)), headers: Vec::new() })
    }

    fn ready(&self) -> bool {
        self.began
    }

    fn encode(&mut self, frame: &[i16]) -> Message {
        Message::binary(util::pcm_to_le_bytes(frame))
    }

    /// AssemblyAI hard-closes the session on frames outside 50-1000 ms.
    fn pad_tail(&self) -> bool {
        true
    }

    fn on_message(&mut self, text: &str, _out: &mut Vec<Message>) -> bool {
        let Ok(v) = serde_json::from_str::<Value>(text) else { return false };
        match v["type"].as_str() {
            Some("Begin") => {
                self.began = true;
                true
            }
            Some("Turn") => self.on_turn(&v),
            Some("Termination") => {
                self.terminated = true;
                true
            }
            Some("Error") => {
                let msg = v["error"].as_str().or_else(|| v["message"].as_str()).unwrap_or("AssemblyAI error");
                self.err = Some(util::err(SttErrorCode::Other, "AssemblyAI reported an error.", util::scrub(msg, &self.key)));
                true
            }
            _ => false,
        }
    }

    fn on_close(&mut self, code: u16, reason: &str) {
        if code != 1000 && code != 1005 && self.err.is_none() {
            // 3005+ are AssemblyAI application errors (bad frame size, session limits, ...).
            self.err = Some(util::err(
                SttErrorCode::Unreachable,
                "AssemblyAI closed the connection.",
                format!("close {code}: {}", util::scrub(reason, &self.key)),
            ));
        }
    }

    fn finalize(&mut self) -> Vec<Message> {
        self.finalize_sent = true;
        vec![Message::text(r#"{"type":"ForceEndpoint"}"#)]
    }

    fn done(&self) -> bool {
        self.flushed || self.terminated
    }

    fn partial_pending(&self) -> bool {
        !self.partial.is_empty()
    }

    fn closing(&mut self) -> Vec<Message> {
        vec![Message::text(r#"{"type":"Terminate"}"#)]
    }

    fn text(&self) -> String {
        if self.turns.is_empty() {
            self.partial.clone()
        } else {
            self.turns.iter().map(|t| t.text.as_str()).collect::<Vec<_>>().join(" ")
        }
    }

    fn error(&self) -> Option<SttError> {
        self.err.clone()
    }
}

const _: () = assert!(FRAME_SAMPLES * 1000 / 16_000 == 50);

#[cfg(test)]
mod tests {
    use super::*;

    fn turn(t: &str, end: bool, formatted: bool, order: u64) -> String {
        format!(r#"{{"type":"Turn","turn_order":{order},"turn_is_formatted":{formatted},"end_of_turn":{end},"transcript":"{t}"}}"#)
    }

    #[test]
    fn query_has_v3_params() {
        let q = ws_query("tok+/=", "universal-3-5-pro", &["Open Whispr".into(), "Rust".into()]);
        assert!(q.starts_with("sample_rate=16000&encoding=pcm_s16le&format_turns=true&token=tok%2B%2F%3D"));
        assert!(q.contains("&speech_model=universal-3-5-pro"));
        assert!(q.contains("&keyterms_prompt=%5B%22Open%20Whispr%22%2C%22Rust%22%5D"));
        assert!(!ws_query("t", "", &[]).contains("speech_model"));
    }

    #[test]
    fn turns_dedupe_raw_then_formatted() {
        let mut p = AssemblyProto::default();
        let mut o = Vec::new();
        assert!(!p.ready());
        assert!(p.on_message(r#"{"type":"Begin","id":"abc"}"#, &mut o) && p.ready());
        assert!(p.on_message(&turn("hello wor", false, false, 0), &mut o));
        assert!(p.partial_pending());
        assert!(p.on_message(&turn("hello world", true, false, 0), &mut o));
        assert!(!p.partial_pending());
        p.finalize();
        assert!(!p.done());
        // formatted variant of the same turn replaces the raw one instead of duplicating it
        assert!(p.on_message(&turn("Hello, world.", true, true, 0), &mut o));
        assert!(p.done());
        assert!(p.on_message(&turn("Second turn.", true, true, 1), &mut o));
        assert_eq!(p.text(), "Hello, world. Second turn.");
        // an identical repeat is ignored
        assert!(!p.on_message(&turn("Second turn.", true, true, 1), &mut o));
        assert_eq!(p.turns.len(), 2);
    }

    #[test]
    fn partial_is_the_fallback_text() {
        let mut p = AssemblyProto::default();
        let mut o = Vec::new();
        p.on_message(&turn("only a partial", false, false, 0), &mut o);
        assert_eq!(p.text(), "only a partial");
    }
}
