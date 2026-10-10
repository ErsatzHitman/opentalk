//! Energy-based voice activity detection and silence trimming.
//!
//! Thresholds follow OpenWhispr's `localSpeechGate.js`: a frame is speech when
//! `rms >= 0.003 && peak >= 0.02` (full scale = 1.0); anything quieter than `SILENCE_RMS` is
//! silence. Analysis runs over 20 ms frames; pauses of up to `HANGOVER_MS` between two speech
//! frames count as speech so natural gaps inside a sentence are not cut out.
use crate::stt::Audio;

pub const FRAME_MS: u64 = 20;
#[cfg(test)]
pub const SILENCE_RMS: f32 = 0.002;
pub const SPEECH_RMS: f32 = 0.003;
pub const SPEECH_PEAK: f32 = 0.02;
/// Longest pause between speech frames still treated as part of the speech.
pub const HANGOVER_MS: u64 = 300;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct VadInfo {
    /// Total milliseconds classified as speech.
    pub speech_ms: u64,
    pub first_speech_ms: Option<u64>,
    pub last_speech_ms: Option<u64>,
    pub peak_rms: f32,
}

/// (rms, peak) of one frame, normalised to full scale 1.0.
fn frame_stats(frame: &[i16]) -> (f32, f32) {
    let mut sum = 0f64;
    let mut peak = 0i32;
    for &s in frame {
        let v = s as f64;
        sum += v * v;
        peak = peak.max((s as i32).abs());
    }
    let rms = (sum / frame.len().max(1) as f64).sqrt() / 32768.0;
    (rms as f32, peak as f32 / 32768.0)
}

/// Energy-based analysis over 20 ms frames.
pub fn analyze(audio: &Audio) -> VadInfo {
    if audio.sample_rate == 0 || audio.samples.is_empty() {
        return VadInfo::default();
    }
    let frame_len = (audio.sample_rate as u64 * FRAME_MS / 1000).max(1) as usize;
    let mut peak_rms = 0f32;
    let mut speech: Vec<bool> = Vec::with_capacity(audio.samples.len() / frame_len + 1);
    for frame in audio.samples.chunks(frame_len) {
        // Ignore a sliver of a final frame: its statistics are not comparable.
        if frame.len() < frame_len / 2 && !speech.is_empty() {
            break;
        }
        let (rms, peak) = frame_stats(frame);
        peak_rms = peak_rms.max(rms);
        speech.push(rms >= SPEECH_RMS && peak >= SPEECH_PEAK);
    }
    let first = speech.iter().position(|&s| s);
    let last = speech.iter().rposition(|&s| s);
    let (Some(first), Some(last)) = (first, last) else {
        return VadInfo { peak_rms, ..VadInfo::default() };
    };

    // Count speech frames plus short gaps that sit between two speech frames.
    let hang = (HANGOVER_MS / FRAME_MS) as usize;
    let mut frames = 0usize;
    let mut gap = 0usize;
    for &s in &speech[first..=last] {
        if s {
            frames += 1;
            if gap <= hang {
                frames += gap;
            }
            gap = 0;
        } else {
            gap += 1;
        }
    }
    let total_ms = audio.duration_ms();
    VadInfo {
        speech_ms: frames as u64 * FRAME_MS,
        first_speech_ms: Some(first as u64 * FRAME_MS),
        last_speech_ms: Some(((last as u64 + 1) * FRAME_MS).min(total_ms)),
        peak_rms,
    }
}

/// Trim leading/trailing non-speech keeping `pad_ms` around speech. No speech -> unchanged.
pub fn trim(audio: &Audio, pad_ms: u64) -> Audio {
    let info = analyze(audio);
    let (Some(first), Some(last)) = (info.first_speech_ms, info.last_speech_ms) else {
        return audio.clone();
    };
    let per_ms = audio.sample_rate as u64;
    let start = (first.saturating_sub(pad_ms) * per_ms / 1000) as usize;
    let end = (((last + pad_ms) * per_ms / 1000) as usize).min(audio.samples.len());
    if start == 0 && end == audio.samples.len() {
        return audio.clone();
    }
    Audio { samples: audio.samples[start.min(end)..end].to_vec(), sample_rate: audio.sample_rate }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f64::consts::PI;

    const RATE: u32 = 16_000;

    fn tone(ms: u64, amp: f64) -> Vec<i16> {
        (0..(RATE as u64 * ms / 1000) as usize)
            .map(|n| (amp * 32767.0 * (2.0 * PI * 220.0 * n as f64 / RATE as f64).sin()) as i16)
            .collect()
    }

    fn silence(ms: u64) -> Vec<i16> {
        vec![0; (RATE as u64 * ms / 1000) as usize]
    }

    fn noise(ms: u64, amp: f64, seed: u64) -> Vec<i16> {
        let mut x = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (0..(RATE as u64 * ms / 1000) as usize)
            .map(|_| {
                x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                (((x >> 33) as f64 / (1u64 << 31) as f64 - 0.5) * 2.0 * amp * 32767.0) as i16
            })
            .collect()
    }

    fn audio(parts: &[Vec<i16>]) -> Audio {
        Audio { samples: parts.concat(), sample_rate: RATE }
    }

    #[test]
    fn digital_silence_has_no_speech() {
        let info = analyze(&audio(&[silence(2000)]));
        assert_eq!(info.speech_ms, 0);
        assert_eq!(info.first_speech_ms, None);
        assert_eq!(info.last_speech_ms, None);
        assert_eq!(info.peak_rms, 0.0);
    }

    #[test]
    fn empty_and_zero_rate_audio_are_safe() {
        assert_eq!(analyze(&Audio::default()), VadInfo::default());
        assert_eq!(analyze(&Audio { samples: vec![1000; 100], sample_rate: 0 }), VadInfo::default());
        assert!(trim(&Audio::default(), 150).samples.is_empty());
    }

    #[test]
    fn quiet_noise_is_not_speech() {
        // rms ~0.0006, peak ~0.002: well under both thresholds.
        let info = analyze(&audio(&[noise(1500, 0.002, 1)]));
        assert_eq!(info.speech_ms, 0);
        assert!(info.peak_rms < SILENCE_RMS);
    }

    #[test]
    fn tone_between_silence_is_located() {
        let a = audio(&[silence(500), tone(1000, 0.3), silence(700)]);
        let info = analyze(&a);
        assert_eq!(info.first_speech_ms, Some(500));
        assert_eq!(info.last_speech_ms, Some(1500));
        assert_eq!(info.speech_ms, 1000);
        assert!(info.peak_rms > 0.19 && info.peak_rms < 0.22, "{}", info.peak_rms);
    }

    #[test]
    fn short_pause_inside_speech_counts() {
        let a = audio(&[tone(400, 0.3), silence(200), tone(400, 0.3)]);
        assert_eq!(analyze(&a).speech_ms, 1000);
    }

    #[test]
    fn long_pause_inside_speech_is_excluded() {
        let a = audio(&[tone(400, 0.3), silence(800), tone(400, 0.3)]);
        let info = analyze(&a);
        assert_eq!(info.speech_ms, 800);
        assert_eq!(info.first_speech_ms, Some(0));
        assert_eq!(info.last_speech_ms, Some(1600));
    }

    #[test]
    fn low_peak_hum_is_not_speech() {
        // rms 0.0035 but peak only 0.005: fails the peak requirement.
        let hum = tone(1000, 0.005);
        assert_eq!(analyze(&audio(&[hum])).speech_ms, 0);
    }

    #[test]
    fn noise_floor_between_words_does_not_extend_speech() {
        let a = audio(&[noise(800, 0.001, 7), tone(500, 0.2), noise(800, 0.001, 9)]);
        let info = analyze(&a);
        assert_eq!(info.speech_ms, 500);
        assert_eq!(info.first_speech_ms, Some(800));
    }

    #[test]
    fn trim_keeps_padding_both_sides() {
        let a = audio(&[silence(1000), tone(500, 0.3), silence(1000)]);
        let t = trim(&a, 150);
        assert_eq!(t.duration_ms(), 150 + 500 + 150);
        assert_eq!(t.sample_rate, RATE);
        // Speech is still present and centred.
        let info = analyze(&t);
        // The trimmed clip's frame grid is offset from the original's, so allow one frame of slack.
        let first = info.first_speech_ms.unwrap();
        assert!((130..=150).contains(&first), "{first}");
    }

    #[test]
    fn trim_clamps_at_edges() {
        let a = audio(&[silence(60), tone(500, 0.3), silence(60)]);
        let t = trim(&a, 150);
        assert_eq!(t.samples.len(), a.samples.len());
    }

    #[test]
    fn trim_without_speech_is_unchanged() {
        let a = audio(&[silence(1000)]);
        assert_eq!(trim(&a, 150).samples, a.samples);
    }

    #[test]
    fn trim_zero_padding_cuts_exactly() {
        let a = audio(&[silence(400), tone(200, 0.3), silence(400)]);
        assert_eq!(trim(&a, 0).duration_ms(), 200);
    }

    fn fixture(name: &str) -> Audio {
        let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name);
        Audio::from_wav(&std::fs::read(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))).expect("wav")
    }

    #[test]
    fn fixture_silence_is_silent() {
        let a = fixture("silence_2s.wav");
        assert_eq!(a.sample_rate, 16_000);
        let info = analyze(&a);
        assert_eq!(info.speech_ms, 0);
        assert_eq!(trim(&a, 150).samples.len(), a.samples.len());
    }

    #[test]
    fn fixture_tts_has_speech_and_trims() {
        for name in ["tts_clean.wav", "tts_fillers.wav"] {
            let a = fixture(name);
            let info = analyze(&a);
            let dur = a.duration_ms();
            assert!(info.speech_ms > dur / 3, "{name}: speech {} of {dur}", info.speech_ms);
            assert!(info.peak_rms > 0.01, "{name}");
            let t = trim(&a, 150);
            println!("{name}: {dur} ms, {info:?}, trimmed to {} ms", t.duration_ms());
            assert!(t.samples.len() <= a.samples.len());
            // Speech boundaries are preserved by trimming.
            let ti = analyze(&t);
            assert!(ti.speech_ms.abs_diff(info.speech_ms) <= 2 * FRAME_MS, "{name}: {} vs {}", ti.speech_ms, info.speech_ms);
            assert!(ti.first_speech_ms.unwrap() <= 150, "{name}");
        }
    }
}
