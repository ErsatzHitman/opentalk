//! Split a long dictation into pieces a local model can take in one request (Whisper sees 30 s at
//! a time; the NeMo exports refuse a few minutes). Each cut lands on the quietest 100 ms in the
//! last third of the allowed length, so it falls in a pause rather than through a word.
use std::ops::Range;

const FRAME_MS: usize = 20;
/// Width of the quiet window a cut is centred on.
const QUIET_FRAMES: usize = 5;

/// Sample ranges covering `samples` in order, none longer than `max_s` seconds.
pub fn split(samples: &[i16], rate: u32, max_s: f32) -> Vec<Range<usize>> {
    let n = samples.len();
    if rate == 0 || !max_s.is_finite() || n as f32 <= max_s * rate as f32 {
        return std::iter::once(0..n).collect();
    }
    let frame = (rate as usize * FRAME_MS / 1000).max(1);
    let energy: Vec<f64> = samples.chunks(frame).map(|c| c.iter().map(|&s| (s as f64) * (s as f64)).sum::<f64>() / c.len() as f64).collect();
    let max_frames = ((max_s * 1000.0) as usize / FRAME_MS).max(QUIET_FRAMES * 2);
    let mut out = Vec::new();
    let mut start = 0usize; // in frames
    while (energy.len() - start) > max_frames {
        let lo = start + max_frames * 2 / 3;
        let hi = start + max_frames - QUIET_FRAMES;
        // quietest window of QUIET_FRAMES frames starting in lo..=hi
        let cut = (lo..=hi)
            .min_by(|&a, &b| {
                let ea: f64 = energy[a..a + QUIET_FRAMES].iter().sum();
                let eb: f64 = energy[b..b + QUIET_FRAMES].iter().sum();
                ea.total_cmp(&eb)
            })
            .map_or(start + max_frames, |w| w + QUIET_FRAMES / 2);
        out.push(start * frame..(cut * frame).min(n));
        start = cut;
    }
    out.push(start * frame..n);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(ms: usize, amp: f32) -> Vec<i16> {
        (0..16 * ms).map(|i| (amp * (i as f32 * 0.17).sin()) as i16).collect()
    }

    #[test]
    fn short_audio_is_one_piece() {
        assert_eq!(split(&tone(5_000, 3000.0), 16_000, 28.0), vec![0..80_000]);
        assert_eq!(split(&[], 16_000, 28.0), vec![0..0]);
        assert_eq!(split(&tone(100_000, 1.0), 16_000, f32::INFINITY).len(), 1, "streaming models are never split");
    }

    #[test]
    fn long_audio_is_cut_in_the_pauses() {
        // 22 s of speech, a 400 ms pause, 22 s of speech, a pause, 10 s of speech
        let mut pcm = tone(22_000, 3000.0);
        let pause1 = pcm.len();
        pcm.extend(vec![0i16; 16 * 400]);
        pcm.extend(tone(22_000, 3000.0));
        let pause2 = pcm.len();
        pcm.extend(vec![0i16; 16 * 400]);
        pcm.extend(tone(10_000, 3000.0));
        let parts = split(&pcm, 16_000, 28.0);
        assert_eq!(parts.len(), 3, "{parts:?}");
        assert_eq!(parts.first().unwrap().start, 0);
        assert_eq!(parts.last().unwrap().end, pcm.len());
        for w in parts.windows(2) {
            assert_eq!(w[0].end, w[1].start, "contiguous");
        }
        for r in &parts {
            assert!(r.len() as f32 <= 28.0 * 16_000.0, "{r:?} is too long");
        }
        let in_pause = |at: usize, p: usize| at >= p && at <= p + 16 * 400;
        assert!(in_pause(parts[0].end, pause1), "first cut {} is in the pause at {pause1}", parts[0].end);
        assert!(in_pause(parts[1].end, pause2), "second cut {} is in the pause at {pause2}", parts[1].end);
    }

    #[test]
    fn speech_without_pauses_is_still_bounded() {
        let pcm = tone(95_000, 3000.0);
        let parts = split(&pcm, 16_000, 28.0);
        assert!(parts.len() >= 4);
        assert!(parts.iter().all(|r| r.len() as f32 <= 28.0 * 16_000.0 && !r.is_empty()));
        assert_eq!(parts.iter().map(|r| r.len()).sum::<usize>(), pcm.len());
    }
}
