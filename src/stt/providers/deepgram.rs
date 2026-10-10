//! Deepgram pre-recorded REST: `POST {base}/listen?model=..&smart_format=true&punctuate=true
//! [&language=xx][&keyterm=..]`, `Authorization: Token <key>`, raw `audio/wav` body.
//! (Realtime dictation uses the WebSocket path in `streaming/`.)
use super::{api_key, base_url, model_or, percent_encode, timeout};
use crate::core::config::SttProviderId;
use crate::core::http;
use crate::stt::errors::{self, SttError};
use crate::stt::{Audio, BatchProvider, Credentials, SttOptions, Transcript};

pub const DEFAULT_BASE: &str = "https://api.deepgram.com/v1";
pub const DEFAULT_MODEL: &str = "nova-3";
const MAX_KEYTERMS: usize = 100;
const MAX_KEYTERM_CHARS: usize = 50;

pub struct Deepgram;

impl BatchProvider for Deepgram {
    fn id(&self) -> SttProviderId {
        SttProviderId::Deepgram
    }

    fn prewarm_url(&self, creds: &Credentials) -> Option<String> {
        Some(base_url(creds, DEFAULT_BASE))
    }

    fn transcribe(&self, audio: &Audio, opts: &SttOptions, creds: &Credentials) -> Result<Transcript, SttError> {
        let key = api_key(creds, "Deepgram")?;
        let wav = audio.to_wav();
        super::check_size("Deepgram", wav.len(), super::BYOK_FILE_LIMIT)?;
        let model = model_or(opts, DEFAULT_MODEL);
        let url = format!("{}/listen?{}", base_url(creds, DEFAULT_BASE), query(model, opts));

        let auth = format!("Token {key}");
        let res = http::post(&url, &[("Authorization", &auth)], "audio/wav", &wav, timeout(opts));
        let secrets = [key];
        let resp = errors::check_response_with("Deepgram", Some(model), false, res, &secrets)?;
        let v = resp.json().map_err(|e| errors::bad_response_with("Deepgram", false, &format!("invalid JSON ({e})"), &resp.body, &secrets))?;
        let channel = v.pointer("/results/channels/0");
        let Some(text) = channel.and_then(|c| c.pointer("/alternatives/0/transcript")).and_then(|t| t.as_str()) else {
            return Err(errors::bad_response_with("Deepgram", false, "no transcript", &resp.body, &secrets));
        };
        let detected_language = channel.and_then(|c| c.get("detected_language")).and_then(|l| l.as_str()).map(String::from);
        Ok(Transcript { text: text.to_string(), detected_language })
    }
}

fn query(model: &str, opts: &SttOptions) -> String {
    let mut q = format!("model={}&smart_format=true&punctuate=true", percent_encode(model));
    if let Some(lang) = opts.language.as_deref().filter(|l| !l.is_empty() && *l != "auto") {
        q.push_str("&language=");
        q.push_str(&percent_encode(lang));
    }
    // Nova-3 takes `keyterm`; older models take `keywords`.
    let param = if model.starts_with("nova-3") { "keyterm" } else { "keywords" };
    let terms = opts.hints.iter().map(|t| t.trim().chars().take(MAX_KEYTERM_CHARS).collect::<String>()).filter(|t| !t.is_empty()).take(MAX_KEYTERMS);
    for t in terms {
        q.push('&');
        q.push_str(param);
        q.push('=');
        q.push_str(&percent_encode(&t));
    }
    q
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stt::SttErrorCode;
    use crate::stt::providers::testutil::*;

    const OK: &str = r#"{"metadata":{},"results":{"channels":[{"detected_language":"en","alternatives":[{"transcript":"Hello there.","confidence":0.99}]}]}}"#;

    #[test]
    fn request_shape() {
        let mock = serve(vec![Resp::json(200, OK)]);
        let mut o = opts("nova-3");
        o.language = Some("en-US".into());
        o.hints = vec!["Priya Shah".into(), "Open&Talk".into()];
        let a = audio();
        let t = Deepgram.transcribe(&a, &o, &creds(&mock, "dg-key-12345")).unwrap();
        assert_eq!(t.text, "Hello there.");
        assert_eq!(t.detected_language.as_deref(), Some("en"));
        let r = mock.one();
        assert_eq!(r.method, "POST");
        assert_eq!(r.path, "/listen?model=nova-3&smart_format=true&punctuate=true&language=en-US&keyterm=Priya%20Shah&keyterm=Open%26Talk");
        assert_eq!(r.header("authorization"), Some("Token dg-key-12345"));
        assert_eq!(r.header("content-type"), Some("audio/wav"));
        assert_eq!(r.body, a.to_wav());
    }

    #[test]
    fn auto_language_and_no_hints() {
        let mock = serve(vec![Resp::json(200, OK)]);
        Deepgram.transcribe(&audio(), &opts(""), &creds(&mock, "dg-key-12345")).unwrap();
        assert_eq!(mock.one().path, "/listen?model=nova-3&smart_format=true&punctuate=true");
    }

    #[test]
    fn older_models_use_keywords() {
        let mut o = opts("nova-2");
        o.hints = vec!["x".into()];
        assert!(query("nova-2", &o).ends_with("&keywords=x"));
    }

    #[test]
    fn empty_transcript_is_ok_but_missing_channel_is_not() {
        let mock = serve(vec![Resp::json(200, r#"{"results":{"channels":[{"alternatives":[{"transcript":""}]}]}}"#)]);
        assert_eq!(Deepgram.transcribe(&audio(), &opts(""), &creds(&mock, "dg-key-12345")).unwrap().text, "");
        let mock = serve(vec![Resp::json(200, r#"{"results":{"channels":[]}}"#)]);
        let e = Deepgram.transcribe(&audio(), &opts(""), &creds(&mock, "dg-key-12345")).unwrap_err();
        assert_eq!(e.code, SttErrorCode::Other);
    }

    #[test]
    fn errors() {
        let mock = serve(vec![Resp::json(401, r#"{"err_code":"INVALID_AUTH"}"#)]);
        let e = Deepgram.transcribe(&audio(), &opts(""), &creds(&mock, "dg-key-12345")).unwrap_err();
        assert_eq!((e.code, e.message.as_str()), (SttErrorCode::AuthFailed, "Deepgram rejected your API key."));
        let e = Deepgram.transcribe(&audio(), &opts(""), &Credentials::default()).unwrap_err();
        assert_eq!(e.code, SttErrorCode::NotConfigured);
    }

    #[test]
    fn configured_key_echoed_by_the_provider_is_redacted() {
        const KEY: &str = "dg3f9a1c77b25e0d84aa6c1e";
        let mock = serve(vec![Resp::json(401, &format!(r#"{{"err_code":"INVALID_AUTH","err_msg":"Invalid credentials. Token {KEY}"}}"#))]);
        let e = Deepgram.transcribe(&audio(), &opts(""), &creds(&mock, KEY)).unwrap_err();
        assert_eq!(e.code, SttErrorCode::AuthFailed);
        assert!(e.detail.starts_with("HTTP 401: "), "{}", e.detail);
        assert_no_leak(&e, KEY);
        // an unusable 2xx body echoing the key goes through bad_response
        let mock = serve(vec![Resp::json(200, &format!(r#"{{"echo":"{KEY}"}}"#))]);
        let e = Deepgram.transcribe(&audio(), &opts(""), &creds(&mock, KEY)).unwrap_err();
        assert_no_leak(&e, KEY);
        // and the dispatcher path (`providers::get`) agrees
        let mock = serve(vec![Resp::json(401, &format!("bad {KEY}"))]);
        assert_no_leak(&crate::stt::providers::get(SttProviderId::Deepgram).transcribe(&audio(), &opts(""), &creds(&mock, KEY)).unwrap_err(), KEY);
    }
}
