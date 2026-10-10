//! FLAC upload probe (PLAN 9.5, BENCHMARK.md "FLAC upload experiment").
//!
//! Part 1: size and encode time of `flacenc` 0.5.1 on the fixture clips, plus seeded-noise variants, with a
//! lossless round trip through the pure-Rust `claxon` decoder.
//! Part 2: paced 127.0.0.1 upload of those bodies at 2/5/10/20/50 Mbps, paced by a high-resolution waitable
//! timer, compared with the `bytes * 8 / rate` formula.
//!
//! Usage: `cargo run --release [-- [--fixtures DIR] [--skip-upload] [--skip-encode] [--spin] [--default-buffers] [--verbose]]`

use flacenc::error::Verify;
use std::io::{Read, Write};
use std::os::windows::io::AsRawSocket;
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::Threading::{
    CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, CreateWaitableTimerExW, INFINITE, SetWaitableTimer, WaitForSingleObject,
};

const ENCODES: usize = 21;
const UPLOAD_RUNS: usize = 5;
const WRITE_SIZE: usize = 1460;
/// Loopback socket buffers (the default; `--default-buffers` keeps the OS defaults, which stall, see README).
const BIG_BUF: i32 = 4 << 20;
const TIMER_ALL_ACCESS: u32 = 0x001F_0003;

/// 16 kHz mono PCM16 clip.
#[derive(Clone)]
struct Clip {
    name: String,
    samples: Vec<i32>,
    wav_bytes: usize,
}

fn read_wav(path: &Path) -> Clip {
    let raw = std::fs::read(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    // Walk the RIFF chunks; the fixtures carry an 18-byte fmt chunk, so the header is not a fixed 44 bytes.
    let mut pos = 12;
    let mut data = None;
    while pos + 8 <= raw.len() {
        let id = &raw[pos..pos + 4];
        let len = u32::from_le_bytes(raw[pos + 4..pos + 8].try_into().unwrap()) as usize;
        if id == b"data" {
            data = Some(&raw[pos + 8..(pos + 8 + len).min(raw.len())]);
            break;
        }
        pos += 8 + len + (len & 1);
    }
    let data = data.expect("no data chunk");
    let samples = data.chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]]) as i32).collect();
    let name = path.file_name().unwrap().to_string_lossy().into_owned();
    // The app writes a canonical 44-byte header; the fixtures carry 2 extra bytes (18-byte fmt chunk).
    let wav_bytes = 44 + data.len();
    Clip { name, samples, wav_bytes }
}

/// xorshift64*: deterministic, seeded noise so the table is reproducible.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    /// Uniform integer in `[-amp, amp]`.
    fn uniform(&mut self, amp: i32) -> i32 {
        (self.next() >> 33) as i32 % (2 * amp + 1) - amp
    }
}

fn with_noise(clip: &Clip, amp: i32, seed: u64) -> Clip {
    let mut rng = Rng(seed);
    let samples = clip.samples.iter().map(|&s| (s + rng.uniform(amp)).clamp(-32768, 32767)).collect();
    Clip { name: format!("{} + noise {amp}", clip.name), samples, wav_bytes: clip.wav_bytes }
}

fn encode_flac(samples: &[i32]) -> Vec<u8> {
    let config = flacenc::config::Encoder::default().into_verified().expect("flac config");
    let source = flacenc::source::MemSource::from_samples(samples, 1, 16, 16_000);
    let stream = flacenc::encode_with_fixed_block_size(&config, source, config.block_size).expect("flac encode");
    let mut sink = flacenc::bitsink::ByteSink::new();
    flacenc::component::BitRepr::write(&stream, &mut sink).expect("flac write");
    sink.as_slice().to_vec()
}

fn decode_flac(bytes: &[u8]) -> Result<Vec<i32>, String> {
    let mut reader = claxon::FlacReader::new(bytes).map_err(|e| e.to_string())?;
    let info = reader.streaminfo();
    if info.channels != 1 || info.bits_per_sample != 16 || info.sample_rate != 16_000 {
        return Err(format!("unexpected stream info {info:?}"));
    }
    reader.samples().map(|s| s.map_err(|e| e.to_string())).collect()
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    let idx = ((sorted.len() as f64 - 1.0) * p).ceil() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

struct Encoded {
    label: String,
    wav_bytes: usize,
    flac: Vec<u8>,
}

fn run_encode(clips: &[Clip]) -> Vec<Encoded> {
    println!("## Size and encode time (flacenc 0.5.1, default config, 4096-sample blocks, {ENCODES} encodes)\n");
    println!("| Clip (16 kHz mono) | WAV bytes | FLAC bytes | FLAC / WAV | Encode median / p95 (ms) | Round trip |");
    println!("|---|---|---|---|---|---|");
    let mut out = Vec::new();
    for clip in clips {
        let secs = clip.samples.len() as f64 / 16_000.0;
        let mut flac = encode_flac(&clip.samples); // warm-up, also the body used for the size
        let mut times = Vec::with_capacity(ENCODES);
        for _ in 0..ENCODES {
            let t = Instant::now();
            flac = std::hint::black_box(encode_flac(std::hint::black_box(&clip.samples)));
            times.push(t.elapsed().as_secs_f64() * 1000.0);
        }
        times.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let round_trip = match decode_flac(&flac) {
            Ok(back) if back == clip.samples => "lossless".to_string(),
            Ok(back) => format!("MISMATCH ({} vs {} samples)", back.len(), clip.samples.len()),
            Err(e) => format!("decode error: {e}"),
        };
        println!(
            "| {}, {secs:.2} s | {} | {} | {:.3} | {:.1} / {:.1} | {round_trip} |",
            clip.name,
            clip.wav_bytes,
            flac.len(),
            flac.len() as f64 / clip.wav_bytes as f64,
            percentile(&times, 0.5),
            percentile(&times, 0.95),
        );
        out.push(Encoded { label: clip.name.clone(), wav_bytes: clip.wav_bytes, flac });
    }
    println!();
    out
}

/// High-resolution waitable timer used to pace the sender.
struct HiResTimer(HANDLE);

impl HiResTimer {
    fn new() -> Result<Self, String> {
        // SAFETY: plain FFI call with a null name and security attributes; the handle is closed in Drop.
        let h = unsafe { CreateWaitableTimerExW(None, None, CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, TIMER_ALL_ACCESS) }
            .map_err(|e| format!("CreateWaitableTimerExW: {e}"))?;
        Ok(Self(h))
    }

    /// Blocks for about `d`.
    fn wait(&self, d: Duration) {
        // Negative due time = relative, in 100 ns units.
        let due = -((d.as_nanos() / 100) as i64).max(1);
        // SAFETY: `self.0` is a valid timer handle; `due` outlives the call and no completion routine is set.
        unsafe {
            if SetWaitableTimer(self.0, &due, 0, None, None, false).is_ok() {
                WaitForSingleObject(self.0, INFINITE);
            }
        }
    }
}

impl Drop for HiResTimer {
    fn drop(&mut self) {
        // SAFETY: the handle came from CreateWaitableTimerExW and is closed exactly once.
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

/// Paced upload of `body` over loopback at `mbps`; returns milliseconds until the receiver replied.
///
/// Deadline pacing: write `i` is due at `start + i * interval`. A write that is late is sent at once, so
/// sub-granularity intervals (234 us at 50 Mbps) average out to the right rate in bursts.
fn set_buffers(sock: &impl AsRawSocket) {
    use windows::Win32::Networking::WinSock::{SO_RCVBUF, SO_SNDBUF, SOCKET, SOL_SOCKET, setsockopt};
    let v = BIG_BUF.to_ne_bytes();
    let s = SOCKET(sock.as_raw_socket() as usize);
    // SAFETY: `s` is a live socket owned by the caller and `v` outlives both calls.
    unsafe {
        setsockopt(s, SOL_SOCKET, SO_RCVBUF, Some(&v));
        setsockopt(s, SOL_SOCKET, SO_SNDBUF, Some(&v));
    }
}

fn paced_upload(body: &[u8], mbps: f64, timer: &HiResTimer, spin: bool, big: bool) -> Run {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    if big {
        set_buffers(&listener); // inherited by the accepted socket
    }
    let addr = listener.local_addr().unwrap();
    let total = body.len();
    let rx = std::thread::spawn(move || {
        let (mut s, _) = listener.accept().expect("accept");
        s.set_nodelay(true).ok();
        let mut buf = vec![0u8; 64 * 1024];
        let mut got = 0;
        while got < total {
            match s.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => got += n,
            }
        }
        let _ = s.write_all(&[1]);
    });
    let mut s = TcpStream::connect(addr).expect("connect");
    if big {
        set_buffers(&s);
    }
    s.set_nodelay(true).ok();
    let interval = Duration::from_secs_f64(WRITE_SIZE as f64 * 8.0 / (mbps * 1e6));
    let start = Instant::now();
    let mut max_late = Duration::ZERO;
    for (i, chunk) in body.chunks(WRITE_SIZE).enumerate() {
        let due = start + interval.mul_f64(i as f64);
        let now = Instant::now();
        if due > now {
            let rem = due - now;
            // Timer granularity is ~0.5 ms; shorter waits would only overshoot, so send in a burst instead.
            if rem >= Duration::from_micros(600) {
                timer.wait(rem);
            } else if spin {
                while Instant::now() < due {
                    std::hint::spin_loop();
                }
            }
        }
        max_late = max_late.max(Instant::now().saturating_duration_since(due));
        s.write_all(chunk).expect("write");
    }
    let last_write = start.elapsed();
    let mut ack = [0u8; 1];
    let _ = s.read_exact(&mut ack);
    let ms = start.elapsed().as_secs_f64() * 1000.0;
    let _ = rx.join();
    Run { total_ms: ms, last_write_ms: last_write.as_secs_f64() * 1000.0, max_late_ms: max_late.as_secs_f64() * 1000.0 }
}

/// One paced upload: wall time to the receiver's reply, time the last write was issued, worst sender lateness.
struct Run {
    total_ms: f64,
    last_write_ms: f64,
    max_late_ms: f64,
}

fn median(v: &mut [f64]) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[v.len() / 2]
}

fn run_upload(enc: &[Encoded], spin: bool, big: bool, verbose: bool) {
    let timer = match HiResTimer::new() {
        Ok(t) => t,
        Err(e) => {
            println!("high-resolution timer unavailable: {e}");
            return;
        }
    };
    println!(
        "## Paced loopback upload ({WRITE_SIZE}-byte writes, deadline-paced with a high-resolution waitable timer{}, median of {UPLOAD_RUNS}, ms)\n",
        if spin { " plus spin for sub-600 us gaps" } else { "" }
    );
    println!("| Body | Uplink | Bytes | Formula (ms) | Measured (ms) | Deviation |");
    println!("|---|---|---|---|---|---|");
    let bodies: Vec<(String, Vec<u8>)> = enc
        .iter()
        .flat_map(|e| {
            let wav = vec![0x55u8; e.wav_bytes];
            [(format!("{} WAV", e.label), wav), (format!("{} FLAC", e.label), e.flac.clone())]
        })
        .filter(|(l, _)| l.starts_with("tts_fillers.wav") && (l.contains("noise 30") || !l.contains("noise")))
        .collect();
    for mbps in [2.0, 5.0, 10.0, 20.0, 50.0] {
        for (label, body) in &bodies {
            let all: Vec<Run> = (0..UPLOAD_RUNS).map(|_| paced_upload(body, mbps, &timer, spin, big)).collect();
            let mut runs: Vec<f64> = all.iter().map(|r| r.total_ms).collect();
            if verbose {
                let d: Vec<String> = all.iter().map(|r| format!("{:.0}/{:.0}/{:.0}", r.total_ms, r.last_write_ms, r.max_late_ms)).collect();
                eprintln!("  runs (total/last-write/max-late ms) {label} {mbps} Mbps: {}", d.join("  "));
            }
            let measured = median(&mut runs);
            let formula = body.len() as f64 * 8.0 / (mbps * 1e6) * 1000.0;
            println!(
                "| {label} | {mbps} Mbps | {} | {formula:.0} | {measured:.0} | {:+.1}% |",
                body.len(),
                (measured - formula) / formula * 100.0
            );
        }
    }
    println!();
}

fn machine_info() {
    let env = |k: &str| std::env::var(k).unwrap_or_else(|_| "?".into());
    println!("## Machine\n");
    println!("- CPU: {} ({} logical processors)", env("PROCESSOR_IDENTIFIER"), env("NUMBER_OF_PROCESSORS"));
    if let Ok(o) = std::process::Command::new("cmd").args(["/c", "ver"]).output() {
        println!("- OS: {}", String::from_utf8_lossy(&o.stdout).trim());
    }
    println!("- Build: release, fat LTO, codegen-units 1, target {}", std::env::consts::ARCH);
    println!();
}

fn main() {
    let mut fixtures = PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/fixtures"));
    let (mut skip_upload, mut skip_encode, mut spin, mut verbose, mut big) = (false, false, false, false, true);
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--fixtures" => fixtures = PathBuf::from(args.next().expect("--fixtures DIR")),
            "--skip-upload" => skip_upload = true,
            "--skip-encode" => skip_encode = true,
            "--spin" => spin = true,
            "--verbose" => verbose = true,
            "--default-buffers" => big = false,
            other => panic!("unknown argument {other}"),
        }
    }
    machine_info();
    let clean = read_wav(&fixtures.join("tts_clean.wav"));
    let fillers = read_wav(&fixtures.join("tts_fillers.wav"));
    // Same row order as BENCHMARK.md.
    let clips = vec![
        clean.clone(),
        fillers.clone(),
        with_noise(&clean, 30, 0x5EED_0001),
        with_noise(&fillers, 30, 0x5EED_0002),
        with_noise(&fillers, 100, 0x5EED_0003),
    ];
    let enc = if skip_encode {
        clips
            .iter()
            .map(|c| Encoded { label: c.name.clone(), wav_bytes: c.wav_bytes, flac: encode_flac(&c.samples) })
            .collect()
    } else {
        run_encode(&clips)
    };
    if !skip_upload {
        run_upload(&enc, spin, big, verbose);
    }
}
