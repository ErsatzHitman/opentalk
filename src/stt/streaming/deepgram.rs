//! Deepgram live transcription (`wss://api.deepgram.com/v1/listen`), mirroring OpenWhispr's
//! deepgramStreaming.js: raw linear16 binary frames, `Results` with `is_final` /
//! `from_finalize`, `Finalize` on stop, `CloseStream` at the end, `KeepAlive` when idle.
use super::session::{self, ConnectSpec, Protocol};
use super::util;
use super::{DEFAULT_DEEPGRAM_ORIGIN, ws_url};
use crate::core::config::SttProviderId;
use crate::stt::{Credentials, StreamingProvider, StreamingSession, SttError, SttErrorCode, SttOptions};
use serde_json::Value;
use std::time::Duration;
use tungstenite::Message;

const NAME: &str = "Deepgram";
const KEEPALIVE: Duration = Duration::from_secs(3);
const MAX_KEYTERMS: usize = 100;

/// Base language codes Nova-3 accepts; anything else falls back to Nova-2 as OpenWhispr does.
const NOVA3_LANGUAGES: &[&str] = &[
    "ar", "be", "bn", "bs", "bg", "ca", "hr", "cs", "da", "nl", "en", "et", "fi", "fr", "de", "el", "he", "hi", "hu", "id", "it", "ja",
    "kn", "ko", "lv", "lt", "mk", "ms", "mr", "no", "fa", "pl", "pt", "ro", "ru", "sr", "sk", "sl", "es", "sv", "tl", "ta", "te", "tr",
    "uk", "ur", "vi", "multi",
];

pub struct Deepgram;

impl StreamingProvider for Deepgram {
    fn id(&self) -> SttProviderId {
        SttProviderId::Deepgram
    }

    fn start(&self, opts: &SttOptions, creds: &Credentials) -> Result<Box<dyn StreamingSession>, SttError> {
        let key = creds.api_key.as_deref().map(str::trim).filter(|k| !k.is_empty()).ok_or_else(|| util::not_configured(NAME))?;
        let (path, _model) = listen_path(opts);
        session::spawn(Box::new(DeepgramProto {
            url: ws_url(creds, DEFAULT_DEEPGRAM_ORIGIN, &path),
            key: key.to_string(),
            ..DeepgramProto::default()
        }))
    }
}

/// `/v1/listen?...` for the options, plus the model it selected.
pub(crate) fn listen_path(opts: &SttOptions) -> (String, &'static str) {
    let lang = util::explicit_language(opts.language.as_deref());
    let nova3 = match lang {
        None => true,
        Some(l) => {
            let l = l.to_ascii_lowercase();
            NOVA3_LANGUAGES.contains(&l.as_str()) || NOVA3_LANGUAGES.contains(&util::base_language(&l).as_str())
        }
    };
    // The registry only offers nova-3; honouring a pinned family would defeat the nova-2
    // downgrade that keeps languages outside the Nova-3 list working.
    let model = if nova3 { "nova-3" } else { "nova-2" };
    let mut q = format!("encoding=linear16&sample_rate=16000&channels=1&model={model}&punctuate=true&interim_results=true");
    if let Some(l) = lang {
        q.push_str("&language=");
        q.push_str(&util::percent_encode(l));
    }
    // Nova-3 takes `keyterm`, Nova-2 `keywords`.
    let param = if nova3 { "keyterm" } else { "keywords" };
    for t in util::clean_hints(&opts.hints, MAX_KEYTERMS) {
        q.push('&');
        q.push_str(param);
        q.push('=');
        q.push_str(&util::percent_encode(&t));
    }
    (format!("/v1/listen?{q}"), model)
}

#[derive(Default)]
struct DeepgramProto {
    url: String,
    key: String,
    segments: Vec<String>,
    partial: String,
    finalize_sent: bool,
    flushed: bool,
    err: Option<SttError>,
}

impl Protocol for DeepgramProto {
    fn name(&self) -> &'static str {
        NAME
    }

    fn connect_spec(&mut self) -> Result<ConnectSpec, SttError> {
        Ok(ConnectSpec { url: self.url.clone(), headers: vec![("Authorization".into(), format!("Token {}", self.key))] })
    }

    fn ready(&self) -> bool {
        true
    }

    fn encode(&mut self, frame: &[i16]) -> Message {
        Message::binary(util::pcm_to_le_bytes(frame))
    }

    fn on_message(&mut self, text: &str, _out: &mut Vec<Message>) -> bool {
        let Ok(v) = serde_json::from_str::<Value>(text) else { return false };
        match v["type"].as_str() {
            Some("Results") => {
                let transcript = v["channel"]["alternatives"][0]["transcript"].as_str().unwrap_or("").trim();
                let from_finalize = v["from_finalize"].as_bool() == Some(true);
                let is_final = v["is_final"].as_bool() == Some(true) || from_finalize;
                if from_finalize && self.finalize_sent {
                    self.flushed = true;
                }
                if is_final {
                    self.partial.clear();
                    if !transcript.is_empty() {
                        self.segments.push(transcript.to_string());
                    }
                    true
                } else {
                    let changed = self.partial != transcript;
                    self.partial = transcript.to_string();
                    changed
                }
            }
            Some("Error") => {
                let desc = v["description"].as_str().or_else(|| v["message"].as_str()).unwrap_or("Deepgram error");
                self.err = Some(util::err(SttErrorCode::Other, "Deepgram reported an error.", util::scrub(desc, &self.key)));
                true
            }
            _ => false,
        }
    }

    fn on_close(&mut self, code: u16, reason: &str) {
        if code != 1000 && code != 1005 && self.err.is_none() {
            self.err = Some(util::err(
                SttErrorCode::Unreachable,
                "Deepgram closed the connection.",
                format!("close {code}: {}", util::scrub(reason, &self.key)),
            ));
        }
    }

    fn finalize(&mut self) -> Vec<Message> {
        self.finalize_sent = true;
        vec![Message::text(r#"{"type":"Finalize"}"#)]
    }

    fn done(&self) -> bool {
        self.flushed
    }

    fn partial_pending(&self) -> bool {
        !self.partial.is_empty()
    }

    fn closing(&mut self) -> Vec<Message> {
        vec![Message::text(r#"{"type":"CloseStream"}"#)]
    }

    fn keepalive(&self) -> Option<(Duration, Message)> {
        Some((KEEPALIVE, Message::text(r#"{"type":"KeepAlive"}"#)))
    }

    fn text(&self) -> String {
        if self.segments.is_empty() { self.partial.clone() } else { self.segments.join(" ") }
    }

    fn error(&self) -> Option<SttError> {
        self.err.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(lang: Option<&str>, hints: &[&str]) -> SttOptions {
        SttOptions {
            model: "nova-3".into(),
            language: lang.map(String::from),
            hints: hints.iter().map(|s| s.to_string()).collect(),
            timeout_ms: 0,
        }
    }

    #[test]
    fn url_defaults_and_params() {
        let (p, m) = listen_path(&opts(None, &[]));
        assert_eq!(m, "nova-3");
        assert_eq!(p, "/v1/listen?encoding=linear16&sample_rate=16000&channels=1&model=nova-3&punctuate=true&interim_results=true");
        let (p, _) = listen_path(&opts(Some("auto"), &[]));
        assert!(!p.contains("language="));
        let (p, _) = listen_path(&opts(Some("pt-BR"), &["Open Whispr", "Rust", "rust"]));
        assert!(p.contains("&language=pt-BR"));
        assert!(p.contains("&keyterm=Open%20Whispr&keyterm=Rust"));
        assert_eq!(p.matches("keyterm=").count(), 2);
    }

    #[test]
    fn unsupported_language_falls_back_to_nova2_keywords() {
        let (p, m) = listen_path(&opts(Some("th"), &["Thai"]));
        assert_eq!(m, "nova-2");
        assert!(p.contains("model=nova-2") && p.contains("&language=th") && p.contains("&keywords=Thai"));
    }

    #[test]
    fn results_assembly() {
        let mut p = DeepgramProto::default();
        let mut out = Vec::new();
        let r = |t: &str, fin: bool, ff: bool| {
            format!(r#"{{"type":"Results","is_final":{fin},"from_finalize":{ff},"channel":{{"alternatives":[{{"transcript":"{t}"}}]}}}}"#)
        };
        assert!(p.on_message(&r("hel", false, false), &mut out));
        assert!(p.partial_pending() && p.text() == "hel");
        assert!(p.on_message(&r("hello there", true, false), &mut out));
        assert!(!p.partial_pending());
        assert!(p.on_message(&r("general", false, false), &mut out));
        p.finalize();
        assert!(!p.done());
        assert!(p.on_message(&r("general kenobi", true, true), &mut out));
        assert!(p.done());
        assert_eq!(p.text(), "hello there general kenobi");
        assert!(!p.on_message(r#"{"type":"Metadata","request_id":"x"}"#, &mut out));
    }
}
