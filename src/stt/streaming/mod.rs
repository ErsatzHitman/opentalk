//! Realtime STT over WebSocket (tungstenite + native-tls). Input is 16 kHz mono PCM in
//! 50 ms frames; OpenAI Realtime needs 24 kHz upsampling. See docs/PLAN.md §4a.
//!
//! - [`session`]: generic one-thread-per-session engine (connect, frame, settle, close).
//! - `deepgram`, `assemblyai`, `openai_realtime`, `gemini_live`: one [`session::Protocol`] each.
//! - Corti and Tinfoil have no streaming transport here: `get`/`start` return `None`, so the
//!   caller records and uses the batch path.
//!
//! Tests (and only tests) can redirect every provider with a `ws://` base URL, either through
//! `Credentials::base_url` or the `OPENTALK_STT_WS_BASE_URL` environment variable.
mod assemblyai;
mod deepgram;
mod gemini_live;
mod openai_realtime;
pub mod session;
#[cfg(test)]
mod test_support;
#[cfg(test)]
mod tests;
mod util;

use super::registry;
use super::{Credentials, SttOptions, StreamingProvider, StreamingSession, SttError};
use crate::core::config::{Config, SttProviderId};

pub(crate) const DEFAULT_DEEPGRAM_ORIGIN: &str = "wss://api.deepgram.com";
pub(crate) const DEFAULT_ASSEMBLYAI_ORIGIN: &str = "wss://streaming.assemblyai.com";
pub(crate) const DEFAULT_OPENAI_ORIGIN: &str = "wss://api.openai.com";
pub(crate) const DEFAULT_GEMINI_ORIGIN: &str = "wss://generativelanguage.googleapis.com";

/// Environment variable overriding the WebSocket origin of every streaming provider.
pub const WS_BASE_ENV: &str = "OPENTALK_STT_WS_BASE_URL";

/// A `ws://` / `wss://` origin override: `Credentials::base_url` first (only when it already
/// is a WebSocket URL, since the batch path puts an https URL there), then the environment.
fn ws_override(creds: &Credentials) -> Option<String> {
    let usable = |s: &str| {
        let s = s.trim();
        (s.starts_with("ws://") || s.starts_with("wss://")).then(|| s.trim_end_matches('/').to_string())
    };
    creds.base_url.as_deref().and_then(usable).or_else(|| std::env::var(WS_BASE_ENV).ok().as_deref().and_then(usable))
}

/// `wss://host` + `path_and_query`, honouring the override.
pub(crate) fn ws_url(creds: &Credentials, default_origin: &str, path_and_query: &str) -> String {
    format!("{}{}", ws_override(creds).unwrap_or_else(|| default_origin.to_string()), path_and_query)
}

/// HTTP(S) twin of [`ws_url`] for the REST calls that accompany a socket (AssemblyAI token).
pub(crate) fn http_url(creds: &Credentials, default_ws_origin: &str, path_and_query: &str) -> String {
    let ws = ws_url(creds, default_ws_origin, path_and_query);
    if let Some(rest) = ws.strip_prefix("wss://") {
        format!("https://{rest}")
    } else if let Some(rest) = ws.strip_prefix("ws://") {
        format!("http://{rest}")
    } else {
        ws
    }
}

pub fn get(id: SttProviderId, model: &str) -> Option<&'static dyn StreamingProvider> {
    if !registry::model(id, model)?.streaming {
        return None;
    }
    match id {
        SttProviderId::Deepgram => Some(&deepgram::Deepgram),
        SttProviderId::Assemblyai => Some(&assemblyai::AssemblyAi),
        SttProviderId::Openai => Some(&openai_realtime::OpenAiRealtime),
        SttProviderId::Gemini => Some(&gemini_live::GeminiLive),
        // Corti (OAuth + REST interaction flow) and Tinfoil (attested TLS) stream nowhere here.
        _ => None,
    }
}

/// Start a session for the active provider when `cfg.stt.prefer_streaming` (or the model is
/// streaming-only) and the model supports streaming; None = use batch. Uses `super::credentials`
/// and `super::options` for keys/options.
pub fn start(cfg: &Config) -> Option<Result<Box<dyn StreamingSession>, SttError>> {
    start_with(cfg, super::credentials, super::options)
}

/// [`start`] with the credential/option resolvers injected (they depend on stored secrets).
pub(crate) fn start_with(
    cfg: &Config,
    credentials: impl FnOnce(&Config) -> Credentials,
    options: impl FnOnce(&Config) -> SttOptions,
) -> Option<Result<Box<dyn StreamingSession>, SttError>> {
    let id = cfg.stt.provider;
    let model = cfg.stt.models.get(&id).map(String::as_str).unwrap_or_else(|| registry::default_model(id));
    let info = registry::model(id, model)?;
    if !((cfg.stt.prefer_streaming && info.streaming) || info.streaming_only) {
        return None;
    }
    let provider = get(id, model)?;
    let mut opts = options(cfg);
    if opts.model.is_empty() {
        opts.model = model.to_string();
    }
    Some(provider.start(&opts, &credentials(cfg)))
}
