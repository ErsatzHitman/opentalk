//! Personal dictionary + snippets helpers. TASK cleanup implements (ports OpenWhispr's
//! dictionaryPromptCap.js, dictionaryEchoFilter.js, snippets.ts; adds case-fix and fuzzy fix).
use crate::core::config::Snippet;

/// Words fed to STT/LLM as hints: dictionary words + snippet triggers (deduped, case-insensitive).
pub fn hint_words(dictionary: &[String], snippets: &[Snippet]) -> Vec<String> {
    let _ = (dictionary, snippets);
    todo!("cleanup")
}

/// `words.join(", ")` trimmed at the last comma within `max_chars` (OpenWhispr trimDictionaryPrompt).
pub fn build_prompt(words: &[String], max_chars: usize) -> String {
    let _ = (words, max_chars);
    todo!("cleanup")
}

/// Whisper echoed the prompt back instead of transcribing (90% / 70% rule).
pub fn is_prompt_echo(text: &str, prompt: &str) -> bool {
    let _ = (text, prompt);
    todo!("cleanup")
}

/// Expand snippet triggers (longest first, Unicode word boundaries, case-insensitive, Turkish i).
pub fn expand_snippets(text: &str, snippets: &[Snippet]) -> String {
    let _ = (text, snippets);
    todo!("cleanup")
}

/// Exact case-insensitive matches take the dictionary's casing ("openai" -> "OpenAI").
pub fn case_fix(text: &str, dictionary: &[String]) -> String {
    let _ = (text, dictionary);
    todo!("cleanup")
}

/// Conservative fuzzy correction of 1-3 token spans toward dictionary terms.
pub fn fuzzy_fix(text: &str, dictionary: &[String]) -> String {
    let _ = (text, dictionary);
    todo!("cleanup")
}
