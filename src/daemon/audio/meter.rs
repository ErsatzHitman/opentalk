//! Streaming microphone level meter: turns 16 kHz mono PCM into the display level the pill's
//! waveform shows (0.0 .. 1.0), with a noise gate that makes silence exactly 0.0.
//!
//! Why this exists (research-waveform.md): the old path published the RMS of the latest 43 ms and
//! the pill sampled it once per 80 ms. A real microphone's room noise moved that value on nearly
//! every packet, so the picture never settled, the animation timer never slowed, and speech onsets
//! reached half bar height ~80-115 ms late. Here the decisions are made once, in the audio path,
//! in the dB domain, and the pill only has to draw an envelope of the result.
//!
//! Pipeline, per sample then per 10 ms hop:
//! 1. one-pole high-pass (~70 Hz): removes DC offset and rumble so a biased cheap USB mic is still
//!    digitally quiet;
//! 2. power over the latest 20 ms (320 samples), evaluated every 10 ms (160 samples) whatever the
//!    packet size, so the gate timing does not depend on how WASAPI chops the stream;
//! 3. adaptive noise floor in dB: snaps down at once, creeps up (6 dB/s while the gate is shut,
//!    1.5 dB/s while it is open, so speech cannot drag it up), clamped to -66 .. -38 dBFS; during
//!    the first 300 ms it only tracks downwards (a seed taken from the room, not from a hot mic);
//! 4. gate with hysteresis: opens above `max(floor + 9 dB, -48 dBFS)`, closes after 150 ms below
//!    `max(floor + 5 dB, -52 dBFS)`; a closed gate publishes exactly 0.0;
//! 5. bar level: OpenWhispr's `min(1, (rms * 8)^0.75)` of the latest 20 ms, with a dead zone under
//!    -48 dBFS so a quiet room is a flat line. The gate, floor and reference above only feed the
//!    per-recording diagnostics.
//!
//! Pure and allocation-free after construction: unit-tested without audio hardware.

/// Samples per analysis hop (10 ms at 16 kHz).
pub const HOP: usize = 160;
/// Samples in the analysis window (20 ms).
pub const WINDOW: usize = 320;
const HOP_MS: f32 = 10.0;

/// High-pass corner: `1 - 2 pi f / fs` for 70 Hz at 16 kHz.
const HPF_R: f32 = 0.9725;

/// The floor only tracks downwards for this long after the start.
const SEED_HOPS: u32 = 30;
pub const FLOOR_MIN_DB: f32 = -66.0;
pub const FLOOR_MAX_DB: f32 = -38.0;
/// Floor creep while the gate is shut / open (dB per second).
const FLOOR_RISE_SHUT_DB_S: f32 = 6.0;
const FLOOR_RISE_OPEN_DB_S: f32 = 1.5;

pub const OPEN_ABOVE_FLOOR_DB: f32 = 9.0;
pub const CLOSE_ABOVE_FLOOR_DB: f32 = 5.0;
pub const OPEN_MIN_DB: f32 = -48.0;
pub const CLOSE_MIN_DB: f32 = -52.0;
/// How long the level must stay under the close threshold before the gate shuts.
pub const HOLD_MS: f32 = 150.0;

/// Display reference: the level that fills the bars. Follows the running speech peak.
pub const REF_MIN_DB: f32 = -34.0;
pub const REF_MAX_DB: f32 = -10.0;
const REF_ATTACK_MS: f32 = 50.0;
const REF_DECAY_DB_S: f32 = 3.0;

/// RMS under this is room noise: the bars stay flat (about -48 dBFS).
pub const DEAD_ZONE_RMS: f32 = 0.004;
/// OpenWhispr's mapping (`waveformMath.ts`): conversational speech RMS is about 0.02 - 0.15; the gain
/// puts loud voicing at the top of the lane and the 0.75 exponent keeps quiet speech visible while
/// leaving contrast between neighbouring bars.
const LEVEL_GAIN: f32 = 8.0;
const LEVEL_EXPONENT: f32 = 0.75;

/// Bar level 0..=1 of an RMS amplitude (full scale 1.0); exactly 0.0 inside the dead zone.
pub fn bar_level(rms: f32) -> f32 {
    if !rms.is_finite() || rms < DEAD_ZONE_RMS {
        return 0.0;
    }
    (rms * LEVEL_GAIN).powf(LEVEL_EXPONENT).min(1.0)
}

/// Result of one 10 ms hop.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Hop {
    /// Bar level in 0..=1 ([`bar_level`] of the latest 20 ms); exactly 0.0 in the dead zone.
    pub level: f32,
    /// The gate is open.
    pub open: bool,
    /// Power of the latest 20 ms in dBFS (-120 for digital silence).
    pub db: f32,
}

/// Running totals for the per-recording diagnostics line.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct MeterStats {
    pub hops: u64,
    pub open_hops: u64,
    /// Noise floor now, dBFS.
    pub floor_db: f32,
}

pub struct Meter {
    x1: f32,
    y1: f32,
    primed: bool,
    /// Squares of the high-passed signal over the window.
    sq: [f32; WINDOW],
    pos: usize,
    filled: usize,
    acc: usize,
    hop_no: u32,
    floor_db: f32,
    ref_db: f32,
    open: bool,
    below_ms: f32,
    hops: u64,
    open_hops: u64,
}

impl Default for Meter {
    fn default() -> Self {
        Self::new()
    }
}

#[inline]
fn power_db(mean_square: f32) -> f32 {
    10.0 * (mean_square + 1e-12).log10()
}

impl Meter {
    pub fn new() -> Meter {
        Meter {
            x1: 0.0,
            y1: 0.0,
            primed: false,
            sq: [0.0; WINDOW],
            pos: 0,
            filled: 0,
            acc: 0,
            hop_no: 0,
            floor_db: FLOOR_MAX_DB,
            ref_db: REF_MIN_DB,
            open: false,
            below_ms: 0.0,
            hops: 0,
            open_hops: 0,
        }
    }

    /// Feed PCM of any length; `emit` is called once per completed 10 ms hop.
    pub fn push(&mut self, pcm: &[i16], mut emit: impl FnMut(Hop)) {
        for &s in pcm {
            let x = s as f32 * (1.0 / 32768.0);
            // first sample primes the filter, so a constant offset gives exactly 0 from the start
            let y = if self.primed { x - self.x1 + HPF_R * self.y1 } else { 0.0 };
            self.primed = true;
            self.x1 = x;
            self.y1 = y;
            self.sq[self.pos] = y * y;
            self.pos = if self.pos + 1 == WINDOW { 0 } else { self.pos + 1 };
            self.filled = (self.filled + 1).min(WINDOW);
            self.acc += 1;
            if self.acc == HOP {
                self.acc = 0;
                emit(self.hop());
            }
        }
    }

    pub fn stats(&self) -> MeterStats {
        MeterStats { hops: self.hops, open_hops: self.open_hops, floor_db: self.floor_db }
    }

    /// Open threshold (dBFS) for the current floor.
    pub fn open_db(&self) -> f32 {
        (self.floor_db + OPEN_ABOVE_FLOOR_DB).max(OPEN_MIN_DB)
    }

    /// Close threshold (dBFS) for the current floor.
    pub fn close_db(&self) -> f32 {
        (self.floor_db + CLOSE_ABOVE_FLOOR_DB).max(CLOSE_MIN_DB)
    }

    fn hop(&mut self) -> Hop {
        let sum: f32 = self.sq[..self.filled.min(WINDOW)].iter().sum();
        let db = power_db(sum / self.filled.max(1) as f32);
        self.hop_no = self.hop_no.saturating_add(1);
        let dt = HOP_MS / 1000.0;

        // noise floor
        if db < self.floor_db {
            self.floor_db = db.max(FLOOR_MIN_DB);
        } else if self.hop_no > SEED_HOPS {
            let rate = if self.open { FLOOR_RISE_OPEN_DB_S } else { FLOOR_RISE_SHUT_DB_S };
            self.floor_db = (self.floor_db + rate * dt).min(db);
        }
        self.floor_db = self.floor_db.clamp(FLOOR_MIN_DB, FLOOR_MAX_DB);

        // gate
        let (open_db, close_db) = (self.open_db(), self.close_db());
        if !self.open {
            if db > open_db {
                self.open = true;
                self.below_ms = 0.0;
            }
        } else if db < close_db {
            self.below_ms += HOP_MS;
            if self.below_ms >= HOLD_MS {
                self.open = false;
            }
        } else {
            self.below_ms = 0.0;
        }

        // display reference: a quick attack to speech peaks, a slow fall
        if self.open && db > self.ref_db {
            self.ref_db += (db - self.ref_db) * (1.0 - (-HOP_MS / REF_ATTACK_MS).exp());
        } else {
            self.ref_db -= REF_DECAY_DB_S * dt;
        }
        self.ref_db = self.ref_db.clamp(REF_MIN_DB, REF_MAX_DB);

        self.hops += 1;
        if self.open {
            self.open_hops += 1;
        }
        // The bar level is OpenWhispr's: straight from the RMS, no gate in the way (the gate and floor
        // above only feed the diagnostics). A quiet room sits under the dead zone and shows a flat line.
        Hop { level: bar_level(10f32.powf(db / 20.0)), open: self.open, db }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic noise source.
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> f32 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            ((self.0 >> 11) as f64 / (1u64 << 53) as f64) as f32 * 2.0 - 1.0
        }
    }

    fn db_to_rms(db: f32) -> f32 {
        10f32.powf(db / 20.0)
    }

    fn to_i16(x: f32) -> i16 {
        (x * 32768.0).round().clamp(-32768.0, 32767.0) as i16
    }

    /// White (uniform) noise at `db` dBFS RMS.
    fn white(db: f32, ms: usize, rng: &mut Rng) -> Vec<i16> {
        let a = db_to_rms(db) * 3f32.sqrt();
        (0..ms * 16).map(|_| to_i16(rng.next() * a)).collect()
    }

    /// Low-passed (AR(1), pole 0.9) noise at `db` dBFS RMS: room rumble rather than hiss.
    fn ar1(db: f32, ms: usize, rng: &mut Rng) -> Vec<i16> {
        let (p, mut y) = (0.9f32, 0.0f32);
        // unit-variance input -> output variance 1 / (1 - p^2)
        let gain = db_to_rms(db) * (1.0 - p * p).sqrt() * 3f32.sqrt();
        (0..ms * 16)
            .map(|_| {
                y = p * y + rng.next();
                to_i16(y * gain)
            })
            .collect()
    }

    fn sine(hz: f32, rms: f32, ms: usize) -> Vec<i16> {
        let a = rms * 2f32.sqrt();
        (0..ms * 16).map(|n| to_i16(a * (2.0 * std::f32::consts::PI * hz * n as f32 / 16_000.0).sin())).collect()
    }

    fn run(m: &mut Meter, pcm: &[i16], packet: usize) -> Vec<Hop> {
        let mut out = Vec::new();
        for c in pcm.chunks(packet) {
            m.push(c, |h| out.push(h));
        }
        out
    }

    #[test]
    fn digital_silence_is_exactly_zero_and_the_gate_stays_shut() {
        let mut m = Meter::new();
        let hops = run(&mut m, &vec![0i16; 16_000 * 5], 160);
        assert_eq!(hops.len(), 500);
        assert!(hops.iter().all(|h| h.level == 0.0 && !h.open), "digital silence must publish exactly 0.0");
        assert_eq!(m.stats().open_hops, 0);
        assert_eq!(m.stats().floor_db, FLOOR_MIN_DB);
    }

    #[test]
    fn stationary_room_noise_never_opens_the_gate() {
        for db in [-60.0f32, -50.0, -45.0, -40.0] {
            for kind in 0..2 {
                let mut rng = Rng(0x9E37_79B9_7F4A_7C15 ^ (db as i64 as u64));
                let pcm = if kind == 0 { white(db, 10_000, &mut rng) } else { ar1(db, 10_000, &mut rng) };
                let mut m = Meter::new();
                let hops = run(&mut m, &pcm, 160);
                let opened = hops.iter().filter(|h| h.open).count();
                let nonzero = hops.iter().filter(|h| h.level != 0.0).count();
                assert_eq!((opened, nonzero), (0, 0), "{db} dBFS {} noise: gate opened on {opened} hops", if kind == 0 { "white" } else { "AR(1)" });
            }
        }
    }

    #[test]
    fn dc_offset_is_removed() {
        let mut m = Meter::new();
        let hops = run(&mut m, &vec![300i16; 16_000 * 3], 160);
        assert!(hops.iter().all(|h| h.level == 0.0 && !h.open && h.db < -100.0), "a constant offset is not sound");
        // offset plus quiet hiss: the hiss decides, not the offset
        let mut rng = Rng(7);
        let pcm: Vec<i16> = white(-58.0, 5000, &mut rng).iter().map(|&s| s.saturating_add(300)).collect();
        let hops = run(&mut Meter::new(), &pcm, 160);
        assert!(hops.iter().all(|h| h.level == 0.0), "offset + hiss stays silent");
    }

    #[test]
    fn packet_size_does_not_change_the_result() {
        let mut rng = Rng(11);
        let mut pcm = white(-60.0, 1000, &mut rng);
        pcm.extend(sine(200.0, 0.05, 600));
        pcm.extend(white(-60.0, 800, &mut rng));
        let a = run(&mut Meter::new(), &pcm, 160);
        for packet in [1usize, 37, 160, 480, 1600, 7777] {
            let b = run(&mut Meter::new(), &pcm, packet);
            assert_eq!(a.len(), b.len(), "packet {packet}");
            assert!(a.iter().zip(&b).all(|(x, y)| x == y), "packet {packet}: hop results must not depend on packetisation");
        }
    }

    /// Speech-like bursts (150 ms tone, 100 ms gap) after half a second of room tone.
    fn bursts(rms: f32) -> (Vec<i16>, usize) {
        let mut rng = Rng(3);
        let mut pcm = white(-62.0, 500, &mut rng);
        let start_ms = 500;
        for _ in 0..6 {
            pcm.extend(sine(200.0, rms, 150));
            pcm.extend(white(-62.0, 100, &mut rng));
        }
        (pcm, start_ms)
    }

    #[test]
    fn bursts_open_the_gate_within_two_hops_and_fill_the_range() {
        for rms in [0.01f32, 0.03, 0.1, 0.2] {
            let (pcm, start_ms) = bursts(rms);
            let hops = run(&mut Meter::new(), &pcm, 160);
            let onset_hop = start_ms / 10;
            let opened = hops.iter().position(|h| h.open).expect("gate opens");
            assert!(opened <= onset_hop + 1, "rms {rms}: gate opened at hop {opened}, onset at {onset_hop} (>20 ms late)");
            let best = hops[onset_hop..].iter().map(|h| h.level).fold(0.0, f32::max);
            assert!(best >= 0.5, "rms {rms}: best level {best} should reach at least half the range");
            // the gate stays open through the 100 ms gaps (hold 150 ms), the level itself drops to 0 there
            let mid_gap = &hops[onset_hop + 15 + 5..onset_hop + 15 + 8];
            assert!(mid_gap.iter().all(|h| h.open && h.level == 0.0), "rms {rms}: gap shows {mid_gap:?}");
        }
    }

    #[test]
    fn the_gate_closes_after_the_hold_and_the_level_is_exactly_zero_again() {
        let (mut pcm, _) = bursts(0.05);
        let mut rng = Rng(5);
        pcm.extend(white(-62.0, 600, &mut rng));
        let mut m = Meter::new();
        let hops = run(&mut m, &pcm, 160);
        let last_loud = hops.iter().rposition(|h| h.db > -40.0).unwrap();
        let closed = last_loud + hops[last_loud..].iter().position(|h| !h.open).expect("closes");
        let hold_hops = (HOLD_MS / HOP_MS) as usize;
        // 20 ms for the window to drain + the hold
        assert!((last_loud + hold_hops..=last_loud + hold_hops + 4).contains(&closed), "closed {} hops after the last loud one", closed - last_loud);
        assert!(hops[closed..].iter().all(|h| h.level == 0.0 && !h.open));
    }

    #[test]
    fn quiet_and_hot_microphones_both_use_the_range() {
        // a quiet speaker (-45 dBFS) in a quiet room opens the gate and shows a clearly visible bar
        let mut rng = Rng(21);
        let mut pcm = white(-65.0, 600, &mut rng);
        pcm.extend(sine(180.0, db_to_rms(-45.0), 800));
        let hops = run(&mut Meter::new(), &pcm, 160);
        let seg = &hops[60..];
        assert!(seg.iter().any(|h| h.open), "a -45 dBFS speaker must open the gate");
        assert!(seg.iter().map(|h| h.level).fold(0.0, f32::max) >= 0.3, "a quiet speaker still draws a clearly visible bar");
        // a hot mic (-12 dBFS) is not stuck at the top for ever: the reference follows, and a drop of
        // 12 dB later shows as a clearly smaller bar
        let mut pcm = white(-65.0, 600, &mut rng);
        pcm.extend(sine(180.0, db_to_rms(-12.0), 1500));
        pcm.extend(sine(180.0, db_to_rms(-24.0), 400));
        let hops = run(&mut Meter::new(), &pcm, 160);
        let loud = hops[60 + 100..60 + 148].iter().map(|h| h.level).fold(0.0, f32::max);
        let softer = hops.last().unwrap().level;
        assert!(loud > 0.95, "loud {loud}");
        assert!(softer < loud - 0.2 && softer > 0.0, "softer {softer} vs loud {loud}");
    }

    #[test]
    fn the_floor_follows_a_noise_step_and_the_gate_closes_again() {
        let mut rng = Rng(99);
        let mut pcm = white(-60.0, 2000, &mut rng);
        let step_hop = 200;
        pcm.extend(white(-45.0, 12_000, &mut rng));
        let mut m = Meter::new();
        let hops = run(&mut m, &pcm, 160);
        assert!(hops[..step_hop].iter().all(|h| !h.open), "the quiet room is silent");
        assert!(hops[step_hop..step_hop + 10].iter().any(|h| h.open), "the step looks like sound first");
        let closed = step_hop + hops[step_hop..].iter().position(|h| !h.open).expect("floor catches up");
        let secs = (closed - step_hop) as f32 / 100.0;
        assert!(secs <= 10.0, "took {secs} s to accept the new noise as the floor");
        assert!(hops[closed..].iter().all(|h| h.level == 0.0), "and stays flat");
        assert!(m.stats().floor_db > -52.0 && m.stats().floor_db < -43.0, "floor {}", m.stats().floor_db);
        // the room goes quiet again: the floor snaps back down at once
        let hops = run(&mut m, &white(-60.0, 500, &mut rng), 160);
        assert!(m.stats().floor_db < -58.0, "floor {}", m.stats().floor_db);
        assert!(hops.iter().all(|h| !h.open));
        // and speech over the quiet room opens the gate again
        let hops = run(&mut m, &sine(200.0, 0.02, 200), 160);
        assert!(hops.iter().any(|h| h.open));
    }

    #[test]
    fn hysteresis_prevents_chatter_near_the_threshold() {
        // a tone that wobbles +-2 dB (period 90 ms, shorter than the hold) around the open threshold
        let mut rng = Rng(1);
        let mut pcm = white(-65.0, 600, &mut rng);
        let centre = -48.0 + 0.5;
        let n = 16 * 1500;
        for k in 0..n {
            let wob = 2.0 * (2.0 * std::f32::consts::PI * k as f32 / (16.0 * 90.0)).sin();
            let a = db_to_rms(centre + wob) * 2f32.sqrt();
            pcm.push(to_i16(a * (2.0 * std::f32::consts::PI * 300.0 * k as f32 / 16_000.0).sin()));
        }
        let hops = run(&mut Meter::new(), &pcm, 160);
        let flips = hops.windows(2).filter(|w| w[0].open != w[1].open).count();
        assert!(flips <= 2, "{flips} gate transitions around the threshold (want one open at most, one close at the end)");
        assert!(hops.iter().any(|h| h.open));
    }

    #[test]
    fn nan_free_and_bounded_for_extreme_input() {
        let mut m = Meter::new();
        let loud: Vec<i16> = (0..16_000).map(|n| if n % 2 == 0 { i16::MAX } else { i16::MIN }).collect();
        let hops = run(&mut m, &loud, 160);
        assert!(hops.iter().all(|h| h.level.is_finite() && (0.0..=1.0).contains(&h.level)));
        assert!(hops.last().unwrap().open && hops.last().unwrap().level > 0.99);
        let s = m.stats();
        assert!(s.floor_db <= FLOOR_MAX_DB && s.floor_db >= FLOOR_MIN_DB && s.open_hops * 10 > s.hops * 9);
    }
}
