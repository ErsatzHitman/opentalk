//! Dictation cues synthesised once (OpenWhispr dictationCues.js): start C5 523.25 -> E5 659.25 Hz,
//! stop D5 587.33 -> A4 440 Hz; each note 90 ms, 25 ms gap, 5 ms lead, attack 15 ms linear to gain
//! 0.2, exponential decay to 0.0001 at 90 ms. Rendered to in-memory WAVs, played with
//! PlaySoundW(SND_MEMORY | SND_ASYNC | SND_NODEFAULT). TASK win-input implements.

pub fn init() {
    todo!("win-input")
}
pub fn play_start() {
    todo!("win-input")
}
pub fn play_stop() {
    todo!("win-input")
}
/// Pure synthesis (unit-testable): mono i16 samples at `rate`.
pub fn synth(notes: &[f32], rate: u32) -> Vec<i16> {
    let _ = (notes, rate);
    todo!("win-input")
}
