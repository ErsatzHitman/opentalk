//! Google Vertex AI (Gemini via `generateContent`).
//!
//! Auth: the service-account JSON (secret `vertex_service_account_json`) signs an RS256 JWT
//! (pure-Rust `rsa`, ~10 small RustCrypto crates and no C code; hand-rolling RSA would be
//! worse), which is exchanged at `https://oauth2.googleapis.com/token` for an access token.
//! The token is cached until `expires_in - 60 s`, so a dictation costs one API call.
use super::{Deadline, http_error, max_tokens_for, net_err, secret, uri_encode};
use crate::core::config::Config;
use crate::core::http;
use crate::llm::LlmError;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rsa::pkcs8::DecodePrivateKey;
use rsa::{Pkcs1v15Sign, RsaPrivateKey};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::sync::Mutex;
use std::time::{Duration, Instant};

const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const SCOPE: &str = "https://www.googleapis.com/auth/cloud-platform";
/// DER `DigestInfo` prefix for SHA-256 (RFC 8017 section 9.2); lets us reuse the workspace sha2.
const SHA256_DIGEST_INFO: [u8; 19] = [0x30, 0x31, 0x30, 0x0d, 0x06, 0x09, 0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x01, 0x05, 0x00, 0x04, 0x20];

#[derive(Deserialize)]
struct ServiceAccount {
    client_email: String,
    private_key: String,
    #[serde(default)]
    project_id: String,
}

struct Cached {
    /// `token_url|client_email` the token belongs to.
    key: String,
    token: String,
    valid_until: Instant,
}

static TOKEN: Mutex<Option<Cached>> = Mutex::new(None);

pub(super) fn complete(cfg: &Config, system: &str, user: &str, deadline: &Deadline, base: Option<&str>) -> Result<String, LlmError> {
    let ai = &cfg.ai_cleanup;
    let json = secret("vertex_service_account_json").ok_or(LlmError::NotConfigured)?;
    let sa: ServiceAccount = serde_json::from_str(&json).map_err(|_| LlmError::NotConfigured)?;
    let project = if ai.vertex_project.trim().is_empty() { sa.project_id.trim().to_string() } else { ai.vertex_project.trim().to_string() };
    let location = if ai.vertex_location.trim().is_empty() { "us-central1" } else { ai.vertex_location.trim() };
    let model = cfg.llm_model();
    let valid_loc = location.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
    if project.is_empty() || model.trim().is_empty() || !valid_loc {
        return Err(LlmError::NotConfigured);
    }
    let (token_url, api_origin) = match base {
        Some(b) => (format!("{b}/token"), b.to_string()),
        None if location == "global" => (TOKEN_URL.to_string(), "https://aiplatform.googleapis.com".to_string()),
        None => (TOKEN_URL.to_string(), format!("https://{location}-aiplatform.googleapis.com")),
    };
    let url = format!(
        "{api_origin}/v1/projects/{}/locations/{location}/publishers/google/models/{}:generateContent",
        uri_encode(&project),
        uri_encode(model.trim())
    );
    run(&sa, &token_url, &url, system, user, deadline)
}

fn run(sa: &ServiceAccount, token_url: &str, url: &str, system: &str, user: &str, deadline: &Deadline) -> Result<String, LlmError> {
    let body = json!({
        "systemInstruction": { "parts": [{ "text": system }] },
        "contents": [{ "role": "user", "parts": [{ "text": user }] }],
        "generationConfig": { "temperature": 0, "maxOutputTokens": max_tokens_for(user) },
    });
    for attempt in 0..2 {
        let token = access_token(sa, token_url, deadline)?;
        let auth = format!("Bearer {token}");
        let resp = http::post_json(url, &[("Authorization", &auth)], &body, deadline.remaining()?).map_err(net_err)?;
        if resp.status == 401 && attempt == 0 {
            // Revoked or expired early: drop the cache and sign in again once.
            if let Ok(mut g) = TOKEN.lock() {
                *g = None;
            }
            continue;
        }
        if !resp.ok() {
            return Err(http_error(resp.status, &resp.body));
        }
        let v = resp.json().map_err(|e| LlmError::InvalidOutput(format!("bad JSON: {e}")))?;
        return extract(&v);
    }
    Err(LlmError::Http { status: 401, detail: "unauthorized".into() })
}

/// Concatenated non-thought text parts of the first candidate.
fn extract(v: &Value) -> Result<String, LlmError> {
    let text = v
        .pointer("/candidates/0/content/parts")
        .and_then(Value::as_array)
        .map(|parts| {
            parts
                .iter()
                .filter(|p| p.get("thought").and_then(Value::as_bool) != Some(true))
                .filter_map(|p| p.get("text").and_then(Value::as_str))
                .collect::<String>()
        })
        .unwrap_or_default();
    if text.trim().is_empty() {
        let why = v.pointer("/candidates/0/finishReason").or_else(|| v.pointer("/promptFeedback/blockReason")).and_then(Value::as_str).unwrap_or("no text");
        return Err(LlmError::InvalidOutput(format!("empty completion ({why})")));
    }
    Ok(text)
}

/// Cached OAuth access token for this service account, refreshed 60 s before expiry.
fn access_token(sa: &ServiceAccount, token_url: &str, deadline: &Deadline) -> Result<String, LlmError> {
    let key = format!("{token_url}|{}", sa.client_email);
    if let Ok(g) = TOKEN.lock() {
        if let Some(c) = g.as_ref() {
            if c.key == key && Instant::now() < c.valid_until {
                return Ok(c.token.clone());
            }
        }
    }
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let jwt = build_jwt(sa, token_url, now)?;
    let form = format!("grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Ajwt-bearer&assertion={jwt}");
    let resp = http::post(token_url, &[], "application/x-www-form-urlencoded", form.as_bytes(), deadline.remaining()?).map_err(net_err)?;
    if !resp.ok() {
        return Err(http_error(resp.status, &resp.body));
    }
    let v = resp.json().map_err(|e| LlmError::InvalidOutput(format!("bad token JSON: {e}")))?;
    let token = v.get("access_token").and_then(Value::as_str).filter(|t| !t.is_empty()).ok_or_else(|| LlmError::InvalidOutput("token response without access_token".into()))?;
    let ttl = v.get("expires_in").and_then(Value::as_u64).unwrap_or(3600).saturating_sub(60);
    if let Ok(mut g) = TOKEN.lock() {
        *g = Some(Cached { key, token: token.to_string(), valid_until: Instant::now() + Duration::from_secs(ttl) });
    }
    Ok(token.to_string())
}

/// `header.claims.signature` (RS256), valid for one hour from `iat`.
fn build_jwt(sa: &ServiceAccount, aud: &str, iat: u64) -> Result<String, LlmError> {
    let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"RS256","typ":"JWT"}"#);
    let claims = json!({ "iss": sa.client_email, "scope": SCOPE, "aud": aud, "iat": iat, "exp": iat + 3600 });
    let signing_input = format!("{header}.{}", URL_SAFE_NO_PAD.encode(claims.to_string()));
    let key = RsaPrivateKey::from_pkcs8_pem(&sa.private_key).map_err(|_| LlmError::NotConfigured)?;
    let scheme = Pkcs1v15Sign { hash_len: Some(32), prefix: SHA256_DIGEST_INFO.to_vec().into_boxed_slice() };
    let sig = key.sign(scheme, &Sha256::digest(signing_input.as_bytes())).map_err(|e| LlmError::Network(format!("jwt signing failed: {e}")))?;
    Ok(format!("{signing_input}.{}", URL_SAFE_NO_PAD.encode(sig)))
}

#[cfg(test)]
mod tests {
    use super::super::testutil::{Req, serve};
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    /// Throwaway 2048-bit key generated for these tests only; it protects nothing.
    const TEST_KEY: &str = "-----BEGIN PRIVATE KEY-----
MIIEvQIBADANBgkqhkiG9w0BAQEFAASCBKcwggSjAgEAAoIBAQDZjjlf7tLguF6x
r8F85JSlLYQ2K7OFP/3uXCpT1Zoir+5CyiOTjGaY2HXifITW4rbwb0hbECrD3Dvh
Ynn+UggAkxkH15pke3verEA8+5ZTBINWcEuWhBV5HUAy16FMu1NU2hKsgpBcojY4
EY7792L+gtEDzdeX4W0INvJc3VNc9lECPqf/jtlUGmBWbZdtBrss4Eol7NS1JfsT
/vkJdrDm+zREdNQhLdXQQx2mxBEs/QaN+kNFPt3EsHOKSUnV6JjliFSbyXHHr55m
pFGJM3gggak0xfBw7OGE1URFdBwHk0iAEorXqme9EaG2x4/KXZ16lhSw66dY33/3
PH+jfBilAgMBAAECggEAZmVYf9yqiHZ+q2crzJrRk8UOm2iKQEdUP2JF0duRho4D
uI/nmsTUbVBEe+tbosBmllfqWGINns4DqRtui9MUTSDvAQ/rbSvpq26rdYh8wmo3
XvHPwfI9IRoxYzy2gKUSnbdeXq+9PrxihJVPFSzk/SGZOw8hUC0b4mfqPuc1G2U9
kvX0zP1jtb2hqN+mMcYbAnfZyddTjtOsShSMFn9StFvwe+T98JUmmKKCyH2UUuC7
qbeXv+YTYBx214e2+nvlnWFgG+Fl1KsRE867TcuQ5xukEHT1yognlRvV6CMSzC9W
1QQ9xyDypywDFWg+Y8EJ7EEYT0NcbKVG/TIOhllbqQKBgQDyi/ozlp5+aBQSyJ9f
pnSTLYqQPIOdoAjZwEhnSI0jCqmMg/fMf3FJ6GPVIjuFM4IETD18sY9tuFcH2Poh
UgD0YJevQNYKG53ywM2TGMxEvdB68niBeFzIcCYgCaVpmQWH2qAZd2JewzKtaoXI
7yDnWT6FlNhjR2hvtVxQpyZzZwKBgQDln2LQxfBurndHJIbEyRMnaWSHr978tDe2
PCcfevAEgoxoQ4FhSMaP8OSEQ6nA+FOQ6vuW07Ue1G4T+Bj5698CtKcaEOzZ1jBU
h/rFLHl4UFieFTZTgScxpx8oLy8xhEv2wTUjQHS0I8KeTjAeGTzUj7KUu85iZM8f
Pe7kZ6U4EwKBgQC9dfwuehQ6V6B+f7qGo+bh5KwdOTfyU8U2Gw4zp6ef3L9OZPOX
CWH8QoxhO4lU5kREiS1qjuSj52AMYEAa8QyP2DgEjNflM2dw7Yxnv/IoLEGDGI5r
TBLl+zy8v+w3DXIVigCPhAMSVNXsQo5Yw/pm3P6KqBdTjb8Vq4x97+OINwKBgE2F
Aooy2Nm04qXM6gaTT3kVto+sSx6LBOfr5NVW3qLXI+mrKImcVRSRIcSZRzzTWPrq
qacgr9vJoaPyNRRikfZK4MMHBb8A9W4EPh/ExPlePpyX0rBcQMwdfIJHc6pG3JJg
Zb0R7JRIIlLpWItAv+2O2Z4mzSUgnWARu422g+urAoGAD+ckfXUnIGIaokHxvdyu
b0elwqZF+TigZMPVqS+c3iSfJDkWqAjFPFNXSDavSBz95WCU1IOLXBGANbiI7rKp
tuA6DaEJZbaG4kEMuPq34zrRqytUpi1rIHR3e7OT/b4stTdD7MyP70j+4pP9JB/X
Erzm2wnHzmRdLqeWvIM8AlM=
-----END PRIVATE KEY-----";

    fn sa(email: &str) -> ServiceAccount {
        ServiceAccount { client_email: email.into(), private_key: TEST_KEY.into(), project_id: "proj".into() }
    }

    fn verify(jwt: &str) -> Value {
        let parts: Vec<&str> = jwt.split('.').collect();
        assert_eq!(parts.len(), 3);
        let key = RsaPrivateKey::from_pkcs8_pem(TEST_KEY).unwrap().to_public_key();
        let scheme = Pkcs1v15Sign { hash_len: Some(32), prefix: SHA256_DIGEST_INFO.to_vec().into_boxed_slice() };
        let sig = URL_SAFE_NO_PAD.decode(parts[2]).unwrap();
        key.verify(scheme, &Sha256::digest(format!("{}.{}", parts[0], parts[1]).as_bytes()), &sig).expect("RS256 signature verifies");
        assert_eq!(URL_SAFE_NO_PAD.decode(parts[0]).unwrap(), br#"{"alg":"RS256","typ":"JWT"}"#);
        serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts[1]).unwrap()).unwrap()
    }

    #[test]
    fn jwt_is_valid_rs256() {
        let jwt = build_jwt(&sa("svc@proj.iam.gserviceaccount.com"), TOKEN_URL, 1_700_000_000).unwrap();
        let c = verify(&jwt);
        assert_eq!(c["iss"], "svc@proj.iam.gserviceaccount.com");
        assert_eq!(c["scope"], SCOPE);
        assert_eq!(c["aud"], TOKEN_URL);
        assert_eq!(c["iat"], 1_700_000_000u64);
        assert_eq!(c["exp"], 1_700_003_600u64);
        // Tampering breaks verification.
        let mut bad = jwt.clone();
        bad.replace_range(bad.len() - 6.., "AAAAAA");
        assert!(std::panic::catch_unwind(|| verify(&bad)).is_err());
    }

    #[test]
    fn bad_key_is_not_configured() {
        let mut s = sa("x@y");
        s.private_key = "not a pem".into();
        assert_eq!(build_jwt(&s, TOKEN_URL, 0), Err(LlmError::NotConfigured));
    }

    const GEN_OK: &str = r#"{"candidates":[{"content":{"role":"model","parts":[{"text":"pondering","thought":true},{"text":"Hello, "},{"text":"world."}]},"finishReason":"STOP"}]}"#;

    /// Token endpoint at `/token`, API at `/v1/...`; counts token exchanges.
    fn vertex_mock(expires_in: u64, token_hits: Arc<AtomicUsize>) -> super::super::testutil::Mock {
        serve(move |r: &Req| {
            if r.target == "/token" {
                token_hits.fetch_add(1, Ordering::SeqCst);
                let n = token_hits.load(Ordering::SeqCst);
                (200, format!(r#"{{"access_token":"tok-{n}","expires_in":{expires_in},"token_type":"Bearer"}}"#))
            } else if r.header("authorization").is_some_and(|a| a.starts_with("Bearer tok-")) {
                (200, GEN_OK.into())
            } else {
                (401, r#"{"error":{"message":"no token"}}"#.into())
            }
        })
    }

    #[test]
    fn token_exchange_request_and_generate_content_shape() {
        let hits = Arc::new(AtomicUsize::new(0));
        let mock = vertex_mock(3600, hits.clone());
        let s = sa("shape@proj.iam.gserviceaccount.com");
        let url = format!("{}/v1/projects/proj/locations/us-central1/publishers/google/models/gemini-3.5-flash-lite:generateContent", mock.base);
        let out = run(&s, &format!("{}/token", mock.base), &url, "SYS", "USR", &Deadline::new(10_000)).unwrap();
        assert_eq!(out, "Hello, world.");

        let reqs = mock.reqs.lock().unwrap();
        assert_eq!(reqs.len(), 2);
        let t = &reqs[0];
        assert_eq!(t.method, "POST");
        assert_eq!(t.header("content-type"), Some("application/x-www-form-urlencoded"));
        let assertion = t.body.strip_prefix("grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Ajwt-bearer&assertion=").expect("jwt-bearer grant");
        let claims = verify(assertion);
        assert_eq!(claims["aud"], format!("{}/token", mock.base));
        assert_eq!(claims["iss"], "shape@proj.iam.gserviceaccount.com");

        let g = &reqs[1];
        assert_eq!(g.target, "/v1/projects/proj/locations/us-central1/publishers/google/models/gemini-3.5-flash-lite:generateContent");
        assert_eq!(g.header("authorization"), Some("Bearer tok-1"));
        let b: Value = serde_json::from_str(&g.body).unwrap();
        assert_eq!(b["systemInstruction"]["parts"][0]["text"], "SYS");
        assert_eq!(b["contents"][0], json!({"role":"user","parts":[{"text":"USR"}]}));
        assert_eq!(b["generationConfig"]["temperature"], json!(0));
    }

    #[test]
    fn token_is_cached_until_expiry_minus_60s() {
        let hits = Arc::new(AtomicUsize::new(0));
        let mock = vertex_mock(3600, hits.clone());
        let s = sa("cache@proj.iam.gserviceaccount.com");
        let url = format!("{}/v1/x:generateContent", mock.base);
        for _ in 0..3 {
            run(&s, &format!("{}/token", mock.base), &url, "s", "u", &Deadline::new(10_000)).unwrap();
        }
        assert_eq!(hits.load(Ordering::SeqCst), 1, "one exchange serves three completions");

        // expires_in <= 60 means the cached token is already stale: every call exchanges again.
        let hits2 = Arc::new(AtomicUsize::new(0));
        let mock2 = vertex_mock(30, hits2.clone());
        let s2 = sa("shortlived@proj.iam.gserviceaccount.com");
        let url2 = format!("{}/v1/x:generateContent", mock2.base);
        for _ in 0..2 {
            run(&s2, &format!("{}/token", mock2.base), &url2, "s", "u", &Deadline::new(10_000)).unwrap();
        }
        assert_eq!(hits2.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn unauthorized_api_call_refreshes_token_once() {
        let hits = Arc::new(AtomicUsize::new(0));
        let h = hits.clone();
        let mock = serve(move |r| {
            if r.target == "/token" {
                let n = h.fetch_add(1, Ordering::SeqCst) + 1;
                (200, format!(r#"{{"access_token":"tok-{n}","expires_in":3600}}"#))
            } else if r.header("authorization") == Some("Bearer tok-2") {
                (200, GEN_OK.into())
            } else {
                (401, r#"{"error":{"message":"expired"}}"#.into())
            }
        });
        let s = sa("refresh@proj.iam.gserviceaccount.com");
        let url = format!("{}/v1/x:generateContent", mock.base);
        assert_eq!(run(&s, &format!("{}/token", mock.base), &url, "s", "u", &Deadline::new(10_000)).unwrap(), "Hello, world.");
        assert_eq!(hits.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn token_endpoint_error_maps() {
        let mock = serve(|_| (400, r#"{"error":"invalid_grant","error_description":"Invalid JWT Signature."}"#.into()));
        let s = sa("bad@proj.iam.gserviceaccount.com");
        let e = run(&s, &format!("{}/token", mock.base), &format!("{}/v1/x:generateContent", mock.base), "s", "u", &Deadline::new(10_000)).unwrap_err();
        assert_eq!(e, LlmError::Http { status: 400, detail: "Invalid JWT Signature.".into() });
    }

    #[test]
    fn extract_reports_blocked_output() {
        let v: Value = serde_json::from_str(r#"{"candidates":[{"finishReason":"SAFETY"}]}"#).unwrap();
        assert!(matches!(extract(&v), Err(LlmError::InvalidOutput(m)) if m.contains("SAFETY")));
    }
}
