//! Pure logic behind the tray Settings pages: choice tables, value mappings, the reset-file list,
//! prompt-editor rules and the provider "Test" runners. No Win32 UI; the pages
//! (`daemon::tray::page::settings`) and the daemon call into it. Moved here from the former
//! settings window (`src/ui/settings`) together with its tests.
use std::time::Instant;

use crate::core::config::{Config, FillerMode, PanelPosition};

// ---- Preferences ---------------------------------------------------------------------------

/// Keep-warm choices: (label, seconds).
pub const WARM_CHOICES: &[(&str, u32)] = &[("Off", 0), ("30 seconds", 30), ("1 minute", 60), ("5 minutes", 300), ("15 minutes", 900)];

/// Index in [`WARM_CHOICES`] of `secs` (the nearest choice for an unlisted value).
pub fn warm_index(secs: u32) -> usize {
    WARM_CHOICES.iter().position(|(_, s)| *s == secs).unwrap_or_else(|| {
        // Unknown value in config.json: show the closest choice rather than "Off".
        WARM_CHOICES.iter().enumerate().min_by_key(|(_, (_, s))| (*s as i64 - secs as i64).abs()).map(|(i, _)| i).unwrap_or(0)
    })
}

/// Start positions of the floating icon: (label, value).
pub const POSITIONS: &[(&str, PanelPosition)] = &[("Bottom right", PanelPosition::BottomRight), ("Bottom center", PanelPosition::BottomCenter), ("Bottom left", PanelPosition::BottomLeft)];

pub fn position_index(p: PanelPosition) -> usize {
    POSITIONS.iter().position(|(_, v)| *v == p).unwrap_or(0)
}

// ---- Privacy -------------------------------------------------------------------------------

/// Retention choices: (label, days; 0 = keep forever).
pub const RETENTION: &[(&str, u32)] = &[("Keep forever", 0), ("7 days", 7), ("30 days", 30), ("90 days", 90), ("1 year", 365)];

pub fn retention_index(days: u32) -> usize {
    RETENTION.iter().position(|(_, d)| *d == days).unwrap_or_else(|| {
        // An unlisted value (hand-edited config): show the nearest longer period so nothing is
        // displayed as shorter than what is actually enforced.
        RETENTION.iter().position(|(_, d)| *d != 0 && *d >= days).unwrap_or(0)
    })
}

// ---- About: reset --------------------------------------------------------------------------

/// Files removed by "Reset app data" (relative to the app directory).
pub const RESET_FILES: &[&str] = &["config.json", "config.json.bad", "history.jsonl", "secrets.json"];

/// Delete the user's files under `dir`. Returns the first error message, if any; missing files are fine.
pub fn reset_files(dir: &std::path::Path) -> Result<(), String> {
    let mut first_err = None;
    for f in RESET_FILES {
        match std::fs::remove_file(dir.join(f)) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                first_err.get_or_insert(format!("{f}: {e}"));
            }
        }
    }
    match first_err {
        None => Ok(()),
        Some(e) => Err(e),
    }
}

// ---- Cleanup -------------------------------------------------------------------------------

/// What the prompt editor shows: the saved custom prompt, else the default.
pub fn editor_text(custom: &str, default: &str) -> String {
    if custom.trim().is_empty() { default.to_string() } else { custom.to_string() }
}

/// What to store after an edit: nothing (use the default) when the text equals the default.
pub fn stored_prompt(edited: &str, default: &str) -> String {
    if edited.trim().is_empty() || edited.trim() == default.trim() { String::new() } else { edited.to_string() }
}

pub fn filler_index(m: FillerMode) -> usize {
    match m {
        FillerMode::Off => 0,
        FillerMode::Standard => 1,
        FillerMode::Aggressive => 2,
    }
}

pub fn filler_from_index(i: usize) -> FillerMode {
    [FillerMode::Off, FillerMode::Standard, FillerMode::Aggressive][i.min(2)]
}

// ---- Languages -----------------------------------------------------------------------------

/// Transcription languages (compact copy of OpenWhispr's languageRegistry.json: code + label).
/// "auto" omits the language parameter. Order: Auto-detect first, then alphabetical by label.
pub const LANGUAGES: &[(&str, &str)] = &[
    ("auto", "Auto-detect"),
    ("af", "Afrikaans"),
    ("ar", "Arabic"),
    ("hy", "Armenian"),
    ("az", "Azerbaijani"),
    ("be", "Belarusian"),
    ("bs", "Bosnian"),
    ("bg", "Bulgarian"),
    ("ca", "Catalan"),
    ("zh-CN", "Chinese (Simplified)"),
    ("zh-TW", "Chinese (Traditional)"),
    ("hr", "Croatian"),
    ("cs", "Czech"),
    ("da", "Danish"),
    ("nl", "Dutch"),
    ("en-US", "English (US)"),
    ("en-GB", "English (UK)"),
    ("et", "Estonian"),
    ("fi", "Finnish"),
    ("fr", "French"),
    ("gl", "Galician"),
    ("de", "German"),
    ("el", "Greek"),
    ("he", "Hebrew"),
    ("hi", "Hindi"),
    ("hu", "Hungarian"),
    ("is", "Icelandic"),
    ("id", "Indonesian"),
    ("it", "Italian"),
    ("ja", "Japanese"),
    ("kn", "Kannada"),
    ("kk", "Kazakh"),
    ("ko", "Korean"),
    ("lv", "Latvian"),
    ("lt", "Lithuanian"),
    ("mk", "Macedonian"),
    ("ms", "Malay"),
    ("mt", "Maltese"),
    ("mr", "Marathi"),
    ("mi", "Maori"),
    ("ne", "Nepali"),
    ("no", "Norwegian"),
    ("fa", "Persian"),
    ("pl", "Polish"),
    ("pt", "Portuguese"),
    ("ro", "Romanian"),
    ("ru", "Russian"),
    ("sr", "Serbian"),
    ("sk", "Slovak"),
    ("sl", "Slovenian"),
    ("es", "Spanish"),
    ("sw", "Swahili"),
    ("sv", "Swedish"),
    ("tl", "Tagalog"),
    ("ta", "Tamil"),
    ("th", "Thai"),
    ("tr", "Turkish"),
    ("uk", "Ukrainian"),
    ("ur", "Urdu"),
    ("vi", "Vietnamese"),
    ("cy", "Welsh"),
];

pub fn label_of(code: &str) -> &'static str {
    LANGUAGES.iter().find(|(c, _)| *c == code).map(|(_, l)| *l).unwrap_or("Auto-detect")
}


// ---- Provider tests ------------------------------------------------------------------------

/// Short clean sample ("Hey, can you send the quarterly report to Priya by end of day? Thanks.").
const SAMPLE_WAV: &[u8] = include_bytes!("../../tests/fixtures/tts_clean.wav");

/// Text the AI "Run on sample" test cleans.
pub const SAMPLE_TEXT: &str = "um so i think we should uh ship the new release on friday you know because the the client is waiting";

/// Result of a provider test run on a worker thread.
#[derive(Clone, Debug, PartialEq)]
pub enum TestOutcome {
    Ok { text: String, latency_ms: u64 },
    Err(String),
}

fn safe<T>(f: impl FnOnce() -> T) -> Option<T> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).ok()
}

/// Run the embedded sample through the active speech provider (blocking; call on a worker).
pub fn run_stt_sample(cfg: &Config) -> TestOutcome {
    let Some(audio) = crate::stt::Audio::from_wav(SAMPLE_WAV) else {
        return TestOutcome::Err("embedded sample is unreadable".into());
    };
    let started = Instant::now();
    match safe(|| crate::stt::transcribe(cfg, &audio)) {
        Some(Ok(t)) => TestOutcome::Ok { text: t.text, latency_ms: started.elapsed().as_millis() as u64 },
        Some(Err(e)) => TestOutcome::Err(e.to_string()),
        None => TestOutcome::Err("the speech provider is not available in this build".into()),
    }
}

/// Run [`SAMPLE_TEXT`] through the configured cleanup model, as the daemon does (blocking).
pub fn run_llm_sample(cfg: &Config) -> TestOutcome {
    let started = Instant::now();
    let hints = crate::core::dictionary::hint_words(&cfg.dictionary, &cfg.snippets);
    match safe(|| crate::llm::cleanup(cfg, SAMPLE_TEXT, &hints)) {
        Some(Ok(t)) => TestOutcome::Ok { text: t, latency_ms: started.elapsed().as_millis() as u64 },
        Some(Err(e)) => TestOutcome::Err(match e {
            crate::llm::LlmError::NotConfigured => "Not configured: add an API key and a model.".into(),
            crate::llm::LlmError::Timeout => "The model did not answer in time.".into(),
            crate::llm::LlmError::Http { status, detail } => format!("HTTP {status}: {detail}"),
            crate::llm::LlmError::Network(m) => format!("Network error: {m}"),
            crate::llm::LlmError::InvalidOutput(m) => format!("The reply was rejected: {m}"),
        }),
        None => TestOutcome::Err("AI cleanup is not available in this build".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn warm_index_exact_and_nearest() {
        assert_eq!(warm_index(0), 0);
        assert_eq!(warm_index(60), 2);
        assert_eq!(warm_index(900), 4);
        assert_eq!(warm_index(100), 2, "100 s is closest to 1 minute");
    }

    #[test]
    fn positions_cover_every_variant() {
        for v in [PanelPosition::BottomRight, PanelPosition::BottomCenter, PanelPosition::BottomLeft] {
            assert!(POSITIONS.iter().any(|(_, p)| *p == v));
            assert_eq!(POSITIONS[position_index(v)].1, v);
        }
    }

    #[test]
    fn retention_lookup() {
        assert_eq!(retention_index(0), 0);
        assert_eq!(retention_index(30), 2);
        assert_eq!(retention_index(365), 4);
        assert_eq!(RETENTION[retention_index(45)].1, 90, "an unlisted value rounds up to a longer period");
        assert_eq!(retention_index(4000), 0);
    }

    #[test]
    fn reset_removes_user_files_and_keeps_others() {
        let dir = std::env::temp_dir().join(format!("opentalk-ui-reset-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for f in ["config.json", "history.jsonl", "secrets.json", "keep.txt"] {
            std::fs::write(dir.join(f), "x").unwrap();
        }
        reset_files(&dir).unwrap();
        assert!(!dir.join("config.json").exists());
        assert!(!dir.join("history.jsonl").exists());
        assert!(!dir.join("secrets.json").exists());
        assert!(dir.join("keep.txt").exists());
        // Idempotent when files are already gone.
        reset_files(&dir).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn editor_shows_custom_or_default() {
        assert_eq!(editor_text("", "DEF"), "DEF");
        assert_eq!(editor_text("  ", "DEF"), "DEF");
        assert_eq!(editor_text("mine", "DEF"), "mine");
    }

    #[test]
    fn storing_default_text_clears_the_custom_prompt() {
        assert_eq!(stored_prompt("DEF\n", "DEF"), "");
        assert_eq!(stored_prompt("", "DEF"), "");
        assert_eq!(stored_prompt("changed", "DEF"), "changed");
    }

    #[test]
    fn filler_index_roundtrip() {
        for m in [FillerMode::Off, FillerMode::Standard, FillerMode::Aggressive] {
            assert_eq!(filler_from_index(filler_index(m)), m);
        }
    }

    #[test]
    fn language_registry_shape() {
        assert_eq!(LANGUAGES.len(), 61);
        assert_eq!(LANGUAGES[0].0, "auto");
        assert_eq!(label_of("de"), "German");
        assert_eq!(label_of("xx"), "Auto-detect");
    }

    #[test]
    fn sample_is_a_valid_short_wav() {
        let a = crate::stt::Audio::from_wav(SAMPLE_WAV).expect("tts_clean.wav parses");
        assert_eq!(a.sample_rate, 16_000);
        assert!(a.duration_ms() > 1000 && a.duration_ms() < 15_000, "{} ms", a.duration_ms());
    }
}

#[cfg(test)]
mod live_tests {
    use super::*;
    use crate::core::config::SttProviderId;

    /// Verdict logic shared with `tests/e2e.rs` (one definition: `tests/common/live.rs`).
    #[allow(dead_code)]
    mod live {
        include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/common/live.rs"));
    }
    use live::{Live, report_live, try_model};

    /// Free OpenRouter audio-input models (PLAN section 10); the live policy allows `:free` only.
    const FREE_MODELS: [&str; 3] =
        ["thinkingmachines/inkling-small:free", "thinkingmachines/inkling:free", "nvidia/nemotron-3-nano-omni-30b-a3b-reasoning:free"];

    /// Words of the sample ("...send the quarterly report to Priya by end of day"); a reply
    /// without any of them is not a transcript of it (e.g. "I need the audio file.").
    const SPOKEN: [&str; 2] = ["report", "priya"];

    /// Runs the Test button's pipeline (`run_sample`) on `model` and judges the reply.
    fn judge(cfg: &Config, model: &str, backoff_s: &[u64], sleep: impl FnMut(std::time::Duration)) -> Live {
        let mut cfg = cfg.clone();
        cfg.stt.models.insert(SttProviderId::Openrouter, model.to_string());
        try_model(
            model,
            &SPOKEN,
            backoff_s,
            || match run_stt_sample(&cfg) {
                TestOutcome::Ok { text, latency_ms } => {
                    eprintln!("{model}: {latency_ms} ms: {text:?}");
                    Ok(text)
                }
                TestOutcome::Err(e) => {
                    eprintln!("{model}: {e}");
                    Err(e)
                }
            },
            sleep,
        )
    }

    /// A 200 reply that does not contain the spoken words must never count as VERIFIED.
    #[test]
    fn reply_without_the_spoken_words_is_not_verified() {
        let mut calls = 0;
        let v = try_model(
            FREE_MODELS[2],
            &SPOKEN,
            &[0, 3],
            || {
                calls += 1;
                Ok("I need the audio file.".to_string())
            },
            |_| {},
        );
        assert_eq!(calls, 1);
        assert_eq!(v, live::dropped_audio(FREE_MODELS[2]));
        assert!(FREE_MODELS.iter().all(|m| m.ends_with(":free")));
    }

    /// The Test button's pipeline against the real OpenRouter free tier. Needs
    /// `OPENTALK_SECRET_OPENROUTER`. Prints `VERIFIED` / `UNVERIFIED - <reason>` / `FAILED`;
    /// upstream limits are UNVERIFIED (fails under `OPENTALK_LIVE_REQUIRE=1`), a bad key or
    /// request fails.
    #[test]
    #[ignore = "network: OpenRouter free model"]
    fn live_stt_test_button_openrouter() {
        const TEST: &str = "live_stt_test_button_openrouter";
        if std::env::var("OPENTALK_SECRET_OPENROUTER").is_err() {
            return report_live(TEST, Live::Unverified("OPENTALK_SECRET_OPENROUTER not set".into()));
        }
        let mut cfg = Config::default();
        cfg.stt.provider = SttProviderId::Openrouter;
        let mut notes = Vec::new();
        for model in FREE_MODELS {
            assert!(model.ends_with(":free"), "live tests may only use :free models");
            match judge(&cfg, model, &[0, 3, 8], std::thread::sleep) {
                Live::Unverified(why) => notes.push(why),
                done => return report_live(TEST, done),
            }
        }
        report_live(TEST, Live::Unverified(notes.join("; ")));
    }
}
