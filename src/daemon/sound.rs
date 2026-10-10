//! Dictation cues synthesised once (OpenWhispr dictationCues.js): start C5 523.25 -> E5 659.25 Hz,
//! stop D5 587.33 -> A4 440 Hz; each note 90 ms, 25 ms gap, 5 ms lead, attack 15 ms linear to gain
//! 0.2, exponential decay to 0.0001 at 90 ms. Rendered to in-memory WAVs, played with
//! PlaySoundW(SND_MEMORY | SND_ASYNC | SND_NODEFAULT). TASK win-input implements.

use std::sync::OnceLock;
use std::sync::mpsc::{Sender, channel};

const START_NOTES: [f32; 2] = [523.25, 659.25];
const STOP_NOTES: [f32; 2] = [587.33, 440.0];
const SAMPLE_RATE: u32 = 44_100;

const NOTE_DURATION: f64 = 0.09;
const NOTE_GAP: f64 = 0.025;
const NOTE_ATTACK: f64 = 0.015;
/// Web Audio schedules the oscillator to stop 10 ms after the gain ramp ends.
const NOTE_TAIL: f64 = 0.01;
/// First note starts 5 ms after "now" (dictationCues.js `context.currentTime + 0.005`).
const LEAD: f64 = 0.005;
const MAX_GAIN: f64 = 0.2;
const MIN_GAIN: f64 = 0.0001;

struct Cues {
    /// 20 ms of silence, played once by [`init`] to load winmm and open the audio path.
    warmup: Vec<u8>,
    start: Vec<u8>,
    stop: Vec<u8>,
}

static CUES: OnceLock<Cues> = OnceLock::new();

fn cues() -> &'static Cues {
    CUES.get_or_init(|| Cues {
        warmup: wav_bytes(&[0i16; 882], SAMPLE_RATE),
        start: wav_bytes(&synth(&START_NOTES, SAMPLE_RATE), SAMPLE_RATE),
        stop: wav_bytes(&synth(&STOP_NOTES, SAMPLE_RATE), SAMPLE_RATE),
    })
}

/// Render both cues now and queue 20 ms of silence so the first hotkey press neither synthesises
/// nor pays PlaySound's one-time cost (loading winmm, opening the device: ~12 ms measured). The
/// silence plays on the cue thread, ahead of any cue asked for later, so this returns after the
/// synthesis only (a few ms) and is safe to call from the UI thread.
pub fn init() {
    static WARMED: std::sync::Once = std::sync::Once::new();
    let _ = cues();
    WARMED.call_once(|| post(Cue::Warm));
}

/// How long the start cue sounds: the lead, the notes (one pitch apart) and the last note's tail.
/// "Mute system audio" waits this long before muting so the cue is still heard.
pub fn start_cue_duration() -> std::time::Duration {
    let pitch = NOTE_DURATION + NOTE_GAP;
    std::time::Duration::from_secs_f64(LEAD + (START_NOTES.len() - 1) as f64 * pitch + NOTE_DURATION + NOTE_TAIL)
}

/// Play the start cue. Returns at once: the first `PlaySound` of a process can take tens of ms (it
/// loads winmm and opens the audio path), and this is called on the UI thread just as the pill's
/// entrance animation begins.
pub fn play_start() {
    post(Cue::Start);
}

/// Play the stop cue (returns at once, see [`play_start`]).
pub fn play_stop() {
    post(Cue::Stop);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Cue {
    Warm,
    Start,
    Stop,
}

/// `PlaySound` is process-global (a new sound cuts off the one playing), so one thread plays every
/// cue, in the order they were asked for.
static PLAYER: OnceLock<Option<Sender<Cue>>> = OnceLock::new();

fn play_cue(c: Cue) {
    play(match c {
        Cue::Warm => &cues().warmup,
        Cue::Start => &cues().start,
        Cue::Stop => &cues().stop,
    });
}

/// A thread that plays what it is sent, in order, with `play`.
fn spawn_player(play: impl Fn(Cue) + Send + 'static) -> Option<Sender<Cue>> {
    let (tx, rx) = channel::<Cue>();
    std::thread::Builder::new()
        .name("cue-player".into())
        .spawn(move || {
            while let Ok(c) = rx.recv() {
                play(c);
            }
        })
        .ok()
        .map(|_| tx)
}

fn post(c: Cue) {
    let tx = PLAYER.get_or_init(|| spawn_player(play_cue));
    // no thread (or it died): play here rather than lose the cue
    if !tx.as_ref().is_some_and(|tx| tx.send(c).is_ok()) {
        play_cue(c);
    }
}

fn play(wav: &'static [u8]) {
    use windows::Win32::Media::Audio::{PlaySoundW, SND_ASYNC, SND_MEMORY, SND_NODEFAULT};
    // SAFETY: with SND_MEMORY the "name" is a pointer to an in-memory WAV image. It must stay
    // valid while playing asynchronously; `wav` is `'static` (lives in the CUES OnceLock).
    let _ = unsafe {
        PlaySoundW(windows::core::PCWSTR(wav.as_ptr().cast()), None, SND_MEMORY | SND_ASYNC | SND_NODEFAULT)
    };
}

/// Pure synthesis (unit-testable): mono i16 samples at `rate`.
///
/// Mirrors the Web Audio graph of dictationCues.js: one sine oscillator per note feeding a gain
/// node that starts at 0.0001, ramps linearly to 0.2 over 15 ms, then exponentially back to
/// 0.0001 at 90 ms; notes start `0.09 + 0.025` s apart after a 5 ms lead.
pub fn synth(notes: &[f32], rate: u32) -> Vec<i16> {
    if notes.is_empty() || rate == 0 {
        return Vec::new();
    }
    let sr = f64::from(rate);
    let pitch = NOTE_DURATION + NOTE_GAP;
    let total = LEAD + (notes.len() - 1) as f64 * pitch + NOTE_DURATION + NOTE_TAIL;
    let mut mix = vec![0f64; (total * sr).ceil() as usize];
    let note_len = ((NOTE_DURATION + NOTE_TAIL) * sr).round() as usize;

    for (i, &freq) in notes.iter().enumerate() {
        let first = ((LEAD + i as f64 * pitch) * sr).round() as usize;
        let w = 2.0 * std::f64::consts::PI * f64::from(freq);
        for k in 0..note_len {
            let Some(slot) = mix.get_mut(first + k) else { break };
            let t = k as f64 / sr;
            let gain = if t < NOTE_ATTACK {
                MIN_GAIN + (MAX_GAIN - MIN_GAIN) * (t / NOTE_ATTACK)
            } else if t < NOTE_DURATION {
                MAX_GAIN * (MIN_GAIN / MAX_GAIN).powf((t - NOTE_ATTACK) / (NOTE_DURATION - NOTE_ATTACK))
            } else {
                MIN_GAIN
            };
            *slot += (w * t).sin() * gain;
        }
    }
    mix.iter().map(|&s| (s * f64::from(i16::MAX)).round().clamp(-32768.0, 32767.0) as i16).collect()
}

/// 44-byte RIFF/WAVE header + 16-bit mono PCM data.
fn wav_bytes(samples: &[i16], rate: u32) -> Vec<u8> {
    let data_len = (samples.len() * 2) as u32;
    let mut b = Vec::with_capacity(44 + samples.len() * 2);
    b.extend_from_slice(b"RIFF");
    b.extend_from_slice(&(36 + data_len).to_le_bytes());
    b.extend_from_slice(b"WAVEfmt ");
    b.extend_from_slice(&16u32.to_le_bytes()); // fmt chunk size
    b.extend_from_slice(&1u16.to_le_bytes()); // PCM
    b.extend_from_slice(&1u16.to_le_bytes()); // mono
    b.extend_from_slice(&rate.to_le_bytes());
    b.extend_from_slice(&(rate * 2).to_le_bytes()); // byte rate
    b.extend_from_slice(&2u16.to_le_bytes()); // block align
    b.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
    b.extend_from_slice(b"data");
    b.extend_from_slice(&data_len.to_le_bytes());
    for s in samples {
        b.extend_from_slice(&s.to_le_bytes());
    }
    b
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secs(n: usize) -> f64 {
        n as f64 / f64::from(SAMPLE_RATE)
    }
    fn at(t: f64) -> usize {
        (t * f64::from(SAMPLE_RATE)).round() as usize
    }

    /// Frequency estimate from rising zero crossings inside `[from, to)` samples.
    fn zc_freq(s: &[i16], from: usize, to: usize) -> f64 {
        let mut first = None;
        let mut last = 0usize;
        let mut n = 0;
        for i in from.max(1)..to.min(s.len()) {
            if s[i - 1] < 0 && s[i] >= 0 {
                if first.is_none() {
                    first = Some(i);
                } else {
                    n += 1;
                }
                last = i;
            }
        }
        let first = first.expect("no zero crossings");
        n as f64 / ((last - first) as f64 / f64::from(SAMPLE_RATE))
    }

    fn peak(s: &[i16]) -> i32 {
        s.iter().map(|&v| i32::from(v).abs()).max().unwrap_or(0)
    }

    /// Cues are queued, not played inline: asking returns at once even when playing is slow, and the
    /// order of the requests is the order of the sounds.
    #[test]
    fn cues_play_on_their_own_thread_in_request_order() {
        use std::sync::{Arc, Mutex};
        use std::time::{Duration, Instant};
        let log: Arc<Mutex<Vec<(Cue, std::thread::ThreadId)>>> = Arc::new(Mutex::new(Vec::new()));
        let l = log.clone();
        let tx = spawn_player(move |c| {
            std::thread::sleep(Duration::from_millis(40)); // a slow first PlaySound
            l.lock().unwrap().push((c, std::thread::current().id()));
        })
        .expect("player");
        let t0 = Instant::now();
        for c in [Cue::Start, Cue::Stop, Cue::Start] {
            tx.send(c).unwrap();
        }
        assert!(t0.elapsed() < Duration::from_millis(30), "asking for a cue blocked for {:?}", t0.elapsed());
        let deadline = Instant::now() + Duration::from_secs(3);
        while log.lock().unwrap().len() < 3 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        let got = log.lock().unwrap().clone();
        assert_eq!(got.iter().map(|g| g.0).collect::<Vec<_>>(), [Cue::Start, Cue::Stop, Cue::Start]);
        assert!(got.iter().all(|g| g.1 != std::thread::current().id()), "played off the calling thread");
    }

    #[test]
    fn duration_matches_schedule() {
        // 5 ms lead + 2 notes spaced 115 ms + last note 90 ms + 10 ms oscillator tail.
        let s = synth(&START_NOTES, SAMPLE_RATE);
        let expect = 0.005 + 0.115 + 0.09 + 0.01;
        assert!((secs(s.len()) - expect).abs() < 0.001, "{} vs {expect}", secs(s.len()));
        let one = synth(&[440.0], SAMPLE_RATE);
        assert!((secs(one.len()) - 0.105).abs() < 0.001);
    }

    #[test]
    fn start_cue_duration_matches_the_rendered_cue() {
        let rendered = secs(synth(&START_NOTES, SAMPLE_RATE).len());
        let d = start_cue_duration().as_secs_f64();
        assert!((d - rendered).abs() < 0.001, "{d} vs {rendered}");
    }

    #[test]
    fn empty_inputs() {
        assert!(synth(&[], SAMPLE_RATE).is_empty());
        assert!(synth(&[440.0], 0).is_empty());
    }

    #[test]
    fn lead_is_silent() {
        let s = synth(&START_NOTES, SAMPLE_RATE);
        assert!(s[..at(0.005) - 1].iter().all(|&v| v == 0));
    }

    #[test]
    fn peak_gain_is_point_two() {
        for notes in [&START_NOTES[..], &STOP_NOTES[..]] {
            let s = synth(notes, SAMPLE_RATE);
            let p = f64::from(peak(&s)) / f64::from(i16::MAX);
            // Attack peaks at 0.2; the sine may not sample the exact crest.
            assert!((0.18..=0.2001).contains(&p), "peak {p}");
        }
    }

    #[test]
    fn envelope_attacks_then_decays_exponentially() {
        let s = synth(&[440.0], SAMPLE_RATE);
        let win = |a: f64, b: f64| f64::from(peak(&s[at(a)..at(b)]));
        let early = win(0.005, 0.0075); // first 2.5 ms of attack
        let top = win(0.015, 0.022); // around the 15 ms crest
        let mid = win(0.05, 0.057);
        let late = win(0.088, 0.094);
        assert!(early < top * 0.5, "attack: {early} vs {top}");
        assert!(mid < top * 0.3, "decay: {mid} vs {top}");
        assert!(late < mid * 0.1, "late: {late} vs {mid}");
        // The window opens 45 ms into the note: 0.2 * 0.0005^((45-15)/75) ~= 0.0096 -> ~314 LSB.
        let expect = 0.2 * (0.0001f64 / 0.2).powf((0.045 - 0.015) / 0.075) * f64::from(i16::MAX);
        assert!((mid / expect - 1.0).abs() < 0.2, "mid {mid} expect {expect}");
    }

    #[test]
    fn second_note_starts_after_gap() {
        let s = synth(&START_NOTES, SAMPLE_RATE);
        // End of note 1 (t = 5 + 90 = 95 ms) up to note 2 start (120 ms): only the sub-LSB tail.
        let gap = &s[at(0.0975)..at(0.1195)];
        assert!(peak(gap) <= 4, "gap not silent: {}", peak(gap));
        // Note 2 reaches full level at 120 + 15 ms.
        assert!(peak(&s[at(0.134)..at(0.141)]) > 5000);
    }

    #[test]
    fn note_frequencies() {
        let start = synth(&START_NOTES, SAMPLE_RATE);
        let stop = synth(&STOP_NOTES, SAMPLE_RATE);
        let check = |s: &[i16], note: usize, want: f64| {
            let t0 = 0.005 + note as f64 * 0.115;
            let f = zc_freq(s, at(t0 + 0.002), at(t0 + 0.06));
            assert!((f / want - 1.0).abs() < 0.02, "{f} vs {want}");
        };
        check(&start, 0, 523.25);
        check(&start, 1, 659.25);
        check(&stop, 0, 587.33);
        check(&stop, 1, 440.0);
    }

    #[test]
    fn wav_header_is_valid() {
        let s = synth(&START_NOTES, SAMPLE_RATE);
        let w = wav_bytes(&s, SAMPLE_RATE);
        assert_eq!(&w[0..4], b"RIFF");
        assert_eq!(&w[8..16], b"WAVEfmt ");
        assert_eq!(u32::from_le_bytes(w[4..8].try_into().unwrap()) as usize, w.len() - 8);
        assert_eq!(u16::from_le_bytes(w[20..22].try_into().unwrap()), 1, "PCM");
        assert_eq!(u16::from_le_bytes(w[22..24].try_into().unwrap()), 1, "mono");
        assert_eq!(u32::from_le_bytes(w[24..28].try_into().unwrap()), SAMPLE_RATE);
        assert_eq!(u16::from_le_bytes(w[34..36].try_into().unwrap()), 16);
        assert_eq!(&w[36..40], b"data");
        assert_eq!(u32::from_le_bytes(w[40..44].try_into().unwrap()) as usize, s.len() * 2);
        assert_eq!(w.len(), 44 + s.len() * 2);
    }

    #[test]
    fn cues_are_cached_and_distinct() {
        // `cues()`, not `init()`: init plays through the audio device (desktop tests only).
        let a = cues() as *const Cues;
        assert_eq!(a, cues() as *const Cues);
        assert_ne!(cues().start, cues().stop);
    }

    #[test]
    #[ignore = "desktop: plays both cues through the default audio device"]
    fn desktop_sound_plays() {
        init();
        std::thread::sleep(std::time::Duration::from_millis(100));
        let t = std::time::Instant::now();
        play_start();
        let took = t.elapsed();
        // init() paid the one-time winmm/device cost (~12 ms); a cue on the hotkey path must not.
        assert!(took < std::time::Duration::from_millis(5), "first cue blocked {took:?}");
        std::thread::sleep(std::time::Duration::from_millis(400));
        play_stop();
        std::thread::sleep(std::time::Duration::from_millis(400));
    }
}
