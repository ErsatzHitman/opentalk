//! AWS Bedrock Converse: `POST https://bedrock-runtime.{region}.amazonaws.com/model/{id}/converse`
//! signed with a hand-rolled SigV4 (sha2 + hmac, no AWS SDK). Credentials come from the secrets
//! `aws_access_key_id`, `aws_secret_access_key` and optionally `aws_session_token`.
use super::{Deadline, authority, http_error, max_tokens_for, net_err, secret, uri_encode};
use crate::core::config::Config;
use crate::core::http;
use crate::llm::LlmError;
use hmac::{Hmac, KeyInit, Mac};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

pub(super) fn complete(cfg: &Config, system: &str, user: &str, deadline: &Deadline, base: Option<&str>) -> Result<String, LlmError> {
    let region = cfg.ai_cleanup.bedrock_region.trim();
    let model = cfg.llm_model();
    let valid_region = !region.is_empty() && region.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
    if !valid_region || model.trim().is_empty() {
        return Err(LlmError::NotConfigured);
    }
    let access = secret("aws_access_key_id").ok_or(LlmError::NotConfigured)?;
    let secret_key = secret("aws_secret_access_key").ok_or(LlmError::NotConfigured)?;
    let token = secret("aws_session_token");
    let origin = base.map_or_else(|| format!("https://bedrock-runtime.{region}.amazonaws.com"), str::to_string);
    let creds = Creds { access_key: &access, secret_key: &secret_key, session_token: token.as_deref() };
    run(&origin, region, model.trim(), &creds, now_secs(), system, user, deadline)
}

pub(crate) struct Creds<'a> {
    pub access_key: &'a str,
    pub secret_key: &'a str,
    pub session_token: Option<&'a str>,
}

#[allow(clippy::too_many_arguments)]
fn run(origin: &str, region: &str, model: &str, creds: &Creds, unix_secs: u64, system: &str, user: &str, deadline: &Deadline) -> Result<String, LlmError> {
    let path = format!("/model/{}/converse", uri_encode(model));
    let url = format!("{origin}{path}");
    let body = serde_json::to_vec(&json!({
        "system": [{ "text": system }],
        "messages": [{ "role": "user", "content": [{ "text": user }] }],
        "inferenceConfig": { "temperature": 0, "maxTokens": max_tokens_for(user) },
    }))
    .map_err(|e| LlmError::Network(e.to_string()))?;

    let date = amz_date(unix_secs);
    let mut headers = vec![
        ("content-type".to_string(), "application/json".to_string()),
        ("host".to_string(), authority(&url).to_string()),
        ("x-amz-date".to_string(), date.clone()),
    ];
    if let Some(t) = creds.session_token {
        headers.push(("x-amz-security-token".to_string(), t.to_string()));
    }
    let signed = sign("POST", &path, "", &headers, &body, region, "bedrock", &date, creds);

    let mut send: Vec<(&str, &str)> = vec![("x-amz-date", &date), ("authorization", &signed.authorization)];
    if let Some(t) = creds.session_token {
        send.push(("x-amz-security-token", t));
    }
    let resp = http::post(&url, &send, "application/json", &body, deadline.remaining()?).map_err(net_err)?;
    if !resp.ok() {
        return Err(http_error(resp.status, &resp.body));
    }
    let v = resp.json().map_err(|e| LlmError::InvalidOutput(format!("bad JSON: {e}")))?;
    extract(&v)
}

/// Concatenated text blocks of `output.message.content`.
fn extract(v: &Value) -> Result<String, LlmError> {
    let text = v
        .pointer("/output/message/content")
        .and_then(Value::as_array)
        .map(|blocks| blocks.iter().filter_map(|b| b.get("text").and_then(Value::as_str)).collect::<String>())
        .unwrap_or_default();
    if text.trim().is_empty() {
        return Err(LlmError::InvalidOutput("empty completion".into()));
    }
    Ok(text)
}

// ---------------------------------------------------------------------------------------------
// SigV4
// ---------------------------------------------------------------------------------------------

/// Everything SigV4 produces; tests compare each intermediate against the AWS test suite.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Signed {
    pub canonical_request: String,
    pub string_to_sign: String,
    pub signature: String,
    pub signed_headers: String,
    /// Value for the `Authorization` header.
    pub authorization: String,
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

fn hmac_sha256(key: &[u8], data: &[u8]) -> Vec<u8> {
    // HMAC accepts keys of any length, so `new_from_slice` cannot fail.
    match <Hmac<Sha256> as KeyInit>::new_from_slice(key) {
        Ok(mut m) => {
            m.update(data);
            m.finalize().into_bytes().to_vec()
        }
        Err(_) => Vec::new(),
    }
}

/// Decode `%XX` escapes (and leave malformed ones as-is); `+` is not special in SigV4 queries.
fn uri_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let hexv = |c: u8| (c as char).to_digit(16);
        if b[i] == b'%' && i + 2 < b.len() && let (Some(hi), Some(lo)) = (hexv(b[i + 1]), hexv(b[i + 2])) {
            let v = (hi * 16 + lo) as u8;
            out.push(v);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Canonical URI: every path segment of the *as-sent* path is encoded once more (non-S3 rule).
fn canonical_uri(path: &str) -> String {
    if path.is_empty() {
        return "/".into();
    }
    path.split('/').map(uri_encode).collect::<Vec<_>>().join("/")
}

/// Canonical query: decode, re-encode, sort by name then value.
fn canonical_query(query: &str) -> String {
    let mut pairs: Vec<(String, String)> = query
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            (uri_encode(&uri_decode(k)), uri_encode(&uri_decode(v)))
        })
        .collect();
    pairs.sort();
    pairs.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("&")
}

/// Sign a request. `headers` must hold every header to be signed (at least `host` and
/// `x-amz-date`; `x-amz-security-token` when using temporary credentials). `path` is the
/// request path exactly as sent on the wire.
#[allow(clippy::too_many_arguments)]
pub(crate) fn sign(
    method: &str,
    path: &str,
    query: &str,
    headers: &[(String, String)],
    body: &[u8],
    region: &str,
    service: &str,
    amz_date: &str,
    creds: &Creds,
) -> Signed {
    let mut hdrs: Vec<(String, String)> = headers
        .iter()
        .map(|(k, v)| (k.to_ascii_lowercase(), v.split_whitespace().collect::<Vec<_>>().join(" ")))
        .collect();
    hdrs.sort();
    let canonical_headers: String = hdrs.iter().map(|(k, v)| format!("{k}:{v}\n")).collect();
    let signed_headers = hdrs.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>().join(";");

    let canonical_request = format!(
        "{method}\n{}\n{}\n{canonical_headers}\n{signed_headers}\n{}",
        canonical_uri(path),
        canonical_query(query),
        hex(&Sha256::digest(body)),
    );

    let day = &amz_date[..8.min(amz_date.len())];
    let scope = format!("{day}/{region}/{service}/aws4_request");
    let string_to_sign = format!("AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}", hex(&Sha256::digest(canonical_request.as_bytes())));

    let k_date = hmac_sha256(format!("AWS4{}", creds.secret_key).as_bytes(), day.as_bytes());
    let k_region = hmac_sha256(&k_date, region.as_bytes());
    let k_service = hmac_sha256(&k_region, service.as_bytes());
    let k_signing = hmac_sha256(&k_service, b"aws4_request");
    let signature = hex(&hmac_sha256(&k_signing, string_to_sign.as_bytes()));

    let authorization = format!(
        "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed_headers}, Signature={signature}",
        creds.access_key
    );
    Signed { canonical_request, string_to_sign, signature, signed_headers, authorization }
}

fn now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// `YYYYMMDDTHHMMSSZ` for a unix timestamp (civil-from-days, proleptic Gregorian).
pub(crate) fn amz_date(unix_secs: u64) -> String {
    let days = (unix_secs / 86_400) as i64;
    let rem = unix_secs % 86_400;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}{month:02}{day:02}T{:02}{:02}{:02}Z", rem / 3600, rem % 3600 / 60, rem % 60)
}

#[cfg(test)]
mod tests {
    use super::super::complete_with;
    use super::super::testutil::{cfg, serve};
    use super::*;
    use crate::core::config::LlmProviderId;

    // aws-sig-v4-test-suite credentials / scope.
    const AK: &str = "AKIDEXAMPLE";
    const SK: &str = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY";
    const DATE: &str = "20150830T123600Z";

    fn creds() -> Creds<'static> {
        Creds { access_key: AK, secret_key: SK, session_token: None }
    }
    fn h(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn amz_date_formats() {
        assert_eq!(amz_date(1_440_938_160), DATE);
        assert_eq!(amz_date(0), "19700101T000000Z");
        assert_eq!(amz_date(1_709_164_799), "20240228T235959Z");
        assert_eq!(amz_date(1_709_251_200), "20240301T000000Z"); // 2024 is a leap year
    }

    /// aws-sig-v4-test-suite `get-vanilla`.
    #[test]
    fn suite_get_vanilla() {
        let s = sign("GET", "/", "", &h(&[("Host", "example.amazonaws.com"), ("X-Amz-Date", DATE)]), b"", "us-east-1", "service", DATE, &creds());
        assert_eq!(
            s.canonical_request,
            "GET\n/\n\nhost:example.amazonaws.com\nx-amz-date:20150830T123600Z\n\nhost;x-amz-date\ne3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            s.string_to_sign,
            "AWS4-HMAC-SHA256\n20150830T123600Z\n20150830/us-east-1/service/aws4_request\nbb579772317eb040ac9ed261061d46c1f17a8133879d6129b6e1c25292927e63"
        );
        assert_eq!(s.signature, "5fa00fa31553b73ebf1942676e86291e8372ff2a2260956d9b8aae1d763fbf31");
        assert_eq!(
            s.authorization,
            "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/service/aws4_request, SignedHeaders=host;x-amz-date, Signature=5fa00fa31553b73ebf1942676e86291e8372ff2a2260956d9b8aae1d763fbf31"
        );
    }

    /// aws-sig-v4-test-suite `get-vanilla-query-order-key-case` (query params get sorted).
    #[test]
    fn suite_get_query_order() {
        let s = sign(
            "GET",
            "/",
            "Param2=value2&Param1=value1",
            &h(&[("Host", "example.amazonaws.com"), ("X-Amz-Date", DATE)]),
            b"",
            "us-east-1",
            "service",
            DATE,
            &creds(),
        );
        assert!(s.canonical_request.starts_with("GET\n/\nParam1=value1&Param2=value2\n"));
        assert_eq!(s.signature, "b97d918cfa904a5beff61c982a1b6f458b799221646efd99d3219ec94cdf2500");
    }

    /// aws-sig-v4-test-suite `post-x-www-form-urlencoded` (body hash + content-type signed).
    #[test]
    fn suite_post_form() {
        let s = sign(
            "POST",
            "/",
            "",
            &h(&[("Content-Type", "application/x-www-form-urlencoded"), ("Host", "example.amazonaws.com"), ("X-Amz-Date", DATE)]),
            b"Param1=value1",
            "us-east-1",
            "service",
            DATE,
            &creds(),
        );
        assert_eq!(s.signed_headers, "content-type;host;x-amz-date");
        assert_eq!(s.signature, "ff11897932ad3f4e8b18135d722051e5ac45fc38421b1da7b9d196a0fe09473a");
        assert_eq!(
            s.authorization,
            "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/service/aws4_request, SignedHeaders=content-type;host;x-amz-date, Signature=ff11897932ad3f4e8b18135d722051e5ac45fc38421b1da7b9d196a0fe09473a"
        );
    }

    /// Header values are trimmed and inner whitespace collapsed (suite `get-header-value-trim`).
    #[test]
    fn header_values_are_normalized() {
        let s = sign("GET", "/", "", &h(&[("Host", "example.amazonaws.com"), ("My-Header1", "  value1  "), ("My-Header2", "\"a   b   c\""), ("X-Amz-Date", DATE)]), b"", "us-east-1", "service", DATE, &creds());
        assert!(s.canonical_request.contains("my-header1:value1\nmy-header2:\"a b c\"\n"));
    }

    #[test]
    fn path_is_double_encoded() {
        assert_eq!(canonical_uri("/model/anthropic.claude-v2%3A1/converse"), "/model/anthropic.claude-v2%253A1/converse");
        assert_eq!(canonical_uri(""), "/");
        assert_eq!(canonical_query("a=%2F&b=c%20d"), "a=%2F&b=c%20d");
    }

    /// Bedrock-shaped request; expected values computed independently with Python hmac/hashlib:
    /// POST /model/anthropic.claude-3-haiku-20240307-v1%3A0/converse, us-east-1, service bedrock,
    /// 20240115T103000Z, session token present.
    #[test]
    fn bedrock_shaped_signature() {
        let body = br#"{"messages":[{"role":"user","content":[{"text":"hi"}]}]}"#;
        let path = format!("/model/{}/converse", uri_encode("anthropic.claude-3-haiku-20240307-v1:0"));
        assert_eq!(path, "/model/anthropic.claude-3-haiku-20240307-v1%3A0/converse");
        let c = Creds { access_key: AK, secret_key: SK, session_token: Some("SESSIONTOKEN/abc+def==") };
        let s = sign(
            "POST",
            &path,
            "",
            &h(&[
                ("content-type", "application/json"),
                ("host", "bedrock-runtime.us-east-1.amazonaws.com"),
                ("x-amz-date", "20240115T103000Z"),
                ("x-amz-security-token", "SESSIONTOKEN/abc+def=="),
            ]),
            body,
            "us-east-1",
            "bedrock",
            "20240115T103000Z",
            &c,
        );
        assert_eq!(s.signed_headers, "content-type;host;x-amz-date;x-amz-security-token");
        assert_eq!(s.signature, "57d7cbfbdde32ba30d4436096b202b8798dc50470fc573ecc03564d5b04047ed");
    }

    const CONVERSE_OK: &str = r#"{"output":{"message":{"role":"assistant","content":[{"text":"Hello, "},{"text":"world."}]}},"stopReason":"end_turn"}"#;

    /// Mock Bedrock: re-derives the signature from what actually arrived on the wire.
    #[test]
    fn mock_server_request_is_correctly_signed() {
        let mock = serve(|r| {
            let host = r.header("host").unwrap_or("").to_string();
            let date = r.header("x-amz-date").unwrap_or("").to_string();
            let path = r.target.clone();
            let mut hs = vec![("content-type".to_string(), r.header("content-type").unwrap_or("").to_string()), ("host".into(), host), ("x-amz-date".into(), date.clone())];
            let tok = r.header("x-amz-security-token").map(str::to_string);
            if let Some(t) = &tok {
                hs.push(("x-amz-security-token".into(), t.clone()));
            }
            let c = Creds { access_key: "AKIATEST", secret_key: "SECRETTEST", session_token: tok.as_deref() };
            let expect = sign("POST", &path, "", &hs, r.body.as_bytes(), "eu-west-1", "bedrock", &date, &c);
            if r.header("authorization") == Some(expect.authorization.as_str()) { (200, CONVERSE_OK.into()) } else { (403, r#"{"message":"The request signature we calculated does not match"}"#.into()) }
        });
        let c = Creds { access_key: "AKIATEST", secret_key: "SECRETTEST", session_token: Some("TOK") };
        let out = run(&mock.base, "eu-west-1", "anthropic.claude-haiku-4-5:0", &c, 1_700_000_000, "SYS", "USR", &Deadline::new(5000)).unwrap();
        assert_eq!(out, "Hello, world.");
        let reqs = mock.reqs.lock().unwrap();
        let r = &reqs[0];
        assert_eq!(r.method, "POST");
        assert_eq!(r.target, "/model/anthropic.claude-haiku-4-5%3A0/converse");
        assert_eq!(r.header("x-amz-date"), Some("20231114T221320Z"));
        assert_eq!(r.header("x-amz-security-token"), Some("TOK"));
        assert!(r.header("authorization").unwrap().starts_with("AWS4-HMAC-SHA256 Credential=AKIATEST/20231114/eu-west-1/bedrock/aws4_request, SignedHeaders=content-type;host;x-amz-date;x-amz-security-token, Signature="));
        let b: Value = serde_json::from_str(&r.body).unwrap();
        assert_eq!(b["system"], json!([{"text": "SYS"}]));
        assert_eq!(b["messages"], json!([{"role": "user", "content": [{"text": "USR"}]}]));
        assert_eq!(b["inferenceConfig"]["temperature"], json!(0));
        assert_eq!(b["inferenceConfig"]["maxTokens"], json!(256));
    }

    #[test]
    fn no_session_token_header_when_absent() {
        let mock = serve(|_| (200, CONVERSE_OK.into()));
        run(&mock.base, "us-east-1", "m", &creds(), 0, "s", "u", &Deadline::new(5000)).unwrap();
        let reqs = mock.reqs.lock().unwrap();
        assert!(reqs[0].header("x-amz-security-token").is_none());
        assert!(reqs[0].header("authorization").unwrap().contains("SignedHeaders=content-type;host;x-amz-date,"));
    }

    #[test]
    fn errors_map() {
        let mock = serve(|_| (403, r#"{"message":"The security token included in the request is invalid."}"#.into()));
        let e = run(&mock.base, "us-east-1", "m", &creds(), 0, "s", "u", &Deadline::new(5000)).unwrap_err();
        assert_eq!(e, LlmError::Http { status: 403, detail: "The security token included in the request is invalid.".into() });
        let mock = serve(|_| (200, r#"{"output":{"message":{"content":[]}}}"#.into()));
        assert!(matches!(run(&mock.base, "us-east-1", "m", &creds(), 0, "s", "u", &Deadline::new(5000)), Err(LlmError::InvalidOutput(_))));
    }

    #[test]
    fn config_validation() {
        let mut c = cfg(LlmProviderId::Bedrock);
        c.ai_cleanup.bedrock_region = "us-east-1.evil.com/".into();
        assert_eq!(complete_with(&c, "s", "u", 1000, None), Err(LlmError::NotConfigured));
        c.ai_cleanup.bedrock_region = String::new();
        assert_eq!(complete_with(&c, "s", "u", 1000, None), Err(LlmError::NotConfigured));
    }
}
