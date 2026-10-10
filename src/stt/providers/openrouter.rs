//! OpenRouter: audio-capable chat models used as a transcriber.
//! `POST {base}/chat/completions` with the recording as an `input_audio` part; the system
//! prompt asks for a verbatim transcript and carries the language hint and dictionary.
use super::{api_key, base_lang, base_url, join_prompt, model_or, timeout};
use crate::core::config::SttProviderId;
use crate::core::http;
use crate::stt::errors::{self, SttError};
use crate::stt::{Audio, BatchProvider, Credentials, SttOptions, Transcript};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Value, json};

pub const DEFAULT_BASE: &str = "https://openrouter.ai/api/v1";
pub const DEFAULT_MODEL: &str = "google/gemini-3.5-flash-lite";
/// Audio travels inline as base64, so keep the WAV modest.
const FILE_LIMIT: usize = 14 * 1024 * 1024;
const MAX_VOCAB_CHARS: usize = 4000;

pub struct Openrouter;

impl BatchProvider for Openrouter {
    fn id(&self) -> SttProviderId {
        SttProviderId::Openrouter
    }

    fn prewarm_url(&self, creds: &Credentials) -> Option<String> {
        Some(base_url(creds, DEFAULT_BASE))
    }

    fn transcribe(&self, audio: &Audio, opts: &SttOptions, creds: &Credentials) -> Result<Transcript, SttError> {
        let key = api_key(creds, "OpenRouter")?;
        let wav = audio.to_wav();
        super::check_size("OpenRouter", wav.len(), FILE_LIMIT)?;
        let model = model_or(opts, DEFAULT_MODEL);
        let body = request_body(model, &wav, opts);

        let url = format!("{}/chat/completions", base_url(creds, DEFAULT_BASE));
        let auth = format!("Bearer {key}");
        let headers = [("Authorization", auth.as_str()), ("HTTP-Referer", "https://opentalk.app"), ("X-Title", "OpenTalk")];
        let res = http::post_json(&url, &headers, &body, timeout(opts));
        let resp = errors::check_response("OpenRouter", Some(model), false, res)?;
        let v: Value = resp.json().map_err(|e| errors::bad_response("OpenRouter", false, &format!("invalid JSON ({e})"), &resp.body))?;
        parse_response(&v, &resp.body).map(|text| Transcript { text, detected_language: None })
    }
}

pub fn system_prompt(opts: &SttOptions) -> String {
    let mut p = String::from(
        "You are a verbatim speech-to-text engine. Transcribe the audio exactly as spoken, in the language spoken; never translate. \
         Reply with the transcript text only: no preamble, no quotation marks, no markdown, no timestamps, no speaker labels, no commentary. \
         Use natural punctuation and capitalisation. If there is no intelligible speech, reply with nothing.",
    );
    if let Some(lang) = opts.language.as_deref().filter(|l| !l.is_empty() && *l != "auto") {
        p.push_str(&format!(" The speaker is speaking the language with ISO 639-1 code \"{}\".", base_lang(lang)));
    }
    let vocab = join_prompt(&opts.hints, MAX_VOCAB_CHARS);
    if !vocab.is_empty() {
        p.push_str(&format!(" Spell these names and terms exactly as written when they are spoken: {vocab}."));
    }
    p
}

fn request_body(model: &str, wav: &[u8], opts: &SttOptions) -> Value {
    json!({
        "model": model,
        "temperature": 0,
        "messages": [
            { "role": "system", "content": system_prompt(opts) },
            { "role": "user", "content": [
                { "type": "text", "text": "Transcribe this audio." },
                { "type": "input_audio", "input_audio": { "data": STANDARD.encode(wav), "format": "wav" } },
            ]},
        ],
    })
}

fn parse_response(v: &Value, raw: &str) -> Result<String, SttError> {
    // OpenRouter reports upstream failures as `{"error": {...}}`, sometimes with HTTP 200.
    if let Some(err) = v.get("error").filter(|e| !e.is_null()) {
        let status = err.get("code").and_then(|c| c.as_u64()).filter(|c| (400..600).contains(c)).unwrap_or(502) as u16;
        let msg = err.get("message").and_then(|m| m.as_str()).unwrap_or_default();
        let mut e = errors::classify_for("OpenRouter", None, false, status, &format!("{msg} {raw}"));
        e.detail = format!("HTTP {status} in body: {}", errors::redact(msg));
        return Err(e);
    }
    let content = v.pointer("/choices/0/message/content");
    let text = match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => parts.iter().filter_map(|p| p.get("text").and_then(|t| t.as_str())).collect::<Vec<_>>().join(""),
        Some(Value::Null) | None if v.pointer("/choices/0").is_some() => String::new(),
        _ => return Err(errors::bad_response("OpenRouter", false, "no choices[0].message.content", raw)),
    };
    Ok(clean_transcript(&text))
}

/// Remove the wrapper chat models like to add: code fences, "Here is the transcription:"
/// preambles, a `Transcript:` label and quotes around the whole answer.
pub fn clean_transcript(raw: &str) -> String {
    let mut t = raw.trim().to_string();
    if t.starts_with("```") {
        t = t.split_once('\n').map_or(String::new(), |(_, rest)| rest.to_string());
        if let Some(i) = t.rfind("```") {
            t.truncate(i);
        }
        t = t.trim().to_string();
    }
    // A leading line that only introduces the transcript.
    if let Some((first, rest)) = t.split_once('\n') {
        let f = first.trim().to_lowercase();
        let intro = ["here is", "here's", "here are", "sure", "certainly", "okay", "ok,", "the transcription", "the transcript", "transcription", "transcript"];
        if f.ends_with(':') && intro.iter().any(|p| f.starts_with(p)) {
            t = rest.trim().to_string();
        }
    }
    for label in ["transcription:", "transcript:"] {
        if t.len() > label.len() && t.is_char_boundary(label.len()) && t[..label.len()].eq_ignore_ascii_case(label) {
            t = t[label.len()..].trim().to_string();
            break;
        }
    }
    for (open, close) in [('"', '"'), ('“', '”')] {
        let inner = t.strip_prefix(open).and_then(|r| r.strip_suffix(close));
        if let Some(inner) = inner.filter(|i| !i.contains(open) && !i.contains(close)) {
            t = inner.trim().to_string();
            break;
        }
    }
    if is_placeholder(&t) || is_refusal(&t) { String::new() } else { t }
}

/// A chat model that never received the audio (or declined) answers "I'm sorry, but I can't
/// transcribe the audio." in prose; pasting that as dictation would be worse than an error.
/// Deliberately narrow: a short apology/refusal that talks about the audio input itself, so
/// a dictated sentence such as "I can't transcribe the interview until Friday" survives.
fn is_refusal(t: &str) -> bool {
    let lower = t.trim().to_lowercase().replace('\u{2019}', "'");
    if lower.chars().count() > 240 {
        return false;
    }
    let opens = ["i'm sorry", "i am sorry", "sorry,", "sorry!", "i can't", "i cannot", "i'm unable", "i am unable", "unfortunately"];
    let about_audio = [
        "transcribe the audio",
        "transcribe this audio",
        "transcribe audio",
        "transcribe the provided audio",
        "no audio",
        "audio was provided",
        "audio provided",
        "audio attached",
        "audio file attached",
        "audio content",
        "receive any audio",
        "receive the audio",
        "access to the audio",
        "access to audio",
        "listen to audio",
        "listen to the audio",
        "process audio",
        "process the audio",
    ];
    opens.iter().any(|o| lower.starts_with(o)) && about_audio.iter().any(|a| lower.contains(a))
}

/// Chat models answer silence with a bracketed note such as `(No output)` or `[silence]`;
/// that must never be pasted as if it were dictation.
fn is_placeholder(t: &str) -> bool {
    let inner = [('(', ')'), ('[', ']'), ('*', '*'), ('<', '>')]
        .iter()
        .find_map(|(o, c)| t.strip_prefix(*o).and_then(|r| r.strip_suffix(*c)))
        .map(str::trim);
    let Some(inner) = inner.filter(|i| !i.is_empty() && !i.contains(['(', ')', '[', ']']) && i.split_whitespace().count() <= 6) else {
        return false;
    };
    let lower = inner.to_lowercase();
    ["no output", "no audio", "no speech", "no transcript", "silence", "silent", "inaudible", "unintelligible", "blank", "empty", "background noise", "music"]
        .iter()
        .any(|k| lower.contains(k))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stt::SttErrorCode;
    use crate::stt::providers::testutil::*;

    const OK: &str = r#"{"id":"gen-1","choices":[{"message":{"role":"assistant","content":"Hey, can you send the report?"}}]}"#;

    #[test]
    fn request_shape() {
        let mock = serve(vec![Resp::json(200, OK)]);
        let mut o = opts("thinkingmachines/inkling-small:free");
        o.language = Some("en-US".into());
        o.hints = vec!["Priya".into(), "OpenTalk".into()];
        let a = audio();
        let t = Openrouter.transcribe(&a, &o, &creds(&mock, "sk-or-v1-test-key-0001")).unwrap();
        assert_eq!(t.text, "Hey, can you send the report?");

        let r = mock.one();
        assert_eq!((r.method.as_str(), r.path.as_str()), ("POST", "/chat/completions"));
        assert_eq!(r.header("authorization"), Some("Bearer sk-or-v1-test-key-0001"));
        assert_eq!(r.header("http-referer"), Some("https://opentalk.app"));
        assert_eq!(r.header("x-title"), Some("OpenTalk"));
        assert_eq!(r.header("content-type"), Some("application/json"));
        let b = r.json();
        assert_eq!(b["model"], "thinkingmachines/inkling-small:free");
        assert_eq!(b["temperature"], 0);
        let msgs = b["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0]["role"], "system");
        let sys = msgs[0]["content"].as_str().unwrap();
        assert!(sys.contains("verbatim"), "{sys}");
        assert!(sys.contains("\"en\""), "language hint: {sys}");
        assert!(sys.contains("Priya, OpenTalk"), "dictionary: {sys}");
        assert_eq!(msgs[1]["role"], "user");
        assert_eq!(msgs[1]["content"][0], json!({"type": "text", "text": "Transcribe this audio."}));
        let part = &msgs[1]["content"][1];
        assert_eq!(part["type"], "input_audio");
        assert_eq!(part["input_audio"]["format"], "wav");
        assert_eq!(STANDARD.decode(part["input_audio"]["data"].as_str().unwrap()).unwrap(), a.to_wav());
    }

    #[test]
    fn prompt_without_language_or_hints_is_plain() {
        let p = system_prompt(&opts("m"));
        assert!(!p.contains("ISO 639-1") && !p.contains("Spell these"));
    }

    #[test]
    fn content_parts_and_empty_replies() {
        let v: Value = serde_json::from_str(r#"{"choices":[{"message":{"content":[{"type":"text","text":"Hello "},{"type":"text","text":"world"}]}}]}"#).unwrap();
        assert_eq!(parse_response(&v, "").unwrap(), "Hello world");
        let v: Value = serde_json::from_str(r#"{"choices":[{"message":{"content":null,"reasoning":"..."}}]}"#).unwrap();
        assert_eq!(parse_response(&v, "").unwrap(), "");
        let v: Value = serde_json::from_str(r#"{"choices":[]}"#).unwrap();
        assert_eq!(parse_response(&v, "{}").unwrap_err().code, SttErrorCode::Other);
    }

    #[test]
    fn strips_wrapper_text() {
        for (raw, want) in [
            ("Hello there.", "Hello there."),
            ("  Hello there.\n", "Hello there."),
            ("```\nHello there.\n```", "Hello there."),
            ("```text\nHello there.\n```", "Hello there."),
            ("Here is the transcription:\n\nHello there.", "Hello there."),
            ("Here's the transcript:\nHello there.", "Hello there."),
            ("Transcription: Hello there.", "Hello there."),
            ("\"Hello there.\"", "Hello there."),
            ("“Hello there.”", "Hello there."),
            ("He said \"hi\" and left.", "He said \"hi\" and left."),
            ("\"One.\" \"Two.\"", "\"One.\" \"Two.\""),
            ("Here is what happened: we shipped.", "Here is what happened: we shipped."),
            ("(No output)", ""),
            ("(No audio provided)", ""),
            ("[silence]", ""),
            ("*inaudible*", ""),
            ("I\u{2019}m sorry, but I can\u{2019}t transcribe the audio.", ""),
            ("I'm sorry, but I can't transcribe the audio because no audio was provided.", ""),
            ("Unfortunately there is no audio file attached.", ""),
            ("I'm sorry I can't make the meeting tomorrow.", "I'm sorry I can't make the meeting tomorrow."),
            ("I can't believe we shipped it.", "I can't believe we shipped it."),
            ("I can't transcribe the interview until Friday.", "I can't transcribe the interview until Friday."),
            ("Sorry, the audio file was corrupted, can you resend it?", "Sorry, the audio file was corrupted, can you resend it?"),
            ("I'm unable to process the audio you sent.", ""),
            ("(laughs) that was great", "(laughs) that was great"),
            ("(Please send the report) ok", "(Please send the report) ok"),
            ("", ""),
        ] {
            assert_eq!(clean_transcript(raw), want, "{raw:?}");
        }
    }

    #[test]
    fn upstream_error_in_a_200_body_is_classified() {
        let mock = serve(vec![Resp::json(200, r#"{"error":{"code":429,"message":"Provider returned error: rate-limited upstream"}}"#)]);
        let e = Openrouter.transcribe(&audio(), &opts("m:free"), &creds(&mock, "sk-or-v1-test-key-0001")).unwrap_err();
        assert_eq!(e.code, SttErrorCode::RateLimited);
        assert_eq!(e.message, "OpenRouter rate-limited the request. Wait a moment and try again.");
    }

    #[test]
    fn http_errors() {
        let mock = serve(vec![Resp::json(401, r#"{"error":{"message":"No auth credentials found","code":401}}"#)]);
        let e = Openrouter.transcribe(&audio(), &opts("m"), &creds(&mock, "sk-or-v1-test-key-0001")).unwrap_err();
        assert_eq!((e.code, e.message.as_str()), (SttErrorCode::AuthFailed, "OpenRouter rejected your API key."));
        let mock = serve(vec![Resp::json(402, "{}")]);
        let e = Openrouter.transcribe(&audio(), &opts("m"), &creds(&mock, "sk-or-v1-test-key-0001")).unwrap_err();
        assert_eq!(e.code, SttErrorCode::QuotaExhausted);
        let mock = serve(vec![Resp::json(404, r#"{"error":{"message":"No endpoints found for x/y"}}"#)]);
        let e = Openrouter.transcribe(&audio(), &opts("x/y"), &creds(&mock, "sk-or-v1-test-key-0001")).unwrap_err();
        assert_eq!((e.code, e.message.as_str()), (SttErrorCode::ModelNotFound, "OpenRouter doesn't recognize the model “x/y”."));
        let e = Openrouter.transcribe(&audio(), &opts("m"), &Credentials::default()).unwrap_err();
        assert_eq!(e.code, SttErrorCode::NotConfigured);
    }

    /// Free audio-input models from OpenRouter's catalogue (id ends with `:free`).
    fn free_audio_models() -> Vec<String> {
        let mut ids = vec!["thinkingmachines/inkling-small:free".to_string()];
        let listed = crate::core::http::get("https://openrouter.ai/api/v1/models", &[], std::time::Duration::from_secs(20))
            .ok()
            .filter(|r| r.ok())
            .and_then(|r| r.json().ok());
        for m in listed.as_ref().and_then(|v| v["data"].as_array()).into_iter().flatten() {
            let audio = m.pointer("/architecture/input_modalities").and_then(|a| a.as_array()).is_some_and(|a| a.iter().any(|x| x == "audio"));
            if let Some(id) = m["id"].as_str().filter(|id| audio && id.ends_with(":free") && !ids.iter().any(|i| i == id)) {
                ids.push(id.to_string());
            }
        }
        ids
    }

    /// Verdict logic shared with `tests/e2e.rs` (one definition: `tests/common/live.rs`).
    #[allow(dead_code)]
    mod live {
        include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/common/live.rs"));
    }
    use live::{Live, report_live, try_model};

    /// The live classifier reads user-facing sentences; pin it to the real ones so a reworded
    /// message cannot turn a rate limit into a `Failed` live run (or a bad key into a retry).
    #[test]
    fn live_classifier_matches_the_real_error_messages() {
        use SttErrorCode as C;
        use live::{Attempt, classify_message};
        let cases = [
            (C::RateLimited, Attempt::Retry),
            (C::Unavailable, Attempt::Retry),
            (C::Timeout, Attempt::Retry),
            (C::NoSpeech, Attempt::DroppedAudio),
            (C::AccessDenied, Attempt::AccessDenied),
            (C::AuthFailed, Attempt::Failed),
            (C::ModelNotFound, Attempt::Failed),
            (C::BadRequest, Attempt::Failed),
        ];
        for (code, want) in cases {
            let msg = errors::message_for(code, "OpenRouter", Some("m:free"), false);
            assert_eq!(classify_message(&msg), want, "{code:?}: {msg}");
        }
    }

    /// Live: transcribes tests/fixtures/tts_clean.wav through a free OpenRouter audio model.
    /// Needs OPENTALK_SECRET_OPENROUTER (only `:free` models are ever requested).
    ///
    /// Prints `VERIFIED` (a transcript with the spoken words), `UNVERIFIED` or `FAILED`.
    /// Upstream limits are `UNVERIFIED`: 429/5xx/timeouts (retried with backoff), 403 "only
    /// available on agentic harnesses" (the inkling models, one attempt), and a free endpoint
    /// that answers but drops the audio before the model (NVIDIA Nemotron: prompt_tokens=28
    /// with or without the WAV, so a `NoSpeech` error or a reply without the spoken words;
    /// one attempt, PLAN section 10). `UNVERIFIED` passes unless `OPENTALK_LIVE_REQUIRE=1`,
    /// which turns it into a failure. Anything else (bad key, bad request) is `FAILED`.
    #[test]
    #[ignore]
    fn live_openrouter_transcribe() {
        const TEST: &str = "live_openrouter_transcribe";
        let Ok(key) = std::env::var("OPENTALK_SECRET_OPENROUTER") else {
            return report_live(TEST, Live::Unverified("OPENTALK_SECRET_OPENROUTER not set".into()));
        };
        let wav = std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/tts_clean.wav")).expect("fixture");
        let audio = Audio::from_wav(&wav).expect("fixture is PCM16 WAV");
        let creds = Credentials { api_key: Some(key), base_url: None, extra: vec![] };
        let mut notes = Vec::new();
        let models = free_audio_models();
        eprintln!("candidate free audio models: {models:?}");
        for model in models.iter().take(6) {
            assert!(model.ends_with(":free"), "live tests may only use :free models");
            let mut o = opts(model);
            o.language = Some("en".into());
            o.hints = vec!["Priya".into()];
            o.timeout_ms = 60_000;
            let verdict = try_model(
                model,
                &["report", "priya"],
                &[0, 3, 8],
                || {
                    let t0 = std::time::Instant::now();
                    let res = Openrouter.transcribe(&audio, &o, &creds);
                    match &res {
                        Ok(t) => eprintln!("LIVE model={model} latency={} ms transcript={:?}", t0.elapsed().as_millis(), t.text),
                        Err(e) => eprintln!("LIVE model={model} {:?}: {}", e.code, e.detail),
                    }
                    res.map(|t| t.text).map_err(|e| e.message)
                },
                std::thread::sleep,
            );
            match verdict {
                Live::Unverified(why) => notes.push(why),
                done => return report_live(TEST, done),
            }
        }
        report_live(TEST, Live::Unverified(if notes.is_empty() { "no candidate models were tried".into() } else { notes.join("; ") }));
    }
}
