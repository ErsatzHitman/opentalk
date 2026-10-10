//! Latency instrumentation: one [`Sample`] per dictation, kept in a small in-process ring buffer
//! and written to the log as a single `latency ...` line that `--bench latency` and
//! `bench/bench.ps1` read back (`parse_line`).
//!
//! Definitions (all in ms, QPC based):
//! - `hotkey_pill`: hotkey event seen by the low-level hook (stamped on its thread, so the time
//!   the event waits for the message loop counts) -> pill repainted in its "preparing" state
//! - `hotkey_audio`: hotkey event seen by the hook -> first microphone packet delivered
//! - `prep`: stop requested -> worker about to send the request (stop recorder, VAD, trim)
//! - `stt`: request sent -> transcript back (batch round trip, or the streaming finalise)
//! - `cleanup`: deterministic cleanup + optional AI cleanup + snippets
//! - `paste`: clipboard write + keystroke injection
//! - `total`: stop requested -> paste done
use std::collections::VecDeque;
use std::sync::Mutex;

const RING_CAP: usize = 64;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Sample {
    pub seq: u64,
    /// Hook event -> pill repainted.
    pub hotkey_pill: f64,
    /// Hook event -> first audio packet. Negative when no audio packet was seen.
    pub hotkey_audio: f64,
    pub prep: f64,
    pub stt: f64,
    pub cleanup: f64,
    pub paste: f64,
    pub total: f64,
    /// Length of the recording that was uploaded, ms.
    pub audio_ms: u64,
    /// Bytes uploaded (WAV size; 0 for streaming).
    pub upload_bytes: usize,
    pub streamed: bool,
    pub provider: String,
    pub model: String,
}

static RING: Mutex<VecDeque<Sample>> = Mutex::new(VecDeque::new());

/// Store `s` in the ring and write its log line.
pub fn record(s: Sample) {
    crate::log_info!("{}", format_line(&s));
    let mut ring = RING.lock().unwrap_or_else(|e| e.into_inner());
    if ring.len() >= RING_CAP {
        ring.pop_front();
    }
    ring.push_back(s);
}

/// The most recent samples, oldest first.
#[cfg(test)]
pub fn recent() -> Vec<Sample> {
    RING.lock().unwrap_or_else(|e| e.into_inner()).iter().cloned().collect()
}

pub fn format_line(s: &Sample) -> String {
    format!(
        "latency seq={} hotkey_pill={:.1} hotkey_audio={:.1} prep={:.1} stt={:.1} cleanup={:.1} paste={:.1} total={:.1} audio_ms={} bytes={} streamed={} provider={} model={}",
        s.seq,
        s.hotkey_pill,
        s.hotkey_audio,
        s.prep,
        s.stt,
        s.cleanup,
        s.paste,
        s.total,
        s.audio_ms,
        s.upload_bytes,
        s.streamed as u8,
        s.provider,
        s.model.replace(' ', "_"),
    )
}

/// Parse a log line produced by [`format_line`] (the `[t_ms] INFO ` prefix is ignored).
pub fn parse_line(line: &str) -> Option<Sample> {
    let rest = &line[line.find("latency seq=")? + "latency ".len()..];
    let mut s = Sample::default();
    for kv in rest.split_whitespace() {
        let (k, v) = kv.split_once('=')?;
        let f = || v.parse::<f64>().ok();
        match k {
            "seq" => s.seq = v.parse().ok()?,
            "hotkey_pill" => s.hotkey_pill = f()?,
            "hotkey_audio" => s.hotkey_audio = f()?,
            "prep" => s.prep = f()?,
            "stt" => s.stt = f()?,
            "cleanup" => s.cleanup = f()?,
            "paste" => s.paste = f()?,
            "total" => s.total = f()?,
            "audio_ms" => s.audio_ms = v.parse().ok()?,
            "bytes" => s.upload_bytes = v.parse().ok()?,
            "streamed" => s.streamed = v == "1",
            "provider" => s.provider = v.to_string(),
            "model" => s.model = v.to_string(),
            _ => {}
        }
    }
    Some(s)
}

/// Median and 95th percentile of `values` (nearest-rank); `None` when empty.
pub fn median_p95(values: &mut [f64]) -> Option<(f64, f64)> {
    if values.is_empty() {
        return None;
    }
    values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let at = |q: f64| values[((values.len() as f64 * q).ceil() as usize).clamp(1, values.len()) - 1];
    Some((at(0.5), at(0.95)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(seq: u64) -> Sample {
        Sample {
            seq,
            hotkey_pill: 3.2,
            hotkey_audio: 21.5,
            prep: 4.0,
            stt: 612.3,
            cleanup: 0.4,
            paste: 38.1,
            total: 655.0,
            audio_ms: 8300,
            upload_bytes: 265_000,
            streamed: false,
            provider: "openrouter".into(),
            model: "some/model:free".into(),
        }
    }

    #[test]
    fn line_roundtrip() {
        let s = sample(7);
        let line = format!("[    123.4] INFO  {}", format_line(&s));
        let back = parse_line(&line).unwrap();
        assert_eq!(back.seq, 7);
        assert!((back.stt - 612.3).abs() < 0.05);
        assert_eq!((back.audio_ms, back.upload_bytes, back.streamed), (8300, 265_000, false));
        assert_eq!((back.provider.as_str(), back.model.as_str()), ("openrouter", "some/model:free"));
        assert!(parse_line("[1.0] INFO  hello").is_none());
    }

    #[test]
    fn ring_keeps_the_newest() {
        for i in 0..(RING_CAP as u64 + 5) {
            record(sample(1000 + i));
        }
        let r = recent();
        assert!(r.len() <= RING_CAP);
        assert_eq!(r.last().unwrap().seq, 1000 + RING_CAP as u64 + 4);
    }

    #[test]
    fn percentiles() {
        let mut v = vec![5.0, 1.0, 3.0, 2.0, 4.0, 100.0, 7.0, 6.0, 8.0, 9.0];
        let (m, p95) = median_p95(&mut v).unwrap();
        assert_eq!(m, 5.0);
        assert_eq!(p95, 100.0);
        assert!(median_p95(&mut []).is_none());
    }
}
