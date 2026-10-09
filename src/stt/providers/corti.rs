//! Corti batch dictation: OAuth client-credentials, then the interaction flow
//! (create interaction -> upload recording -> request transcript), and finally delete the
//! interaction so no dictation audio persists on Corti's servers.
//!
//! Credentials: `api_key` = client id; `extra` = `client_secret`, `environment` (`us`|`eu`),
//! `tenant`. `base_url` overrides both hosts (for tests): API at `{base}`, token endpoint at
//! `{base}/realms/{tenant}/protocol/openid-connect/token`.
use super::{extra, timeout};
use crate::core::config::SttProviderId;
use crate::core::http::{self, HttpResponse};
use crate::stt::errors::{self, SttError, SttErrorCode};
use crate::stt::{Audio, BatchProvider, Credentials, SttOptions, Transcript};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

const NAME: &str = "Corti";
/// Corti access tokens live 5 minutes; refresh early so a request never races expiry.
const TOKEN_REFRESH_MARGIN: Duration = Duration::from_secs(30);
const TOKEN_TIMEOUT: Duration = Duration::from_secs(12);

pub struct Corti;

struct Target {
    environment: String,
    tenant: String,
    api: String,
    token_url: String,
}

impl Target {
    fn resolve(creds: &Credentials) -> Result<Target, SttError> {
        let environment = extra(creds, "environment").unwrap_or("us").to_string();
        let tenant = extra(creds, "tenant").unwrap_or("base").to_string();
        if !matches!(environment.as_str(), "us" | "eu") {
            return Err(SttError::new(SttErrorCode::BadRequest, format!("Invalid Corti environment: {environment}")));
        }
        if tenant.is_empty() || !tenant.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-') {
            return Err(SttError::new(SttErrorCode::BadRequest, "Invalid Corti tenant name"));
        }
        let (api, auth_origin) = match creds.base_url.as_deref().map(str::trim).filter(|b| !b.is_empty()) {
            Some(b) => (b.trim_end_matches('/').to_string(), b.trim_end_matches('/').to_string()),
            None => (format!("https://api.{environment}.corti.app/v2"), format!("https://auth.{environment}.corti.app")),
        };
        let token_url = format!("{auth_origin}/realms/{tenant}/protocol/openid-connect/token");
        Ok(Target { environment, tenant, api, token_url })
    }
}

struct CachedToken {
    value: String,
    expires: Instant,
}

static TOKENS: Mutex<Option<HashMap<String, CachedToken>>> = Mutex::new(None);

impl BatchProvider for Corti {
    fn id(&self) -> SttProviderId {
        SttProviderId::Corti
    }

    fn prewarm_url(&self, creds: &Credentials) -> Option<String> {
        Target::resolve(creds).ok().map(|t| t.api)
    }

    fn transcribe(&self, audio: &Audio, opts: &SttOptions, creds: &Credentials) -> Result<Transcript, SttError> {
        let client_id = super::api_key(creds, NAME)?;
        let client_secret = extra(creds, "client_secret").ok_or_else(|| errors::not_configured(NAME))?;
        let target = Target::resolve(creds)?;
        let wav = audio.to_wav();
        super::check_size(NAME, wav.len(), super::BYOK_FILE_LIMIT)?;
        let to = timeout(opts);

        let token = token(&target, client_id, client_secret)?;
        let tenant = target.tenant.as_str();
        let auth = format!("Bearer {token}");
        let api = target.api.as_str();
        let call = |url: String, content_type: &str, body: &[u8]| -> Result<Value, SttError> {
            let headers = [("Authorization", auth.as_str()), ("Tenant-Name", tenant)];
            let resp = errors::check_response(NAME, None, false, http::post(&url, &headers, content_type, body, to))?;
            resp.json().map_err(|e| errors::bad_response(NAME, false, &format!("invalid JSON ({e})"), &resp.body))
        };

        let created = call(
            format!("{api}/interactions/"),
            "application/json",
            json!({ "encounter": { "identifier": format!("opentalk-{}", super::unique_id()), "status": "completed", "type": "consultation" } })
                .to_string()
                .as_bytes(),
        )?;
        let interaction = field(&created, "interactionId")?;

        let result = (|| {
            let rec = call(format!("{api}/interactions/{interaction}/recordings/"), "application/octet-stream", &wav)?;
            let recording = field(&rec, "recordingId")?;
            // Corti needs a concrete primaryLanguage; default to English when auto-detecting.
            let language = opts.language.as_deref().filter(|l| !l.is_empty() && *l != "auto").map_or_else(|| "en".to_string(), super::base_lang);
            let tr = call(
                format!("{api}/interactions/{interaction}/transcripts/"),
                "application/json",
                json!({ "recordingId": recording, "primaryLanguage": language, "isDictation": true }).to_string().as_bytes(),
            )?;
            let text = tr
                .get("transcripts")
                .and_then(|t| t.as_array())
                .into_iter()
                .flatten()
                .filter_map(|u| u.get("text").and_then(|t| t.as_str()))
                .collect::<Vec<_>>()
                .join(" ");
            Ok(Transcript { text, detected_language: None })
        })();

        // Dictation audio must not persist on Corti: deleting the interaction cascades to its
        // recordings and transcripts. Off the critical path.
        delete_later(format!("{api}/interactions/{interaction}"), auth, target.tenant.clone());
        result
    }
}

fn field(v: &Value, name: &str) -> Result<String, SttError> {
    v.get(name)
        .and_then(|x| x.as_str())
        .map(String::from)
        .ok_or_else(|| errors::bad_response(NAME, false, &format!("no `{name}`"), &v.to_string()))
}

fn delete_later(url: String, auth: String, tenant: String) {
    let spawned = std::thread::Builder::new().name("corti-cleanup".into()).spawn(move || {
        let headers = [("Authorization", auth.as_str()), ("Tenant-Name", tenant.as_str())];
        let r: Result<HttpResponse, _> = http::delete(&url, &headers, Duration::from_secs(10));
        match r {
            Ok(resp) if resp.ok() => {}
            Ok(resp) => crate::log_warn!("Corti interaction delete failed: HTTP {}", resp.status),
            Err(e) => crate::log_warn!("Corti interaction delete failed: {e}"),
        }
    });
    if spawned.is_err() {
        crate::log_warn!("could not spawn the Corti cleanup thread");
    }
}

fn cache_key(t: &Target, client_id: &str, client_secret: &str) -> String {
    // The secret is part of the key (digested) so edited credentials always re-authenticate.
    let digest = Sha256::digest(client_secret.as_bytes());
    let short: String = digest.iter().take(8).map(|b| format!("{b:02x}")).collect();
    format!("{}/{}/{}/{}/{short}", t.token_url, t.environment, t.tenant, client_id)
}

fn token(t: &Target, client_id: &str, client_secret: &str) -> Result<String, SttError> {
    let key = cache_key(t, client_id, client_secret);
    if let Some(c) = TOKENS.lock().ok().and_then(|g| g.as_ref().and_then(|m| m.get(&key).filter(|c| Instant::now() < c.expires).map(|c| c.value.clone()))) {
        return Ok(c);
    }
    let form = format!(
        "grant_type=client_credentials&client_id={}&client_secret={}&scope=openid",
        super::percent_encode(client_id),
        super::percent_encode(client_secret)
    );
    let res = http::post(&t.token_url, &[], "application/x-www-form-urlencoded", form.as_bytes(), TOKEN_TIMEOUT);
    let resp = errors::check_response(NAME, None, false, res)?;
    let v: Value = resp.json().map_err(|e| errors::bad_response(NAME, false, &format!("invalid token JSON ({e})"), ""))?;
    let Some(access) = v.get("access_token").and_then(|a| a.as_str()).filter(|a| !a.is_empty()) else {
        return Err(SttError::new(SttErrorCode::AuthFailed, "Corti authentication failed: no access token in response"));
    };
    let ttl = Duration::from_secs(v.get("expires_in").and_then(|e| e.as_u64()).filter(|e| *e > 0).unwrap_or(300));
    let expires = Instant::now() + ttl.saturating_sub(TOKEN_REFRESH_MARGIN);
    if let Ok(mut g) = TOKENS.lock() {
        g.get_or_insert_with(HashMap::new).insert(key, CachedToken { value: access.to_string(), expires });
    }
    Ok(access.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stt::providers::testutil::*;

    fn corti_creds(mock: &Mock, secret: &str) -> Credentials {
        Credentials {
            api_key: Some("client-id-1".into()),
            base_url: Some(mock.url.clone()),
            extra: vec![("client_secret".into(), secret.into()), ("environment".into(), "eu".into()), ("tenant".into(), "acme-1".into())],
        }
    }

    fn script() -> Vec<Resp> {
        vec![
            Resp::json(200, r#"{"access_token":"tok-abc","expires_in":300}"#),
            Resp::json(200, r#"{"interactionId":"int-1"}"#),
            Resp::json(200, r#"{"recordingId":"rec-1"}"#),
            Resp::json(200, r#"{"transcripts":[{"text":"Patient reports"},{"text":"mild headache."}]}"#),
            Resp::json(200, "{}"),
        ]
    }

    #[test]
    fn full_flow() {
        let mock = serve(script());
        let mut o = opts("corti-transcribe");
        o.language = Some("de-DE".into());
        let a = audio();
        let t = Corti.transcribe(&a, &o, &corti_creds(&mock, "full-flow-secret")).unwrap();
        assert_eq!(t.text, "Patient reports mild headache.");

        let reqs = mock.wait_for(5);
        assert_eq!(reqs.len(), 5, "{:?}", reqs.iter().map(|r| (&r.method, &r.path)).collect::<Vec<_>>());
        let tok = &reqs[0];
        assert_eq!((tok.method.as_str(), tok.path.as_str()), ("POST", "/realms/acme-1/protocol/openid-connect/token"));
        assert_eq!(tok.header("content-type"), Some("application/x-www-form-urlencoded"));
        assert_eq!(tok.body_str(), "grant_type=client_credentials&client_id=client-id-1&client_secret=full-flow-secret&scope=openid");

        for r in &reqs[1..] {
            assert_eq!(r.header("authorization"), Some("Bearer tok-abc"));
            assert_eq!(r.header("tenant-name"), Some("acme-1"));
        }
        let create = &reqs[1];
        assert_eq!((create.method.as_str(), create.path.as_str()), ("POST", "/interactions/"));
        let enc = create.json();
        assert_eq!(enc["encounter"]["status"], "completed");
        assert_eq!(enc["encounter"]["type"], "consultation");
        assert!(enc["encounter"]["identifier"].as_str().unwrap().starts_with("opentalk-"));

        let up = &reqs[2];
        assert_eq!((up.method.as_str(), up.path.as_str()), ("POST", "/interactions/int-1/recordings/"));
        assert_eq!(up.header("content-type"), Some("application/octet-stream"));
        assert_eq!(up.body, a.to_wav());

        let tr = &reqs[3];
        assert_eq!((tr.method.as_str(), tr.path.as_str()), ("POST", "/interactions/int-1/transcripts/"));
        assert_eq!(tr.json(), json!({"recordingId": "rec-1", "primaryLanguage": "de", "isDictation": true}));

        let del = &reqs[4];
        assert_eq!((del.method.as_str(), del.path.as_str()), ("DELETE", "/interactions/int-1"));
        assert_eq!(del.header("tenant-name"), Some("acme-1"));
    }

    #[test]
    fn auto_language_defaults_to_english() {
        let mock = serve(script());
        Corti.transcribe(&audio(), &opts("corti-transcribe"), &corti_creds(&mock, "auto-secret")).unwrap();
        assert_eq!(mock.wait_for(5)[3].json()["primaryLanguage"], "en");
    }

    #[test]
    fn token_is_reused_for_the_same_credentials() {
        let mock = serve(vec![
            Resp::json(200, r#"{"access_token":"tok-1","expires_in":300}"#),
            Resp::json(200, r#"{"interactionId":"i1"}"#),
            Resp::json(200, r#"{"recordingId":"r1"}"#),
            Resp::json(200, r#"{"transcripts":[{"text":"a"}]}"#),
            Resp::json(200, "{}"),
            // second dictation: create, upload, transcript, delete (no token request)
            Resp::json(200, r#"{"interactionId":"i2"}"#),
            Resp::json(200, r#"{"recordingId":"r2"}"#),
            Resp::json(200, r#"{"transcripts":[{"text":"b"}]}"#),
            Resp::json(200, "{}"),
        ]);
        let c = corti_creds(&mock, "reuse-secret");
        assert_eq!(Corti.transcribe(&audio(), &opts("m"), &c).unwrap().text, "a");
        mock.wait_for(5);
        assert_eq!(Corti.transcribe(&audio(), &opts("m"), &c).unwrap().text, "b");
        let reqs = mock.wait_for(9);
        assert_eq!(reqs.iter().filter(|r| r.path.contains("openid-connect/token")).count(), 1);
    }

    #[test]
    fn auth_failure_is_classified_and_nothing_else_is_called() {
        let mock = serve(vec![Resp::json(401, r#"{"error":"invalid_client","error_description":"Invalid client secret sk-leaky-secret-12345"}"#)]);
        let e = Corti.transcribe(&audio(), &opts("m"), &corti_creds(&mock, "bad-secret")).unwrap_err();
        assert_eq!((e.code, e.message.as_str()), (SttErrorCode::AuthFailed, "Corti rejected your API key."));
        assert!(!e.detail.contains("leaky-secret"));
        assert_eq!(mock.requests().len(), 1);
    }

    #[test]
    fn failed_transcript_still_deletes_the_interaction() {
        let mock = serve(vec![
            Resp::json(200, r#"{"access_token":"tok-del","expires_in":300}"#),
            Resp::json(200, r#"{"interactionId":"int-9"}"#),
            Resp::json(200, r#"{"recordingId":"rec-9"}"#),
            Resp::json(500, "boom"),
            Resp::json(200, "{}"),
        ]);
        let e = Corti.transcribe(&audio(), &opts("m"), &corti_creds(&mock, "del-secret")).unwrap_err();
        assert_eq!(e.code, SttErrorCode::Unavailable);
        let reqs = mock.wait_for(5);
        assert_eq!(reqs.last().map(|r| (r.method.as_str(), r.path.as_str())), Some(("DELETE", "/interactions/int-9")));
    }

    #[test]
    fn missing_credentials_and_bad_target() {
        let c = Credentials { api_key: Some("id".into()), base_url: None, extra: vec![] };
        assert_eq!(Corti.transcribe(&audio(), &opts("m"), &c).unwrap_err().code, SttErrorCode::NotConfigured);
        let mut c = Credentials { api_key: Some("id".into()), base_url: None, extra: vec![("client_secret".into(), "s".into()), ("environment".into(), "mars".into())] };
        assert_eq!(Corti.transcribe(&audio(), &opts("m"), &c).unwrap_err().code, SttErrorCode::BadRequest);
        c.extra = vec![("client_secret".into(), "s".into()), ("tenant".into(), "a/b".into())];
        assert_eq!(Corti.transcribe(&audio(), &opts("m"), &c).unwrap_err().code, SttErrorCode::BadRequest);
        let c = Credentials { api_key: Some("id".into()), base_url: None, extra: vec![("client_secret".into(), "s".into())] };
        assert_eq!(Corti.prewarm_url(&c).as_deref(), Some("https://api.us.corti.app/v2"));
    }
}
