//! Streaming polyphase windowed-sinc resampler (mono f32 -> mono f32) used when the OS cannot
//! convert to 16 kHz itself (AUTOCONVERTPCM refused) and for non-16 kHz injected WAVs.
//!
//! Output sample `n` is centred on input position `n * in/out`. With `g = gcd(in, out)` the
//! fractional position takes only `out/g` distinct phases, so each phase gets a precomputed,
//! DC-normalised Blackman-windowed sinc kernel and the hot loop is a plain dot product.
use std::f64::consts::PI;

/// Zero crossings of the sinc kept on each side of the centre tap.
const ZERO_CROSSINGS: f64 = 8.0;
/// Cutoff (cycles per input sample) is this fraction of the lower Nyquist rate.
const CUTOFF: f64 = 0.45;

pub struct Resampler {
    phases: usize,
    step: usize,
    /// Kernel reaches `half - 1` samples before and `half` after the centre sample.
    half: usize,
    taps: usize,
    /// `phases * taps` kernel coefficients, one row per phase.
    table: Vec<f32>,
    /// Input history; `buf[0]` is the oldest sample still needed.
    buf: Vec<f32>,
    /// Next output centre, in 1/`phases` input samples relative to `buf[0]`.
    pos: usize,
}

fn gcd(a: usize, b: usize) -> usize {
    if b == 0 { a } else { gcd(b, a % b) }
}

fn blackman(x: f64) -> f64 {
    // x in [-1, 1]
    0.42 + 0.5 * (PI * x).cos() + 0.08 * (2.0 * PI * x).cos()
}

fn sinc(x: f64) -> f64 {
    if x.abs() < 1e-12 { 1.0 } else { (PI * x).sin() / (PI * x) }
}

impl Resampler {
    /// `None` when no conversion is needed or a rate is zero.
    pub fn new(rate_in: u32, rate_out: u32) -> Option<Resampler> {
        if rate_in == 0 || rate_out == 0 || rate_in == rate_out {
            return None;
        }
        let g = gcd(rate_in as usize, rate_out as usize);
        let phases = rate_out as usize / g;
        let step = rate_in as usize / g;
        // Cutoff in cycles per input sample (half of 1.0 is the input Nyquist).
        let fc = CUTOFF * (rate_out as f64 / rate_in as f64).min(1.0);
        let half = ((ZERO_CROSSINGS / (2.0 * fc)).ceil() as usize).max(2);
        let taps = 2 * half;
        let mut table = vec![0f32; phases * taps];
        let mut tmp = vec![0f64; taps];
        for p in 0..phases {
            let frac = p as f64 / phases as f64;
            let mut sum = 0.0f64;
            for (k, slot) in tmp.iter_mut().enumerate() {
                let j = k as f64 - (half as f64 - 1.0);
                let t = j - frac;
                let v = if t.abs() >= half as f64 { 0.0 } else { 2.0 * fc * sinc(2.0 * fc * t) * blackman(t / half as f64) };
                *slot = v;
                sum += v;
            }
            let norm = if sum.abs() > 1e-12 { 1.0 / sum } else { 1.0 };
            for (dst, v) in table[p * taps..(p + 1) * taps].iter_mut().zip(&tmp) {
                *dst = (v * norm) as f32;
            }
        }
        // Prime with silence so the first outputs see zeros before t = 0.
        let lead = half - 1;
        Some(Resampler { phases, step, half, taps, table, buf: vec![0.0; lead], pos: lead * phases })
    }

    /// Feed input samples, appending every output sample that is now computable to `out`.
    pub fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        self.buf.extend_from_slice(input);
        loop {
            let i = self.pos / self.phases;
            if i + self.half >= self.buf.len() {
                break;
            }
            let p = self.pos % self.phases;
            let start = i + 1 - self.half;
            let window = &self.buf[start..start + self.taps];
            let row = &self.table[p * self.taps..(p + 1) * self.taps];
            let mut acc = 0.0f32;
            for (a, b) in window.iter().zip(row) {
                acc += a * b;
            }
            out.push(acc);
            self.pos += self.step;
        }
        // Drop history that no future output can reach.
        let keep_from = (self.pos / self.phases + 1).saturating_sub(self.half).min(self.buf.len());
        if keep_from > 0 {
            self.buf.drain(..keep_from);
            self.pos -= keep_from * self.phases;
        }
    }

    /// Flush the tail by feeding silence, so the last input samples are fully represented.
    pub fn finish(&mut self, out: &mut Vec<f32>) {
        let zeros = vec![0.0f32; self.half];
        self.process(&zeros, out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(rate: u32, hz: f64, secs: f64, amp: f64) -> Vec<f32> {
        (0..(rate as f64 * secs) as usize).map(|n| (amp * (2.0 * PI * hz * n as f64 / rate as f64).sin()) as f32).collect()
    }

    fn rms(v: &[f32]) -> f64 {
        (v.iter().map(|x| (*x as f64).powi(2)).sum::<f64>() / v.len().max(1) as f64).sqrt()
    }

    fn run(rate_in: u32, input: &[f32]) -> Vec<f32> {
        let mut r = Resampler::new(rate_in, 16_000).unwrap();
        let mut out = Vec::new();
        r.process(input, &mut out);
        r.finish(&mut out);
        out
    }

    #[test]
    fn same_rate_needs_no_resampler() {
        assert!(Resampler::new(16_000, 16_000).is_none());
        assert!(Resampler::new(0, 16_000).is_none());
    }

    #[test]
    fn output_length_matches_ratio() {
        for rate in [48_000u32, 44_100, 32_000, 22_050, 8_000, 96_000, 11_025] {
            let out = run(rate, &sine(rate, 440.0, 1.0, 0.5));
            assert!((out.len() as i64 - 16_000).abs() <= 80, "rate {rate}: {} samples", out.len());
        }
    }

    #[test]
    fn passband_tone_keeps_amplitude() {
        for rate in [48_000u32, 44_100, 8_000] {
            let out = run(rate, &sine(rate, 1_000.0, 1.0, 0.5));
            let mid = &out[2_000..out.len() - 2_000];
            let expect = 0.5 / 2f64.sqrt();
            let got = rms(mid);
            assert!((got - expect).abs() / expect < 0.01, "rate {rate}: rms {got} vs {expect}");
        }
    }

    #[test]
    fn tone_frequency_is_preserved() {
        let out = run(48_000, &sine(48_000, 1_000.0, 1.0, 0.5));
        let mid = &out[2_000..out.len() - 2_000];
        let crossings = mid.windows(2).filter(|w| w[0] <= 0.0 && w[1] > 0.0).count();
        let hz = crossings as f64 / (mid.len() as f64 / 16_000.0);
        assert!((hz - 1_000.0).abs() < 5.0, "estimated {hz} Hz");
    }

    #[test]
    fn stopband_tone_is_rejected() {
        // 12 kHz would alias to 4 kHz at 16 kHz output; the filter must remove it.
        for (rate, hz) in [(48_000u32, 12_000.0), (44_100, 12_000.0), (48_000, 10_000.0)] {
            let out = run(rate, &sine(rate, hz, 1.0, 0.5));
            let mid = &out[2_000..out.len() - 2_000];
            let att_db = 20.0 * (rms(mid) / (0.5 / 2f64.sqrt())).log10();
            assert!(att_db < -50.0, "rate {rate} tone {hz}: only {att_db:.1} dB");
        }
    }

    #[test]
    fn chunking_does_not_change_output() {
        let input = sine(44_100, 733.0, 0.5, 0.4);
        let whole = run(44_100, &input);
        let mut r = Resampler::new(44_100, 16_000).unwrap();
        let mut out = Vec::new();
        let (mut i, mut n) = (0, 1);
        while i < input.len() {
            let end = (i + n).min(input.len());
            r.process(&input[i..end], &mut out);
            i = end;
            n = n * 3 % 997 + 1;
        }
        r.finish(&mut out);
        assert_eq!(whole, out);
    }

    #[test]
    fn upsampling_works() {
        let mut r = Resampler::new(8_000, 16_000).unwrap();
        let mut out = Vec::new();
        r.process(&sine(8_000, 500.0, 1.0, 0.5), &mut out);
        r.finish(&mut out);
        let mid = &out[2_000..out.len() - 2_000];
        assert!((rms(mid) - 0.5 / 2f64.sqrt()).abs() < 0.005);
    }
}
