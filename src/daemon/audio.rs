//! Microphone capture via raw WASAPI (shared mode, event-driven, AUTOCONVERTPCM to 16 kHz mono
//! i16 so the OS does the resampling; own downmix + windowed-sinc resampler as fallback).
//!
//! - `Recorder::start` returns as soon as the stream runs; device activation cost is logged.
//! - The capture thread runs a streaming [`meter::Meter`] over each packet and publishes the
//!   display level (0..1, exactly 0.0 while the room is silent) into `level` (f32 bits), plus the
//!   highest level since the pill last looked ([`take_peak`]), so no short sound falls between
//!   two animation frames.
//! - Optional `sink` receives each PCM packet for streaming STT.
//! - `vad` offers energy-based speech detection + silence trimming (150 ms padding).
//! - `OPENTALK_INJECT_WAV=<file>` replaces the microphone with a WAV played back in real time
//!   (e2e tests and benchmarks), see [`inject_wav`].
pub mod meter;
mod resample;
pub mod vad;
pub(crate) mod wasapi;

use crate::stt::Audio;
use crate::{log_info, log_warn};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Capture format delivered to callers.
pub const SAMPLE_RATE: u32 = 16_000;
/// Environment variable naming a WAV to play instead of opening the microphone.
pub const INJECT_ENV: &str = "OPENTALK_INJECT_WAV";
/// How long `stop` waits for the capture thread before returning what it has.
const STOP_TIMEOUT: Duration = Duration::from_secs(2);
/// How long `start` waits for the stream to come up (cold initialisation is 0.3-0.7 s).
const READY_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Device {
    pub id: String,
    pub name: String,
    pub is_default: bool,
}

pub type PcmSink = Box<dyn FnMut(&[i16]) + Send>;

/// Measurements from one capture session (logged, and used by benchmarks / desktop tests).
#[derive(Clone, Debug, Default)]
pub struct CaptureStats {
    /// `"wasapi"` or `"inject"`.
    #[allow(dead_code)] // diagnostic label: set by both backends, read by tests and Debug output
    pub backend: &'static str,
    /// Human readable stream format.
    pub format: String,
    /// Time from `start` call to a running stream, ms.
    pub activation_ms: f64,
    pub packets: u64,
    /// Mean / max time between consecutive packets, ms.
    pub mean_gap_ms: f64,
    pub max_gap_ms: f64,
    /// CPU time (kernel + user) consumed by the capture loop, ms (tick-granular; prefer cycles).
    pub thread_cpu_ms: f64,
    /// Exact CPU cycles consumed by the capture loop (see `cycles_per_ms`).
    pub thread_cycles: u64,
    pub discontinuities: u64,
    /// Total 16 kHz samples captured.
    pub samples: u64,
    /// The device disappeared (or the stream broke) before `stop`.
    pub device_lost: bool,
    /// Share of 10 ms level hops with the noise gate open, percent.
    pub gate_open_pct: f32,
    /// Adaptive noise floor at the end of the recording, dBFS.
    pub floor_dbfs: f32,
}

/// State shared between the capture thread and the `Recorder` handle.
#[derive(Default)]
pub(crate) struct Shared {
    samples: Mutex<Vec<i16>>,
    count: AtomicU64,
    lost: AtomicBool,
    /// Level meter totals (hops, hops with the gate open, floor dBFS as f32 bits), for the log line.
    meter_hops: AtomicU64,
    meter_open: AtomicU64,
    meter_floor: AtomicU32,
}

/// Capture-thread side of [`Shared`]: appends PCM, publishes the level, feeds the sink.
pub(crate) struct Output {
    shared: Arc<Shared>,
    level: Arc<AtomicU32>,
    meter: meter::Meter,
    sink: Option<PcmSink>,
}

impl Output {
    pub(crate) fn new(level: Arc<AtomicU32>, sink: Option<PcmSink>, shared: Arc<Shared>) -> Output {
        shared.samples.lock().unwrap_or_else(|e| e.into_inner()).reserve(SAMPLE_RATE as usize * 10);
        reset_peak();
        Output { shared, level, meter: meter::Meter::new(), sink }
    }

    pub(crate) fn push(&mut self, pcm: &[i16]) {
        if pcm.is_empty() {
            return;
        }
        {
            let mut v = self.shared.samples.lock().unwrap_or_else(|e| e.into_inner());
            v.extend_from_slice(pcm);
            self.shared.count.store(v.len() as u64, Ordering::Relaxed);
        }
        // The meter runs outside the sample lock; every 10 ms hop it completes is published (the
        // level, the peak since the pill last looked, and the wake for a slowed timer).
        let level = &self.level;
        self.meter.push(pcm, |hop| publish_level(level, hop.level));
        let st = self.meter.stats();
        self.shared.meter_hops.store(st.hops, Ordering::Relaxed);
        self.shared.meter_open.store(st.open_hops, Ordering::Relaxed);
        self.shared.meter_floor.store(st.floor_db.to_bits(), Ordering::Relaxed);
        if let Some(sink) = self.sink.as_mut() {
            // A misbehaving sink must not take the capture thread (and the recording) down.
            if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| sink(pcm))).is_err() {
                log_warn!("audio: PCM sink panicked; detaching it");
                self.sink = None;
            }
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.shared.count.load(Ordering::Relaxed) as usize
    }

    pub(crate) fn mark_lost(&self) {
        self.shared.lost.store(true, Ordering::Relaxed);
    }

    /// Copy of everything captured so far (tests).
    #[cfg(test)]
    pub(crate) fn snapshot(&self) -> Vec<i16> {
        self.shared.samples.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

// --- level wake -----------------------------------------------------------------------------

/// A one-shot request from the pill: "tell me when the level leaves `lo..=hi`". The pill slows its
/// animation timer while a recording is silent and asks the capture thread to wake it again, so
/// nothing has to poll the level at 60 Hz to notice speech starting.
struct LevelWake {
    token: u64,
    notify: Box<dyn Fn() + Send + Sync>,
}

static WAKE_ON: AtomicBool = AtomicBool::new(false);
static WAKE_LO: AtomicU32 = AtomicU32::new(0);
static WAKE_HI: AtomicU32 = AtomicU32::new(0);
static WAKE_SLOT: Mutex<Option<LevelWake>> = Mutex::new(None);
static WAKE_TOKENS: AtomicU64 = AtomicU64::new(1);

/// Arm the level wake: the first published level outside `lo..=hi` calls `notify` once (on the
/// capture thread, so it must be cheap and non-blocking) and disarms. Replaces an earlier request.
/// The returned token identifies this request for [`disarm_level_wake`].
pub fn arm_level_wake(lo: f32, hi: f32, notify: Box<dyn Fn() + Send + Sync>) -> u64 {
    let token = WAKE_TOKENS.fetch_add(1, Ordering::Relaxed);
    let mut slot = WAKE_SLOT.lock().unwrap_or_else(|e| e.into_inner());
    WAKE_LO.store(lo.to_bits(), Ordering::Relaxed);
    WAKE_HI.store(hi.to_bits(), Ordering::Relaxed);
    *slot = Some(LevelWake { token, notify });
    WAKE_ON.store(true, Ordering::Release);
    token
}

/// Drop the level wake `token` armed, if it is still the pending one (a newer request, or one that
/// already fired, is left alone).
pub fn disarm_level_wake(token: u64) {
    let mut slot = WAKE_SLOT.lock().unwrap_or_else(|e| e.into_inner());
    if slot.as_ref().is_some_and(|w| w.token == token) {
        WAKE_ON.store(false, Ordering::Release);
        *slot = None;
    }
}

/// Highest display level published since [`take_peak`] last ran (f32 bits; non-negative floats order
/// like their bits, so `fetch_max` works). There is one microphone, hence one process-wide slot.
static PEAK: AtomicU32 = AtomicU32::new(0);

/// The highest level published since the previous call, and reset. The pill calls this once per
/// frame, so a sound shorter than a frame still moves the bars.
pub fn take_peak() -> f32 {
    f32::from_bits(PEAK.swap(0, Ordering::Relaxed))
}

/// Forget the peak (a recording starts or ends).
pub(crate) fn reset_peak() {
    PEAK.store(0, Ordering::Relaxed);
}

/// Store the live level (and raise the peak) and fire the armed wake if it left its band. One relaxed
/// load when nothing is armed. A level that is not a finite number in 0..=1 is clamped (NaN -> 0).
pub(crate) fn publish_level(level: &AtomicU32, value: f32) {
    let v = if value.is_finite() { value.clamp(0.0, 1.0) } else { 0.0 };
    level.store(v.to_bits(), Ordering::Relaxed);
    if v > 0.0 {
        PEAK.fetch_max(v.to_bits(), Ordering::Relaxed);
    }
    if !WAKE_ON.load(Ordering::Acquire) {
        return;
    }
    let (lo, hi) = (f32::from_bits(WAKE_LO.load(Ordering::Relaxed)), f32::from_bits(WAKE_HI.load(Ordering::Relaxed)));
    if v >= lo && v <= hi {
        return;
    }
    // one-shot: whoever clears the flag owns the notification
    if WAKE_ON.swap(false, Ordering::AcqRel) {
        let taken = WAKE_SLOT.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(w) = taken {
            (w.notify)();
        }
    }
}

/// Serialises tests that arm the (process-global) level wake or run real-time capture, so one
/// test's audio cannot fire another's wake or starve its pacing.
#[cfg(test)]
pub(crate) fn serial_guard() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// Manual-reset "please stop" event shared with the capture thread.
pub(crate) struct Stop(wasapi::Ev);

impl Stop {
    fn new() -> Result<Stop, String> {
        wasapi::Ev::new(true).map(Stop)
    }
    pub(crate) fn signal(&self) {
        self.0.set();
    }
    /// Wait up to `ms` for the stop signal; true when stopping.
    pub(crate) fn wait(&self, ms: u32) -> bool {
        self.0.wait(ms)
    }
    pub(crate) fn event_handle(&self) -> windows::Win32::Foundation::HANDLE {
        (self.0).0
    }
}

pub struct Recorder {
    stop: Arc<Stop>,
    shared: Arc<Shared>,
    level: Arc<AtomicU32>,
    /// Only injected recordings own a thread; WASAPI sessions run on the shared audio thread.
    join: Option<JoinHandle<()>>,
    done: Receiver<CaptureStats>,
    activation_ms: f64,
    active: bool,
}

impl Recorder {
    /// Start capturing from `device_id` (None = default console capture device). A device that
    /// cannot be opened falls back to the default one.
    ///
    /// With `OPENTALK_INJECT_WAV` set, plays that WAV in real time instead of opening a device.
    pub fn start(device_id: Option<&str>, level: Arc<AtomicU32>, sink: Option<PcmSink>) -> Result<Recorder, String> {
        if let Some(path) = inject_path() {
            return inject_wav(&path, level, sink);
        }
        let device = device_id.map(str::to_owned);
        begin(level, sink, move |stop, out, ready, done, t0| {
            wasapi::start(wasapi::StartReq { device, stop, out, ready, done, t0 })?;
            Ok(None)
        })
    }

    /// Initialise `device_id` (None = default) in the background and park it for `hold_secs` after
    /// the last use, so the next `start` only pays for `IAudioClient::Start` (~10 ms) instead of the
    /// driver's stream initialisation (250-700 ms on some mic arrays). Does not light the Windows
    /// mic indicator. Call after the user changes the microphone setting; `warmup` already does it
    /// for the default.
    pub fn prepare(device_id: Option<&str>, hold_secs: u32) {
        wasapi::prepare(device_id, hold_secs);
    }

    /// A recording ended: park the stream for `hold_secs` more, or release it at once (`0`).
    pub fn hold_prepared(hold_secs: u32) {
        wasapi::hold(hold_secs);
    }

    /// Release the prepared stream (the next `start` is slow again). Normally never needed.
    pub fn release_prepared() {
        wasapi::release();
    }

    /// Stop and return everything captured (16 kHz mono).
    #[cfg(test)]
    pub fn stop(self) -> Audio {
        self.stop_with_stats().0
    }

    /// Like [`Recorder::stop`], also returning capture measurements.
    pub fn stop_with_stats(mut self) -> (Audio, CaptureStats) {
        self.finish()
    }

    pub fn cancel(mut self) {
        let _ = self.finish();
    }

    /// Milliseconds captured so far.
    #[cfg(test)]
    pub fn elapsed_ms(&self) -> u64 {
        self.shared.count.load(Ordering::Relaxed) * 1000 / SAMPLE_RATE as u64
    }

    /// Time `start` spent bringing the stream up, ms.
    pub fn activation_ms(&self) -> f64 {
        self.activation_ms
    }

    fn finish(&mut self) -> (Audio, CaptureStats) {
        self.active = false;
        self.stop.signal();
        let mut stats = match self.done.recv_timeout(STOP_TIMEOUT) {
            Ok(stats) => stats,
            Err(_) => {
                // Thread hung in a driver call or died: hand back what we already have.
                log_warn!("audio: capture did not finish within {STOP_TIMEOUT:?}; returning captured audio");
                CaptureStats { device_lost: true, ..CaptureStats::default() }
            }
        };
        if let Some(t) = self.join.take() {
            let _ = t.join();
        }
        let samples = std::mem::take(&mut *self.shared.samples.lock().unwrap_or_else(|e| e.into_inner()));
        self.level.store(0f32.to_bits(), Ordering::Relaxed);
        reset_peak();
        stats.samples = samples.len() as u64;
        let hops = self.shared.meter_hops.load(Ordering::Relaxed);
        if hops > 0 {
            stats.gate_open_pct = self.shared.meter_open.load(Ordering::Relaxed) as f32 * 100.0 / hops as f32;
            stats.floor_dbfs = f32::from_bits(self.shared.meter_floor.load(Ordering::Relaxed));
        }
        stats.device_lost |= self.shared.lost.load(Ordering::Relaxed);
        (Audio { samples, sample_rate: SAMPLE_RATE }, stats)
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        if self.active {
            let _ = self.finish();
        }
    }
}

/// Set up shared state, hand it to `launch`, and wait until the stream reports it is live.
fn begin<F>(level: Arc<AtomicU32>, sink: Option<PcmSink>, launch: F) -> Result<Recorder, String>
where
    F: FnOnce(Arc<Stop>, Output, mpsc::Sender<Result<f64, String>>, mpsc::Sender<CaptureStats>, Instant) -> Result<Option<JoinHandle<()>>, String>,
{
    let t0 = Instant::now();
    let stop = Arc::new(Stop::new()?);
    let shared = Arc::new(Shared::default());
    let out = Output::new(level.clone(), sink, shared.clone());
    let (ready_tx, ready_rx) = mpsc::channel();
    let (done_tx, done_rx) = mpsc::channel();
    let join = launch(stop.clone(), out, ready_tx, done_tx, t0)?;
    match ready_rx.recv_timeout(READY_TIMEOUT) {
        Ok(Ok(_)) => Ok(Recorder { stop, shared, level, join, done: done_rx, activation_ms: t0.elapsed().as_secs_f64() * 1000.0, active: true }),
        Ok(Err(e)) => {
            if let Some(j) = join {
                let _ = j.join();
            }
            Err(e)
        }
        Err(_) => {
            // The engine skips requests whose stop event is already set.
            stop.signal();
            Err("microphone did not become ready within 10 s".into())
        }
    }
}

/// Active capture endpoints with friendly names.
pub fn list_devices() -> Vec<Device> {
    wasapi::list_devices()
}

/// Initialise COM + cache the device enumerator and park the default capture stream for
/// `hold_secs` after the last use (`0` = keep it), so the first `start` is fast. Does not open the
/// microphone for capture.
pub fn warmup(hold_secs: u32) {
    wasapi::warmup(hold_secs);
}

/// Read the published level.
#[cfg(test)]
pub fn read_level(level: &AtomicU32) -> f32 {
    f32::from_bits(level.load(std::sync::atomic::Ordering::Relaxed))
}

// ---------------------------------------------------------------------------------------------
// WAV injection
// ---------------------------------------------------------------------------------------------

/// Path from `OPENTALK_INJECT_WAV`, if set and non-empty.
pub fn inject_path() -> Option<PathBuf> {
    inject_path_from(std::env::var_os(INJECT_ENV))
}

fn inject_path_from(v: Option<std::ffi::OsString>) -> Option<PathBuf> {
    v.filter(|s| !s.is_empty()).map(PathBuf::from)
}

/// Load a WAV as 16 kHz mono i16 (any rate is resampled).
fn load_wav_16k(path: &Path) -> Result<Vec<i16>, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let audio = Audio::from_wav(&bytes).ok_or_else(|| format!("{}: not a PCM16 WAV", path.display()))?;
    if audio.sample_rate == SAMPLE_RATE {
        return Ok(audio.samples);
    }
    let mut rs = resample::Resampler::new(audio.sample_rate, SAMPLE_RATE).ok_or_else(|| format!("{}: bad sample rate {}", path.display(), audio.sample_rate))?;
    let input: Vec<f32> = audio.samples.iter().map(|&s| s as f32 / 32768.0).collect();
    let mut out = Vec::with_capacity(input.len() * SAMPLE_RATE as usize / audio.sample_rate as usize + 64);
    rs.process(&input, &mut out);
    rs.finish(&mut out);
    Ok(out.into_iter().map(wasapi::f32_to_i16).collect())
}

/// Start a recorder that plays `path` (a PCM16 WAV) in real time instead of using the microphone.
///
/// Samples are delivered paced by the clock exactly as a live device would, feeding `level` and
/// `sink`. When the file ends the "microphone" keeps delivering digital silence until `stop`, so
/// hold-to-talk sessions behave like a quiet room after the speaker finishes.
pub fn inject_wav(path: &Path, level: Arc<AtomicU32>, sink: Option<PcmSink>) -> Result<Recorder, String> {
    let data = load_wav_16k(path)?;
    log_info!("audio: injecting {} ({} ms) instead of the microphone", path.display(), data.len() as u64 * 1000 / SAMPLE_RATE as u64);
    begin(level, sink, move |stop, out, ready, done, _t0| {
        std::thread::Builder::new()
            .name("opentalk-inject".into())
            .spawn(move || {
                let stats = inject_run(data, stop, out, ready);
                let _ = done.send(stats);
            })
            .map(Some)
            .map_err(|e| format!("cannot spawn inject thread: {e}"))
    })
}

/// Pacing granularity of the injected stream.
const INJECT_STEP_MS: u64 = 10;

fn inject_run(data: Vec<i16>, stop: Arc<Stop>, mut out: Output, ready: mpsc::Sender<Result<f64, String>>) -> CaptureStats {
    let mut stats = CaptureStats { backend: "inject", format: "16000 Hz mono s16 (injected WAV)".into(), ..CaptureStats::default() };
    let _ = ready.send(Ok(0.0));
    let t0 = Instant::now();
    let zeros = vec![0i16; SAMPLE_RATE as usize];
    let (mut sent, mut last, mut gap_sum, mut gap_n) = (0usize, t0, 0.0f64, 0u64);
    let mut step = 0u64;
    loop {
        step += 1;
        let deadline = t0 + Duration::from_millis(step * INJECT_STEP_MS);
        let wait = deadline.saturating_duration_since(Instant::now());
        let stopping = stop.wait(wait.as_micros().div_ceil(1000).min(1000) as u32);
        // Samples a real device would have delivered by now.
        let due = (t0.elapsed().as_micros() * SAMPLE_RATE as u128 / 1_000_000) as usize;
        if due > sent {
            let now = Instant::now();
            let gap = now.duration_since(last).as_secs_f64() * 1000.0;
            last = now;
            gap_sum += gap;
            gap_n += 1;
            stats.max_gap_ms = stats.max_gap_ms.max(gap);
            stats.packets += 1;
            if sent < data.len() {
                let end = due.min(data.len());
                out.push(&data[sent..end]);
                sent = end;
            }
            while sent < due {
                let n = (due - sent).min(zeros.len());
                out.push(&zeros[..n]);
                sent += n;
            }
        }
        if stopping {
            break;
        }
    }
    if gap_n > 0 {
        stats.mean_gap_ms = gap_sum / gap_n as f64;
    }
    stats
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f64::consts::PI;

    fn temp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("opentalk-audio-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn tone_wav(rate: u32, ms: u64, amp: f64) -> Audio {
        let n = (rate as u64 * ms / 1000) as usize;
        Audio { samples: (0..n).map(|i| (amp * 32767.0 * (2.0 * PI * 440.0 * i as f64 / rate as f64).sin()) as i16).collect(), sample_rate: rate }
    }

    #[test]
    fn inject_env_parsing() {
        assert_eq!(inject_path_from(None), None);
        assert_eq!(inject_path_from(Some("".into())), None);
        assert_eq!(inject_path_from(Some("a.wav".into())), Some(PathBuf::from("a.wav")));
    }

    /// The peak slot keeps the highest level between two looks and is empty after each look; garbage
    /// levels are clamped instead of poisoning it.
    #[test]
    fn peak_keeps_the_highest_level_between_looks() {
        let _serial = serial_guard();
        let level = AtomicU32::new(0);
        reset_peak();
        assert_eq!(take_peak(), 0.0);
        for v in [0.1f32, 0.6, 0.3, 0.0, 0.2] {
            publish_level(&level, v);
        }
        assert_eq!(read_level(&level), 0.2, "the live level is the newest one");
        assert_eq!(take_peak(), 0.6, "the peak is the highest since the last look");
        assert_eq!(take_peak(), 0.0, "and starts over after it");
        publish_level(&level, f32::NAN);
        publish_level(&level, -0.5);
        publish_level(&level, f32::INFINITY);
        assert_eq!(take_peak(), 0.0, "NaN, negative and infinite inputs publish 0");
        assert_eq!(read_level(&level), 0.0);
        publish_level(&level, 7.0);
        assert_eq!(read_level(&level), 1.0, "levels above 1 are clamped");
        assert_eq!(take_peak(), 1.0);
    }

    /// Packets of any size feed the meter and the whole path (meter -> level -> peak) stays silent
    /// for room noise and exactly flat for digital silence.
    #[test]
    fn output_publishes_meter_levels_and_stays_flat_in_a_quiet_room() {
        let _serial = serial_guard();
        let level = Arc::new(AtomicU32::new(0));
        let shared = Arc::new(Shared::default());
        let mut out = Output::new(level.clone(), None, shared.clone());
        // 3 s of hiss at about -55 dBFS (+-1.7 LSB-ish scaled): no packet may move the level
        let mut x = 0x2545_F491u32;
        let mut peak_seen = 0.0f32;
        for _ in 0..300 {
            let pkt: Vec<i16> = (0..160)
                .map(|_| {
                    x ^= x << 13;
                    x ^= x >> 17;
                    x ^= x << 5;
                    ((x % 121) as i32 - 60) as i16
                })
                .collect();
            out.push(&pkt);
            assert_eq!(read_level(&level), 0.0);
            peak_seen = peak_seen.max(take_peak());
        }
        assert_eq!(peak_seen, 0.0, "room noise never reaches the pill");
        // a loud burst opens the gate within one packet
        let tone: Vec<i16> = (0..160).map(|n| (8000.0 * (n as f32 * 0.2).sin()) as i16).collect();
        out.push(&tone);
        out.push(&tone);
        assert!(read_level(&level) > 0.3, "speech level {}", read_level(&level));
        assert!(take_peak() > 0.3);
        assert!(shared.meter_hops.load(Ordering::Relaxed) >= 300 && shared.meter_open.load(Ordering::Relaxed) >= 1);
    }

    #[test]
    fn inject_wav_plays_in_real_time_and_feeds_level_and_sink() {
        let _serial = serial_guard();
        let dir = temp_dir("inject");
        let path = dir.join("tone.wav");
        // longer than the whole run, so a late scheduler never lets the tone end under the probes
        std::fs::write(&path, tone_wav(16_000, 3000, 0.5).to_wav()).unwrap();

        let level = Arc::new(AtomicU32::new(0));
        let got = Arc::new(Mutex::new(0usize));
        let got2 = got.clone();
        let sink: PcmSink = Box::new(move |pcm| *got2.lock().unwrap() += pcm.len());
        let t = Instant::now();
        let rec = inject_wav(&path, level.clone(), Some(sink)).expect("inject");
        std::thread::sleep(Duration::from_millis(300));
        let mid_level = read_level(&level);
        let mid_ms = rec.elapsed_ms();
        let mid_wall = t.elapsed().as_millis() as u64;
        std::thread::sleep(Duration::from_millis(400));
        let (audio, stats) = rec.stop_with_stats();
        let wall = t.elapsed().as_millis() as u64;

        assert!(mid_level > 0.5 && mid_level <= 1.0, "a loud steady tone shows a high display level, got {mid_level}");
        // Real-time pacing, in both directions: the stream is never ahead of the clock (a fast-forward
        // would deliver the whole 3 s tone at once), and it has made real progress. How far behind a
        // starved thread may lag is not part of the contract, so the lower bound is generous.
        assert!(mid_ms <= mid_wall + 20, "ran ahead of real time: {mid_ms} ms of audio after {mid_wall} ms");
        assert!(mid_ms >= 50, "stalled: {mid_ms} ms of audio after {mid_wall} ms");
        assert_eq!(audio.sample_rate, 16_000);
        let ms = audio.duration_ms();
        assert!((400..=wall + 20).contains(&ms), "captured {ms} ms in {wall} ms");
        // The tone is delivered bit-exact.
        let src = tone_wav(16_000, 3000, 0.5).samples;
        assert_eq!(audio.samples[..], src[..audio.samples.len()]);
        assert_eq!(*got.lock().unwrap(), audio.samples.len());
        assert_eq!(read_level(&level), 0.0, "level resets on stop");
        assert_eq!(stats.backend, "inject");
        assert!(stats.packets > 10 && stats.max_gap_ms < 500.0, "{stats:?}");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// After the file ends the injected "microphone" keeps delivering digital silence.
    #[test]
    fn inject_wav_pads_with_silence_after_the_file() {
        let _serial = serial_guard();
        let dir = temp_dir("injectpad");
        let path = dir.join("short.wav");
        std::fs::write(&path, tone_wav(16_000, 200, 0.5).to_wav()).unwrap();
        let rec = inject_wav(&path, Arc::new(AtomicU32::new(0)), None).expect("inject");
        std::thread::sleep(Duration::from_millis(600));
        let audio = rec.stop();
        let src = tone_wav(16_000, 200, 0.5).samples;
        assert!(audio.samples.len() > src.len(), "silence follows the file: {} samples", audio.samples.len());
        assert_eq!(&audio.samples[..src.len()], &src[..]);
        assert!(audio.samples[src.len()..].iter().all(|&s| s == 0));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn level_wake_fires_once_when_the_level_leaves_its_band() {
        use std::sync::atomic::AtomicUsize;
        let _serial = serial_guard();
        let level = AtomicU32::new(0);
        let fired = Arc::new(AtomicUsize::new(0));
        let f = fired.clone();
        let first = arm_level_wake(0.0, 0.01, Box::new(move || {
            f.fetch_add(1, Ordering::SeqCst);
        }));
        publish_level(&level, 0.0);
        publish_level(&level, 0.005);
        assert_eq!(fired.load(Ordering::SeqCst), 0, "inside the band: no wake");
        assert_eq!(read_level(&level), 0.005, "the level is stored either way");
        publish_level(&level, 0.2);
        assert_eq!(fired.load(Ordering::SeqCst), 1);
        publish_level(&level, 0.3);
        assert_eq!(fired.load(Ordering::SeqCst), 1, "one-shot");
        // disarming drops the request
        let f = fired.clone();
        let second = arm_level_wake(0.0, 0.01, Box::new(move || {
            f.fetch_add(1, Ordering::SeqCst);
        }));
        disarm_level_wake(first); // the fired request's token: the pending one is not touched
        assert!(WAKE_ON.load(Ordering::SeqCst));
        disarm_level_wake(second);
        publish_level(&level, 0.4);
        assert_eq!(fired.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn start_honours_inject_env() {
        let _serial = serial_guard();
        let dir = temp_dir("injectenv");
        let path = dir.join("env.wav");
        std::fs::write(&path, tone_wav(16_000, 200, 0.5).to_wav()).unwrap();
        // SAFETY: only this test touches OPENTALK_INJECT_WAV, and std serialises env access on Windows.
        unsafe { std::env::set_var(INJECT_ENV, &path) };
        let rec = Recorder::start(None, Arc::new(AtomicU32::new(0)), None);
        unsafe { std::env::remove_var(INJECT_ENV) };
        let rec = rec.expect("start with inject env");
        std::thread::sleep(Duration::from_millis(250));
        let (audio, stats) = rec.stop_with_stats();
        assert_eq!(stats.backend, "inject");
        assert!(audio.duration_ms() >= 100);
        assert!(audio.samples[..100].iter().any(|&s| s != 0));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn inject_wav_resamples_other_rates() {
        let dir = temp_dir("inject48");
        let path = dir.join("tone48.wav");
        std::fs::write(&path, tone_wav(48_000, 300, 0.5).to_wav()).unwrap();
        let samples = load_wav_16k(&path).unwrap();
        assert!((samples.len() as i64 - 4_800).abs() < 80, "{}", samples.len());
        let rms = (samples.iter().map(|&s| (s as f64 / 32768.0).powi(2)).sum::<f64>() / samples.len() as f64).sqrt();
        assert!((rms - 0.5 / 2f64.sqrt()).abs() < 0.03, "{rms}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn inject_wav_reports_bad_files() {
        let dir = temp_dir("injectbad");
        assert!(inject_wav(&dir.join("missing.wav"), Arc::new(AtomicU32::new(0)), None).is_err());
        let bad = dir.join("bad.wav");
        std::fs::write(&bad, b"not a wav").unwrap();
        assert!(inject_wav(&bad, Arc::new(AtomicU32::new(0)), None).is_err());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn cancel_and_drop_stop_the_thread() {
        let _serial = serial_guard();
        let dir = temp_dir("injectcancel");
        let path = dir.join("t.wav");
        std::fs::write(&path, tone_wav(16_000, 200, 0.3).to_wav()).unwrap();
        let level = Arc::new(AtomicU32::new(0));
        let rec = inject_wav(&path, level.clone(), None).unwrap();
        std::thread::sleep(Duration::from_millis(50));
        let t = Instant::now();
        rec.cancel();
        assert!(t.elapsed() < Duration::from_millis(1500), "cancel took {:?}", t.elapsed());
        assert_eq!(read_level(&level), 0.0);
        let rec = inject_wav(&path, level, None).unwrap();
        drop(rec);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn panicking_sink_does_not_kill_recording() {
        let _serial = serial_guard();
        let dir = temp_dir("injectpanic");
        let path = dir.join("t.wav");
        std::fs::write(&path, tone_wav(16_000, 300, 0.3).to_wav()).unwrap();
        let sink: PcmSink = Box::new(|_| panic!("sink bug"));
        let rec = inject_wav(&path, Arc::new(AtomicU32::new(0)), Some(sink)).unwrap();
        std::thread::sleep(Duration::from_millis(400));
        let audio = rec.stop();
        assert!(audio.duration_ms() >= 150, "recording survived the panicking sink: {} ms", audio.duration_ms());
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Real default microphone. Measures (1) a cold first start, (2) a start after `warmup`
    /// prepared the stream, then records 1 s and reports packet cadence and capture-loop CPU.
    #[test]
    #[ignore]
    fn desktop_record_1s() {
        let devices = list_devices();
        println!("capture devices: {devices:#?}");
        if devices.is_empty() {
            println!("no capture device present; nothing to record");
            return;
        }

        // (1) Cold: nothing initialised yet in this process.
        let level = Arc::new(AtomicU32::new(0));
        let t = Instant::now();
        let rec = Recorder::start(None, level.clone(), None).expect("cold start");
        let cold_ms = t.elapsed().as_secs_f64() * 1000.0;
        std::thread::sleep(Duration::from_millis(300));
        let t = Instant::now();
        let (a, cold_stats) = rec.stop_with_stats();
        println!("cold start: {cold_ms:.1} ms; stop: {:.1} ms; {} ms captured ({})", t.elapsed().as_secs_f64() * 1000.0, a.duration_ms(), cold_stats.format);

        // (2) Warm: drop the parked stream, then let warmup prepare it in the background.
        Recorder::release_prepared();
        let t = Instant::now();
        warmup(0);
        println!("warmup() returned after {:.1} ms", t.elapsed().as_secs_f64() * 1000.0);
        std::thread::sleep(Duration::from_millis(2000));

        let sink_samples = Arc::new(AtomicU64::new(0));
        let ss = sink_samples.clone();
        let sink: PcmSink = Box::new(move |pcm| {
            ss.fetch_add(pcm.len() as u64, Ordering::Relaxed);
        });
        let t = Instant::now();
        let rec = Recorder::start(None, level.clone(), Some(sink)).expect("warm start");
        let start_ms = t.elapsed().as_secs_f64() * 1000.0;
        let mut peak_level = 0f32;
        let begin = Instant::now();
        while begin.elapsed() < Duration::from_millis(1000) {
            peak_level = peak_level.max(read_level(&level));
            std::thread::sleep(Duration::from_millis(20));
        }
        let elapsed_before_stop = rec.elapsed_ms();
        let t = Instant::now();
        let (audio, stats) = rec.stop_with_stats();
        let stop_ms = t.elapsed().as_secs_f64() * 1000.0;

        println!("WARM start latency: {start_ms:.1} ms (cold was {cold_ms:.1} ms)");
        println!("stop latency: {stop_ms:.1} ms");
        println!("format: {}", stats.format);
        println!("captured: {} samples = {} ms (elapsed_ms before stop: {elapsed_before_stop})", audio.samples.len(), audio.duration_ms());
        println!("packets: {}, mean gap {:.2} ms, max gap {:.2} ms, discontinuities {}", stats.packets, stats.mean_gap_ms, stats.max_gap_ms, stats.discontinuities);
        let cpm = wasapi::cycles_per_ms();
        let cpu_ms = stats.thread_cycles as f64 / cpm;
        println!(
            "capture loop CPU: {:.2} Mcycles ~= {cpu_ms:.2} ms at {:.0} MHz over {} ms = {:.3}% of one core (GetThreadTimes says {:.2} ms)",
            stats.thread_cycles as f64 / 1e6,
            cpm / 1000.0,
            audio.duration_ms(),
            cpu_ms / audio.duration_ms() as f64 * 100.0,
            stats.thread_cpu_ms
        );
        println!("peak level {peak_level:.5}; sink received {} samples", sink_samples.load(Ordering::Relaxed));
        println!("vad: {:?}", vad::analyze(&audio));

        // (3) Back-to-back: the stream is parked after stop, so this start is fast too.
        let t = Instant::now();
        let rec = Recorder::start(None, level, None).expect("second start");
        println!("back-to-back start: {:.1} ms", t.elapsed().as_secs_f64() * 1000.0);
        rec.cancel();

        assert!(!stats.device_lost);
        assert!((14_000..=19_000).contains(&audio.samples.len()), "unexpected sample count {}", audio.samples.len());
        assert_eq!(sink_samples.load(Ordering::Relaxed), audio.samples.len() as u64);
        assert!(start_ms < 100.0, "prepared start took {start_ms:.1} ms");
    }
}
