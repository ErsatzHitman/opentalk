// Verdict logic shared by the live STT tests (`tests/e2e.rs` and `stt::providers::openrouter`).
//
// std only and free of `crate::` paths, so both crates can include this one file. A live test is
// a verification only when a free model returned a transcript of the spoken audio; everything
// short of that is `Unverified` (never a quiet pass), and a fault on our side is `Failed`.
use std::time::Duration;

/// Outcome of a live STT run.
#[derive(Debug, PartialEq)]
pub enum Live {
    /// A free model transcribed the audio (the transcript contains the spoken keywords).
    Verified(String),
    /// No free model produced a transcript (rate limits, gated models, audio dropped upstream).
    Unverified(String),
    /// Our side is wrong or the key is bad (auth, unknown model, bad request): always a failure.
    Failed(String),
}

/// Why a free model that answers cannot be verified (measured 2026-10-10, PLAN section 10).
pub const DROPPED_AUDIO_REASON: &str = "free endpoint dropped the audio (prompt_tokens=28 with or without the WAV; see PLAN §10)";

/// `Unverified` for a free endpoint that dropped the audio before the model.
pub fn dropped_audio(model: &str) -> Live {
    Live::Unverified(format!("{model}: {DROPPED_AUDIO_REASON}"))
}

/// What to do with one failed attempt, judged from the user-facing error message.
#[derive(Debug, PartialEq, Clone, Copy)]
pub enum Attempt {
    /// Transient (rate limit, capacity, timeout): retry with backoff.
    Retry,
    /// The model answered without the audio ("No audio detected"): unverified, one attempt only.
    DroppedAudio,
    /// The model is gated for this key ("denied access"): unverified, one attempt only.
    AccessDenied,
    /// Bad key, unknown model, bad request: a failure.
    Failed,
}

/// Classifies an `--transcribe-file` / `SttError` message.
pub fn classify_message(msg: &str) -> Attempt {
    if ["rate-limited", "having problems", "took too long"].iter().any(|k| msg.contains(k)) {
        Attempt::Retry
    } else if msg.contains("No audio detected") {
        Attempt::DroppedAudio
    } else if msg.contains("denied access") {
        Attempt::AccessDenied
    } else {
        Attempt::Failed
    }
}

/// Maps a finished live run to a test result. `Unverified` passes unless `require` is set
/// (`OPENTALK_LIVE_REQUIRE=1`); `Failed` never passes.
pub fn live_result(outcome: &Live, require: bool) -> Result<(), String> {
    match outcome {
        Live::Verified(_) => Ok(()),
        Live::Unverified(_) if !require => Ok(()),
        Live::Unverified(why) => Err(format!("OPENTALK_LIVE_REQUIRE=1 and STT was not verified: {why}")),
        Live::Failed(why) => Err(format!("live STT failed: {why}")),
    }
}

/// Prints a `VERIFIED` / `UNVERIFIED` / `FAILED` line for `test` and panics when `live_result` says so.
pub fn report_live(test: &str, outcome: Live) {
    match &outcome {
        Live::Verified(t) => eprintln!("{test}: VERIFIED live transcript: {t:?}"),
        Live::Unverified(why) => eprintln!("{test}: UNVERIFIED - no free model produced a transcript of the audio: {why}"),
        Live::Failed(why) => eprintln!("{test}: FAILED - {why}"),
    }
    let require = std::env::var("OPENTALK_LIVE_REQUIRE").is_ok_and(|v| v == "1");
    if let Err(e) = live_result(&outcome, require) {
        panic!("{test}: {e}");
    }
}

/// Runs one model: `call` returns the transcript or the error message. `backoff_s[i]` is the
/// sleep before attempt `i` (the first is normally 0). Only `Attempt::Retry` messages use the
/// remaining attempts; every other answer ends the model after the attempt that produced it.
/// A transcript without any of `spoken` (lower-case keywords) means the audio never reached
/// the model.
pub fn try_model(
    model: &str,
    spoken: &[&str],
    backoff_s: &[u64],
    mut call: impl FnMut() -> Result<String, String>,
    mut sleep: impl FnMut(Duration),
) -> Live {
    let mut last = String::from("no attempt was made");
    for (attempt, wait) in backoff_s.iter().enumerate() {
        if *wait > 0 {
            sleep(Duration::from_secs(*wait));
        }
        match call() {
            Ok(text) => {
                let lower = text.to_lowercase();
                return if spoken.iter().any(|k| lower.contains(k)) { Live::Verified(text) } else { dropped_audio(model) };
            }
            Err(msg) => match classify_message(&msg) {
                Attempt::Retry => {
                    eprintln!("{model} attempt {}: {msg}", attempt + 1);
                    last = msg;
                }
                Attempt::DroppedAudio => return dropped_audio(model),
                Attempt::AccessDenied => return Live::Unverified(format!("{model}: access denied")),
                Attempt::Failed => return Live::Failed(format!("{model}: {msg}")),
            },
        }
    }
    Live::Unverified(format!("{model}: {last}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Runs `try_model` where every attempt fails with `msg`; returns the verdict, the number of
    /// attempts and the sleeps taken.
    fn always_failing(msg: &str) -> (Live, usize, Vec<Duration>) {
        let (mut calls, mut sleeps) = (0usize, Vec::new());
        let live = try_model(
            "m:free",
            &["friday"],
            &[0, 6, 12, 18],
            || {
                calls += 1;
                Err(msg.to_string())
            },
            |d| sleeps.push(d),
        );
        (live, calls, sleeps)
    }

    #[test]
    fn live_verdicts_distinguish_verified_unverified_and_failed() {
        let (v, u, f) = (Live::Verified("t".into()), Live::Unverified("x".into()), Live::Failed("y".into()));
        assert!(live_result(&v, false).is_ok() && live_result(&v, true).is_ok());
        assert!(live_result(&u, false).is_ok());
        assert!(live_result(&u, true).unwrap_err().contains("not verified"));
        assert!(live_result(&f, false).is_err() && live_result(&f, true).is_err());
    }

    #[test]
    fn messages_are_classified_into_retry_unverified_once_and_failed() {
        for m in [
            "OpenRouter rate-limited the request. Wait a moment and try again.",
            "OpenRouter is having problems right now. Try again shortly.",
            "OpenRouter took too long to respond.",
        ] {
            assert_eq!(classify_message(m), Attempt::Retry, "{m}");
        }
        assert_eq!(classify_message("No audio detected"), Attempt::DroppedAudio);
        assert_eq!(classify_message("OpenRouter denied access. Your key may not include this model."), Attempt::AccessDenied);
        for m in ["OpenRouter rejected your API key.", "OpenRouter doesn't recognize the model \u{201c}x\u{201d}.", ""] {
            assert_eq!(classify_message(m), Attempt::Failed, "{m}");
        }
    }

    #[test]
    fn no_speech_and_denied_get_exactly_one_attempt_and_no_sleep() {
        let (live, calls, sleeps) = always_failing("No audio detected");
        assert_eq!((calls, sleeps.len()), (1, 0));
        assert_eq!(live, Live::Unverified(format!("m:free: {DROPPED_AUDIO_REASON}")));
        assert!(DROPPED_AUDIO_REASON.contains("prompt_tokens=28") && DROPPED_AUDIO_REASON.contains("PLAN §10"));

        let (live, calls, sleeps) = always_failing("OpenRouter denied access. Your key may not include this model.");
        assert_eq!((calls, sleeps.len()), (1, 0));
        assert_eq!(live, Live::Unverified("m:free: access denied".into()));
    }

    #[test]
    fn transient_errors_retry_with_backoff_then_end_unverified() {
        for msg in [
            "OpenRouter rate-limited the request. Wait a moment and try again.",
            "OpenRouter is having problems right now. Try again shortly.",
            "OpenRouter took too long to respond.",
        ] {
            let (live, calls, sleeps) = always_failing(msg);
            assert_eq!(calls, 4, "{msg}");
            assert_eq!(sleeps, [6, 12, 18].map(Duration::from_secs));
            assert_eq!(live, Live::Unverified(format!("m:free: {msg}")));
        }
        // A transient error followed by a transcript still verifies.
        let mut calls = 0;
        let live = try_model(
            "m:free",
            &["friday"],
            &[0, 6],
            || {
                calls += 1;
                if calls == 1 { Err("OpenRouter rate-limited the request.".into()) } else { Ok("On Friday".into()) }
            },
            |_| {},
        );
        assert_eq!((calls, live), (2, Live::Verified("On Friday".into())));
    }

    #[test]
    fn bad_key_fails_at_once_and_an_unrelated_reply_is_the_dropped_audio_case() {
        let (live, calls, sleeps) = always_failing("OpenRouter rejected your API key.");
        assert_eq!((calls, sleeps.len()), (1, 0));
        assert_eq!(live, Live::Failed("m:free: OpenRouter rejected your API key.".into()));

        let live = try_model("m:free", &["friday"], &[0, 6], || Ok("I need the audio file.".into()), |_| {});
        assert_eq!(live, dropped_audio("m:free"));
        let live = try_model("m:free", &["friday"], &[0], || Ok("Ship it on FRIDAY".into()), |_| {});
        assert_eq!(live, Live::Verified("Ship it on FRIDAY".into()));
    }
}
