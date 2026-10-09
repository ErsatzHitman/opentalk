//! Tinfoil: OpenAI-compatible batch endpoint over plain HTTPS. OpenWhispr verifies the
//! enclave attestation; this client does not (stated in the settings note).
use super::openai::{Auth, Request, transcribe_multipart};
use super::{api_key, base_url, model_or, prompt_limit};
use crate::core::config::SttProviderId;
use crate::stt::errors::SttError;
use crate::stt::{Audio, BatchProvider, Credentials, SttOptions, Transcript};

pub const DEFAULT_BASE: &str = "https://inference.tinfoil.sh/v1";
/// The batch model behind the "Voxtral" choice.
pub const BATCH_MODEL: &str = "voxtral-small-24b";

pub struct Tinfoil;

impl BatchProvider for Tinfoil {
    fn id(&self) -> SttProviderId {
        SttProviderId::Tinfoil
    }

    fn prewarm_url(&self, creds: &Credentials) -> Option<String> {
        Some(base_url(creds, DEFAULT_BASE))
    }

    fn transcribe(&self, audio: &Audio, opts: &SttOptions, creds: &Credentials) -> Result<Transcript, SttError> {
        let key = api_key(creds, "Tinfoil")?;
        let url = format!("{}/audio/transcriptions", base_url(creds, DEFAULT_BASE));
        let model = model_or(opts, BATCH_MODEL);
        transcribe_multipart(
            audio,
            opts,
            &Request {
                name: "Tinfoil",
                self_hosted: false,
                url: &url,
                auth: Auth::Bearer(key),
                model,
                prompt_limit: prompt_limit(false, model),
                stream: false,
                keywords: false,
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stt::SttErrorCode;
    use crate::stt::providers::testutil::*;

    #[test]
    fn request_shape() {
        let mock = serve(vec![Resp::json(200, r#"{"text":"hello"}"#)]);
        let mut o = opts("voxtral-small-24b");
        o.language = Some("fr".into());
        o.hints = vec!["Priya".into()];
        let t = Tinfoil.transcribe(&audio(), &o, &creds(&mock, "tf-key-12345")).unwrap();
        assert_eq!(t.text, "hello");
        let r = mock.one();
        assert_eq!((r.method.as_str(), r.path.as_str()), ("POST", "/audio/transcriptions"));
        assert_eq!(r.header("authorization"), Some("Bearer tf-key-12345"));
        let parts = r.multipart();
        assert_eq!(parts.names(), ["file", "model", "language", "prompt"]);
        assert_eq!(parts.text("model"), ["voxtral-small-24b"]);
        assert_eq!(parts.text("prompt"), ["Priya"]);
    }

    #[test]
    fn errors() {
        let mock = serve(vec![Resp::json(401, "{}")]);
        let e = Tinfoil.transcribe(&audio(), &opts(""), &creds(&mock, "tf-key-12345")).unwrap_err();
        assert_eq!((e.code, e.message.as_str()), (SttErrorCode::AuthFailed, "Tinfoil rejected your API key."));
        assert_eq!(mock.one().multipart().text("model"), ["voxtral-small-24b"]);
    }
}
