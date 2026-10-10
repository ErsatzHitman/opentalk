//! Frame statistics and the end-of-burst report the PowerShell driver waits for.
//!
//! Each probe renders continuously for [`BURST_SECS`] (frame statistics), writes one `key=value` line to the
//! file named by `PROBE_OUT` (and stdout), then stops requesting repaints so the remaining time is a true
//! idle: that is the window `measure.ps1` samples.

use std::sync::OnceLock;
use std::time::Instant;

use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::ProcessStatus::{EmptyWorkingSet, GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS_EX};
use windows::Win32::System::Threading::GetCurrentProcess;
use windows::core::PCWSTR;

pub const BURST_SECS: f64 = 4.0;
/// Frames dropped from the statistics (shader / atlas / swapchain warm-up).
const WARMUP_FRAMES: usize = 10;

static MAIN_START: OnceLock<Instant> = OnceLock::new();

/// Call first thing in `main`: the first-frame time is measured from here (window and GPU context creation included).
pub fn mark_start() {
    let _ = MAIN_START.set(Instant::now());
}

pub struct Probe {
    backend: &'static str,
    process_start: Instant,
    start: Option<Instant>,
    last: Option<Instant>,
    first_frame_ms: f64,
    intervals: Vec<f32>,
    cpu: Vec<f32>,
    done: bool,
}

impl Probe {
    pub fn new(backend: &'static str) -> Self {
        Self { backend, process_start: *MAIN_START.get_or_init(Instant::now), start: None, last: None, first_frame_ms: 0.0, intervals: Vec::new(), cpu: Vec::new(), done: false }
    }

    /// Call at the start of every frame. `prev_cpu_ms` is the CPU time of the previous frame (update + tessellate
    /// + paint + submit) when the backend can tell. Returns true while the burst is running (keep repainting).
    pub fn frame(&mut self, prev_cpu_ms: Option<f32>) -> bool {
        let now = Instant::now();
        let start = *self.start.get_or_insert_with(|| {
            self.first_frame_ms = (now - self.process_start).as_secs_f64() * 1000.0;
            now
        });
        if let Some(last) = self.last {
            self.intervals.push((now - last).as_secs_f32() * 1000.0);
        }
        self.last = Some(now);
        if let Some(c) = prev_cpu_ms {
            self.cpu.push(c);
        }
        let running = (now - start).as_secs_f64() < BURST_SECS;
        if !running && !self.done {
            self.done = true;
            self.write_report();
        }
        running
    }

    fn write_report(&self) {
        let stat = |v: &[f32]| -> (f32, f32, f32) {
            let v: Vec<f32> = v.iter().skip(WARMUP_FRAMES).copied().collect();
            if v.is_empty() {
                return (0.0, 0.0, 0.0);
            }
            let mut s = v.clone();
            s.sort_by(|a, b| a.partial_cmp(b).unwrap());
            let p = |q: f64| s[(((s.len() - 1) as f64) * q).round() as usize];
            (v.iter().sum::<f32>() / v.len() as f32, p(0.5), p(0.95))
        };
        let (im, i50, i95) = stat(&self.intervals);
        let (cm, c50, c95) = stat(&self.cpu);
        let mods: Vec<String> = [
            "RTSSHooks64.dll",
            "opengl32.dll",
            "d3d12.dll",
            "dxgi.dll",
            "d3d11.dll",
            "vulkan-1.dll",
            "nvoglv64.dll",
            "nvwgf2umx.dll",
            "igd10iumd64.dll",
            "igd12umd64.dll",
        ]
        .iter()
        .map(|m| format!("{m}={}", module_loaded(m)))
        .collect();
        let line = format!(
            "backend={} pid={} frames={} first_frame_ms={:.0} interval_ms_mean={im:.2} p50={i50:.2} p95={i95:.2} cpu_ms_mean={cm:.2} p50={c50:.2} p95={c95:.2} modules: {}",
            self.backend,
            std::process::id(),
            self.intervals.len() + 1,
            self.first_frame_ms,
            mods.join(" "),
        );
        let (ws, private) = memory_mb();
        let line = format!("{line} before_trim_ws_mb={ws:.1} before_trim_private_mb={private:.1}");
        println!("{line}");
        if let Some(path) = std::env::var_os("PROBE_OUT") {
            let _ = std::fs::write(path, format!("{line}\n"));
        }
        // The real Settings window trims its working set half a second after losing focus (ui/app.rs).
        // SAFETY: the pseudo handle from GetCurrentProcess is always valid for this process.
        unsafe {
            let _ = EmptyWorkingSet(GetCurrentProcess());
        }
    }
}

fn module_loaded(name: &str) -> bool {
    let w: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
    // SAFETY: `w` is a NUL-terminated UTF-16 string that outlives the call; the call only reads the loader table.
    unsafe { GetModuleHandleW(PCWSTR(w.as_ptr())).is_ok() }
}

/// (working set, private bytes) of this process in MB.
fn memory_mb() -> (f64, f64) {
    let mut c = PROCESS_MEMORY_COUNTERS_EX::default();
    // SAFETY: `c` is a properly sized, writable PROCESS_MEMORY_COUNTERS_EX and the pseudo handle is always valid.
    let ok = unsafe { GetProcessMemoryInfo(GetCurrentProcess(), (&raw mut c).cast(), size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32) };
    if ok.is_err() {
        return (0.0, 0.0);
    }
    (c.WorkingSetSize as f64 / 1048576.0, c.PrivateUsage as f64 / 1048576.0)
}
