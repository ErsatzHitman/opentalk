//! Batch STT providers. TASK stt-batch: one file per provider — openai.rs (incl. SSE when
//! stream=true), groq.rs, xai.rs, mistral.rs, gemini.rs, deepgram.rs (REST /v1/listen),
//! tinfoil.rs (plain HTTPS), openrouter.rs (chat completions + input_audio),
//! custom.rs (OpenAI-compatible + Azure). Corti (OAuth multi-step) is TASK stt-stream: corti.rs.
use super::BatchProvider;
use crate::core::config::SttProviderId;

pub fn get(id: SttProviderId) -> &'static dyn BatchProvider {
    let _ = id;
    todo!("stt-batch")
}
