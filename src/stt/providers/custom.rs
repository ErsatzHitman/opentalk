//! Custom OpenAI-compatible endpoint (self-hosted servers, gateways, Azure OpenAI).
//!
//! The base URL is normalised like OpenWhispr `normalizeBaseUrl` (API-path suffixes stripped,
//! query/hash kept aside). Azure hosts (`*.openai.azure.com`, `*.cognitiveservices.azure.com`,
//! `*.services.ai.azure.com`) route by deployment — the model name — and authenticate with
//! an `api-key` header:
//! `{origin}/openai/deployments/{model}/audio/transcriptions?api-version=2024-10-21`.
use super::openai::{Auth, Request, transcribe_multipart};
use super::{model_or, percent_encode, prompt_limit};
use crate::core::config::SttProviderId;
use crate::stt::errors::{SELF_HOSTED_NAME, SttError, SttErrorCode};
use crate::stt::{Audio, BatchProvider, Credentials, SttOptions, Transcript};

pub const DEFAULT_MODEL: &str = "whisper-1";
pub const DEFAULT_AZURE_API_VERSION: &str = "2024-10-21";

const AZURE_HOST_SUFFIXES: [&str; 3] = [".openai.azure.com", ".cognitiveservices.azure.com", ".services.ai.azure.com"];

pub struct Custom;

impl BatchProvider for Custom {
    fn id(&self) -> SttProviderId {
        SttProviderId::Custom
    }

    fn prewarm_url(&self, creds: &Credentials) -> Option<String> {
        // Same scheme defaulting and http-only-on-private-hosts rule as `transcribe`.
        let raw = creds.base_url.as_deref().map(str::trim).filter(|b| !b.is_empty())?;
        let base = normalize_base_url(&with_https_scheme(raw));
        (!base.is_empty() && is_secure_endpoint(&base)).then_some(base)
    }

    fn transcribe(&self, audio: &Audio, opts: &SttOptions, creds: &Credentials) -> Result<Transcript, SttError> {
        let (url, azure) = resolve_endpoint(creds.base_url.as_deref().unwrap_or_default(), model_or(opts, DEFAULT_MODEL))?;
        let model = model_or(opts, DEFAULT_MODEL);
        let key = creds.api_key.as_deref().map(str::trim).filter(|k| !k.is_empty());
        let auth = match key {
            Some(k) if azure => Auth::ApiKey(k),
            Some(k) => Auth::Bearer(k),
            None => Auth::None,
        };
        transcribe_multipart(
            audio,
            opts,
            &Request {
                name: SELF_HOSTED_NAME,
                self_hosted: true,
                url: &url,
                auth,
                model,
                prompt_limit: prompt_limit(url.contains("api.groq.com"), model),
                stream: false,
                keywords: false,
            },
        )
    }
}

/// Transcription URL for the configured base (and whether it is an Azure endpoint).
fn resolve_endpoint(configured: &str, model: &str) -> Result<(String, bool), SttError> {
    let raw = configured.trim();
    if raw.is_empty() {
        return Err(SttError::new(SttErrorCode::NotConfigured, "No server URL is set for the custom endpoint. Add it in Settings."));
    }
    let raw = with_https_scheme(raw);
    let base = normalize_base_url(&raw);
    if base.is_empty() || !is_secure_endpoint(&base) {
        return Err(SttError::new(SttErrorCode::NotConfigured, "The custom transcription endpoint is invalid or unsupported."));
    }
    let azure = is_azure_endpoint(&base);
    let url = if azure {
        azure_transcription_url(&raw, model).unwrap_or_else(|| build_api_url(&base, "/audio/transcriptions"))
    } else {
        build_api_url(&base, "/audio/transcriptions")
    };
    Ok((url, azure))
}

// ---- URL helpers (ports of config/constants.ts and utils/urlUtils.ts) ----

/// Split `url` into (path-and-origin, `?query`, `#hash`).
fn split_decorators(url: &str) -> (&str, &str, &str) {
    let (rest, hash) = url.find('#').map_or((url, ""), |i| (&url[..i], &url[i..]));
    let (path, query) = rest.find('?').map_or((rest, ""), |i| (&rest[..i], &rest[i..]));
    (path, query, hash)
}

/// Strip well-known API suffixes (`/v1/chat/completions`, `/audio/transcriptions`, ...) and
/// trailing slashes, keeping any query string.
pub fn normalize_base_url(value: &str) -> String {
    let value = value.trim();
    if value.is_empty() {
        return String::new();
    }
    const SUFFIXES: [(&str, &str); 10] = [
        ("/v1/chat/completions", "/v1"),
        ("/chat/completions", ""),
        ("/v1/responses", "/v1"),
        ("/responses", ""),
        ("/v1/models", "/v1"),
        ("/models", ""),
        ("/v1/audio/transcriptions", "/v1"),
        ("/audio/transcriptions", ""),
        ("/v1/audio/translations", "/v1"),
        ("/audio/translations", ""),
    ];
    let (path, query, hash) = split_decorators(value);
    let mut path = path.trim_end_matches('/').to_string();
    for (suffix, replacement) in SUFFIXES {
        if path.to_ascii_lowercase().ends_with(suffix) {
            path.truncate(path.len() - suffix.len());
            path.push_str(replacement);
            path = path.trim_end_matches('/').to_string();
        }
    }
    format!("{}{query}{hash}", path.trim_end_matches('/'))
}

fn build_api_url(base: &str, path: &str) -> String {
    let (origin_path, query, hash) = split_decorators(base);
    format!("{origin_path}{path}{query}{hash}")
}

fn with_https_scheme(url: &str) -> String {
    if url.contains("://") { url.to_string() } else { format!("https://{url}") }
}

struct Parsed<'a> {
    scheme: &'a str,
    host: String,
    origin: &'a str,
    path: &'a str,
    query: &'a str,
}

fn parse_url(url: &str) -> Option<Parsed<'_>> {
    let (rest, _) = url.split_once('#').unwrap_or((url, ""));
    let (rest, query) = rest.split_once('?').map_or((rest, ""), |(a, b)| (a, b));
    let (scheme, after) = rest.split_once("://")?;
    let auth_end = after.find('/').unwrap_or(after.len());
    let authority = &after[..auth_end];
    let hostport = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let host = if let Some(v6) = hostport.strip_prefix('[') {
        v6.split(']').next()?.to_string()
    } else {
        hostport.split(':').next()?.to_string()
    };
    if host.is_empty() || !matches!(scheme, "http" | "https") {
        return None;
    }
    let origin_len = scheme.len() + 3 + auth_end;
    Some(Parsed { scheme, host: host.to_ascii_lowercase(), origin: &rest[..origin_len], path: &after[auth_end..], query })
}

pub fn is_azure_endpoint(url: &str) -> bool {
    parse_url(url).is_some_and(|p| AZURE_HOST_SUFFIXES.iter().any(|s| p.host.ends_with(s)))
}

/// Deployment-style Azure URL; an already complete `.../audio/transcriptions` URL is kept.
fn azure_transcription_url(raw: &str, deployment: &str) -> Option<String> {
    let p = parse_url(raw)?;
    let path = p.path.trim_end_matches('/').to_ascii_lowercase();
    if path.ends_with("/audio/transcriptions") || path.ends_with("/audio/translations") {
        return Some(raw.split('#').next().unwrap_or(raw).to_string());
    }
    let name = deployment.trim();
    if name.is_empty() {
        return None;
    }
    let version = p
        .query
        .split('&')
        .find_map(|kv| kv.strip_prefix("api-version="))
        .filter(|v| !v.is_empty())
        .unwrap_or(DEFAULT_AZURE_API_VERSION);
    Some(format!("{}/openai/deployments/{}/audio/transcriptions?api-version={}", p.origin, percent_encode(name), version))
}

/// https, or http to a private/loopback host (OpenWhispr `isSecureHttpEndpoint`).
fn is_secure_endpoint(url: &str) -> bool {
    parse_url(url).is_some_and(|p| p.scheme == "https" || is_private_host(&p.host))
}

fn is_private_host(h: &str) -> bool {
    if h == "localhost" || h == "0.0.0.0" || h == "::1" || h.ends_with(".local") || h.ends_with(".ts.net") {
        return true;
    }
    let octets: Vec<u8> = h.split('.').filter_map(|p| p.parse().ok()).collect();
    if h.split('.').count() == 4 && octets.len() == 4 {
        let (a, b) = (octets[0], octets[1]);
        return a == 127 || a == 10 || (a == 192 && b == 168) || (a == 172 && (16..=31).contains(&b)) || (a == 100 && (64..=127).contains(&b)) || (a == 169 && b == 254);
    }
    h.contains(':') && (h.starts_with("fc") || h.starts_with("fd") || ["fe8", "fe9", "fea", "feb"].iter().any(|p| h.starts_with(p)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stt::providers::testutil::*;

    #[test]
    fn prewarm_url_defaults_scheme_and_skips_insecure_hosts() {
        let c = |u: &str| Credentials { api_key: None, base_url: Some(u.into()), extra: vec![] };
        assert_eq!(Custom.prewarm_url(&c("contoso.openai.azure.com/")).as_deref(), Some("https://contoso.openai.azure.com"));
        assert_eq!(Custom.prewarm_url(&c("http://127.0.0.1:8080/v1/audio/transcriptions")).as_deref(), Some("http://127.0.0.1:8080/v1"));
        assert_eq!(Custom.prewarm_url(&c("http://stt.example.com/v1")), None);
        assert_eq!(Custom.prewarm_url(&c("  ")), None);
        assert_eq!(Custom.prewarm_url(&Credentials::default()), None);
    }

    #[test]
    fn normalizes_like_openwhispr() {
        for (input, want) in [
            ("https://api.example.com/v1/audio/transcriptions", "https://api.example.com/v1"),
            ("https://api.example.com/audio/transcriptions/", "https://api.example.com"),
            ("https://api.example.com/v1/chat/completions", "https://api.example.com/v1"),
            ("https://api.example.com/v1/", "https://api.example.com/v1"),
            ("https://api.example.com/V1/Models", "https://api.example.com/v1"),
            ("https://x.openai.azure.com/openai/v1/audio/transcriptions?api-version=2024-10-21", "https://x.openai.azure.com/openai/v1?api-version=2024-10-21"),
            ("  ", ""),
        ] {
            assert_eq!(normalize_base_url(input), want, "{input}");
        }
        assert_eq!(build_api_url("https://h/v1?a=b", "/audio/transcriptions"), "https://h/v1/audio/transcriptions?a=b");
    }

    #[test]
    fn azure_detection_and_urls() {
        assert!(is_azure_endpoint("https://my-res.openai.azure.com/"));
        assert!(is_azure_endpoint("https://my-res.cognitiveservices.azure.com"));
        assert!(!is_azure_endpoint("https://openai.azure.com.evil.example/"));
        assert!(!is_azure_endpoint("https://api.openai.com/v1"));
        assert_eq!(
            azure_transcription_url("https://my-res.openai.azure.com/", "my deploy").unwrap(),
            "https://my-res.openai.azure.com/openai/deployments/my%20deploy/audio/transcriptions?api-version=2024-10-21"
        );
        assert_eq!(
            azure_transcription_url("https://my-res.openai.azure.com/?api-version=2025-01-01-preview", "w").unwrap(),
            "https://my-res.openai.azure.com/openai/deployments/w/audio/transcriptions?api-version=2025-01-01-preview"
        );
        let full = "https://my-res.openai.azure.com/openai/deployments/w/audio/transcriptions?api-version=2024-06-01";
        assert_eq!(azure_transcription_url(full, "other").unwrap(), full);
        assert!(azure_transcription_url("https://my-res.openai.azure.com", " ").is_none());
    }

    #[test]
    fn azure_resolves_to_a_deployment_url() {
        let (url, azure) = resolve_endpoint("my-res.openai.azure.com", "whisper").unwrap();
        assert!(azure);
        assert_eq!(url, "https://my-res.openai.azure.com/openai/deployments/whisper/audio/transcriptions?api-version=2024-10-21");
        let (url, azure) = resolve_endpoint("https://api.example.com/v1/", "m").unwrap();
        assert!(!azure);
        assert_eq!(url, "https://api.example.com/v1/audio/transcriptions");
    }

    #[test]
    fn azure_authenticates_with_the_api_key_header() {
        let mock = serve(vec![Resp::json(200, r#"{"text":"azure ok"}"#)]);
        let req = Request {
            name: SELF_HOSTED_NAME,
            self_hosted: true,
            url: &format!("{}/openai/deployments/w/audio/transcriptions?api-version=2024-10-21", mock.url),
            auth: Auth::ApiKey("azure-key-1"),
            model: "w",
            prompt_limit: 900,
            stream: false,
            keywords: false,
        };
        assert_eq!(transcribe_multipart(&audio(), &opts("w"), &req).unwrap().text, "azure ok");
        let r = mock.one();
        assert_eq!(r.path, "/openai/deployments/w/audio/transcriptions?api-version=2024-10-21");
        assert_eq!(r.header("api-key"), Some("azure-key-1"));
        assert!(r.header("authorization").is_none());
    }

    #[test]
    fn secure_endpoints() {
        assert!(is_secure_endpoint("https://example.com/v1"));
        assert!(is_secure_endpoint("http://127.0.0.1:8080/v1"));
        assert!(is_secure_endpoint("http://192.168.1.5/v1"));
        assert!(is_secure_endpoint("http://localhost:9000"));
        assert!(!is_secure_endpoint("http://example.com/v1"));
        assert!(!is_secure_endpoint("http://127.example.com"));
        assert!(!is_secure_endpoint("ftp://example.com"));
        assert!(!is_secure_endpoint("not a url"));
    }

    #[test]
    fn generic_server_request() {
        let mock = serve(vec![Resp::json(200, r#"{"text":"from my server"}"#)]);
        // Users often paste the full endpoint.
        let c = Credentials { api_key: Some("local-key-1".into()), base_url: Some(format!("{}/v1/audio/transcriptions", mock.url)), extra: vec![] };
        let mut o = opts("my-whisper");
        o.language = Some("es".into());
        o.hints = vec!["Priya".into()];
        let t = Custom.transcribe(&audio(), &o, &c).unwrap();
        assert_eq!(t.text, "from my server");
        let r = mock.one();
        assert_eq!((r.method.as_str(), r.path.as_str()), ("POST", "/v1/audio/transcriptions"));
        assert_eq!(r.header("authorization"), Some("Bearer local-key-1"));
        assert!(r.header("api-key").is_none());
        let parts = r.multipart();
        assert_eq!(parts.names(), ["file", "model", "language", "prompt"]);
        assert_eq!(parts.text("model"), ["my-whisper"]);
    }

    #[test]
    fn keyless_server_gets_no_auth_header() {
        let mock = serve(vec![Resp::json(200, r#"{"text":"ok"}"#)]);
        let c = Credentials { api_key: None, base_url: Some(mock.url.clone()), extra: vec![] };
        Custom.transcribe(&audio(), &opts(""), &c).unwrap();
        let r = mock.one();
        assert_eq!(r.path, "/audio/transcriptions");
        assert!(r.header("authorization").is_none() && r.header("api-key").is_none());
        assert_eq!(r.multipart().text("model"), ["whisper-1"]);
    }

    #[test]
    fn missing_or_insecure_url_is_not_configured() {
        for base in [None, Some("  ".to_string()), Some("http://example.com/v1".to_string())] {
            let c = Credentials { api_key: Some("k".into()), base_url: base, extra: vec![] };
            let e = Custom.transcribe(&audio(), &opts(""), &c).unwrap_err();
            assert_eq!(e.code, SttErrorCode::NotConfigured);
        }
    }

    #[test]
    fn errors_use_self_hosted_wording() {
        let mock = serve(vec![Resp::json(401, "{}")]);
        let c = creds(&mock, "local-key-1");
        let e = Custom.transcribe(&audio(), &opts(""), &c).unwrap_err();
        assert_eq!((e.code, e.message.as_str()), (SttErrorCode::AuthFailed, "Your server rejected the API key."));
        let mock = serve(vec![Resp::text(404, "Not Found")]);
        let e = Custom.transcribe(&audio(), &opts(""), &creds(&mock, "k")).unwrap_err();
        assert_eq!((e.code, e.message.as_str()), (SttErrorCode::Other, "Your server returned an unexpected error."));
    }
}
