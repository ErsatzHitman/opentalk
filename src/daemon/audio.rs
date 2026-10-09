//! Microphone capture via raw WASAPI (shared mode, event-driven, AUTOCONVERTPCM to 16 kHz mono
//! i16 so the OS does the resampling; own downmix + windowed-sinc resampler as fallback).
//!
//! - `Recorder::start` returns as soon as the stream runs; device activation cost is logged.
//! - The capture thread publishes RMS of the latest ~43 ms window into `level` (f32 bits).
//! - Optional `sink` receives each PCM packet for streaming STT.
//! - `vad` offers energy-based speech detection + silence trimming (150 ms padding).
//! - `OPENTALK_INJECT_WAV=<file>` replaces the microphone with a WAV played back in real time
//!   (e2e tests and benchmarks), see [`inject_wav`].
mod resample;
pub mod vad;
mod wasapi;

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
/// Level window: ~43 ms at 16 kHz.
pub const LEVEL_WINDOW: usize = 688;
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
}

/// State shared between the capture thread and the `Recorder` handle.
#[derive(Default)]
pub(crate) struct Shared {
    samples: Mutex<Vec<i16>>,
    count: AtomicU64,
    lost: AtomicBool,
}

/// Capture-thread side of [`Shared`]: appends PCM, publishes the level, feeds the sink.
pub(crate) struct Output {
    shared: Arc<Shared>,
    level: Arc<AtomicU32>,
    sink: Option<PcmSink>,
}

impl Output {
    pub(crate) fn new(level: Arc<AtomicU32>, sink: Option<PcmSink>, shared: Arc<Shared>) -> Output {
        shared.samples.lock().unwrap_or_else(|e| e.into_inner()).reserve(SAMPLE_RATE as usize * 10);
        Output { shared, level, sink }
    }

    pub(crate) fn push(&mut self, pcm: &[i16]) {
        if pcm.is_empty() {
            return;
        }
        let rms = {
            let mut v = self.shared.samples.lock().unwrap_or_else(|e| e.into_inner());
            v.extend_from_slice(pcm);
            self.shared.count.store(v.len() as u64, Ordering::Relaxed);
            rms_tail(&v)
        };
        self.level.store(rms.to_bits(), Ordering::Relaxed);
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

/// RMS (full scale = 1.0) of the last [`LEVEL_WINDOW`] samples.
fn rms_tail(samples: &[i16]) -> f32 {
    let tail = &samples[samples.len().saturating_sub(LEVEL_WINDOW)..];
    if tail.is_empty() {
        return 0.0;
    }
    let sum: f64 = tail.iter().map(|&s| (s as f64) * (s as f64)).sum();
    ((sum / tail.len() as f64).sqrt() / 32768.0) as f32
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

    /// Initialise `device_id` (None = default) in the background and keep it ready, so the next
    /// `start` only pays for `IAudioClient::Start` (~10 ms) instead of the driver's stream
    /// initialisation (250-700 ms on some mic arrays). Does not light the Windows mic indicator.
    /// Call after the user changes the microphone setting; `warmup` already does it for the default.
    pub fn prepare(device_id: Option<&str>) {
        wasapi::prepare(device_id);
    }

    /// Release the prepared stream (the next `start` is slow again). Normally never needed.
    pub fn release_prepared() {
        wasapi::release();
    }

    /// Stop and return everything captured (16 kHz mono).
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
    pub fn elapsed_ms(&self) -> u64 {
        self.shared.count.load(Ordering::Relaxed) * 1000 / SAMPLE_RATE as u64
    }

    /// Time `start` spent bringing the stream up, ms.
    pub fn activation_ms(&self) -> f64 {
        self.activation_ms
    }

    /// True when the capture device went away (unplugged / disabled) mid-recording. `stop` still
    /// returns what was captured before that.
    pub fn device_lost(&self) -> bool {
        self.shared.lost.load(Ordering::Relaxed)
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
        stats.samples = samples.len() as u64;
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

/// Initialise COM + cache the device enumerator so the first `start` is fast. Does not open the
/// microphone.
pub fn warmup() {
    wasapi::warmup();
}

/// Read the published level.
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

    #[test]
    fn rms_tail_uses_last_window_only() {
        let mut v = vec![i16::MAX; 5000];
        v.extend(vec![0i16; LEVEL_WINDOW]);
        assert_eq!(rms_tail(&v), 0.0);
        let v = vec![16384i16; 100];
        assert!((rms_tail(&v) - 0.5).abs() < 1e-4);
        assert_eq!(rms_tail(&[]), 0.0);
    }

    #[test]
    fn inject_wav_plays_in_real_time_and_feeds_level_and_sink() {
        let dir = temp_dir("inject");
        let path = dir.join("tone.wav");
        std::fs::write(&path, tone_wav(16_000, 500, 0.5).to_wav()).unwrap();

        let level = Arc::new(AtomicU32::new(0));
        let got = Arc::new(Mutex::new(0usize));
        let got2 = got.clone();
        let sink: PcmSink = Box::new(move |pcm| *got2.lock().unwrap() += pcm.len());
        let t = Instant::now();
        let rec = inject_wav(&path, level.clone(), Some(sink)).expect("inject");
        std::thread::sleep(Duration::from_millis(300));
        let mid_level = read_level(&level);
        let mid_ms = rec.elapsed_ms();
        std::thread::sleep(Duration::from_millis(400));
        let (audio, stats) = rec.stop_with_stats();
        let wall = t.elapsed().as_millis() as u64;

        assert!(mid_level > 0.2 && mid_level < 0.5, "level {mid_level}");
        assert!((200..=450).contains(&mid_ms), "elapsed {mid_ms}");
        assert_eq!(audio.sample_rate, 16_000);
        // 700 ms of wall time -> ~700 ms of audio (500 ms tone then silence).
        let ms = audio.duration_ms();
        assert!(ms >= 600 && ms <= wall + 20, "captured {ms} ms in {wall} ms");
        // First 500 ms are the tone, bit-exact.
        let src = tone_wav(16_000, 500, 0.5).samples;
        assert_eq!(&audio.samples[..src.len()], &src[..]);
        assert!(audio.samples[src.len()..].iter().all(|&s| s == 0));
        assert_eq!(*got.lock().unwrap(), audio.samples.len());
        assert_eq!(read_level(&level), 0.0, "level resets on stop");
        assert_eq!(stats.backend, "inject");
        assert!(stats.packets > 20 && stats.max_gap_ms < 100.0, "{stats:?}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn start_honours_inject_env() {
        let dir = temp_dir("injectenv");
        let path = dir.join("env.wav");
        std::fs::write(&path, tone_wav(16_000, 200, 0.5).to_wav()).unwrap();
        // SAFETY: only this test touches OPENTALK_INJECT_WAV, and std serialises env access on Windows.
        unsafe { std::env::set_var(INJECT_ENV, &path) };
        let rec = Recorder::start(None, Arc::new(AtomicU32::new(0)), None);
        unsafe { std::env::remove_var(INJECT_ENV) };
        let rec = rec.expect("start with inject env");
        std::thread::sleep(Duration::from_millis(150));
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
        let dir = temp_dir("injectcancel");
        let path = dir.join("t.wav");
        std::fs::write(&path, tone_wav(16_000, 200, 0.3).to_wav()).unwrap();
        let level = Arc::new(AtomicU32::new(0));
        let rec = inject_wav(&path, level.clone(), None).unwrap();
        std::thread::sleep(Duration::from_millis(50));
        let t = Instant::now();
        rec.cancel();
        assert!(t.elapsed() < Duration::from_millis(500));
        assert_eq!(read_level(&level), 0.0);
        let rec = inject_wav(&path, level, None).unwrap();
        drop(rec);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn panicking_sink_does_not_kill_recording() {
        let dir = temp_dir("injectpanic");
        let path = dir.join("t.wav");
        std::fs::write(&path, tone_wav(16_000, 300, 0.3).to_wav()).unwrap();
        let sink: PcmSink = Box::new(|_| panic!("sink bug"));
        let rec = inject_wav(&path, Arc::new(AtomicU32::new(0)), Some(sink)).unwrap();
        std::thread::sleep(Duration::from_millis(250));
        let audio = rec.stop();
        assert!(audio.duration_ms() >= 200);
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
        warmup();
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
