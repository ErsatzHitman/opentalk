//! OpenWhispr's default cleanup prompt (verbatim from src/locales/en/prompts.json `cleanupPrompt`)
//! plus the transcript wrapper, language instruction and dictionary suffix.
//! TASK llm: copy the exact prompt text and implement `system_prompt` / `user_message`.

/// Build the system prompt: custom prompt (if non-empty) or default, `{{agentName}}` -> "Assistant",
/// + "\n\n" + language instruction, + dictionary suffix when hints are non-empty.
pub fn system_prompt(custom: &str, language: &str, hints: &[String]) -> String {
    let _ = (custom, language, hints);
    todo!("llm")
}

/// `<transcript>\n{text}\n</transcript>\n\nOutput only the cleaned transcript.`
pub fn user_message(text: &str) -> String {
    format!("<transcript>\n{text}\n</transcript>\n\nOutput only the cleaned transcript.")
}
