# renderer_probe

Prototype for BENCHMARK.md's "Settings private bytes" deviation: the Settings window commits about 265 MB
(`PrivateMemorySize64`) against PLAN section 1's 80 MB, almost all of it the OpenGL driver's pools. The same
Settings-like egui page is built three ways to see whether another renderer avoids that. Standalone crate (own empty
`[workspace]`, gitignored `target/`); it never joins the main build, and nothing in `src/` changed.

| Binary | Backend | Build |
|---|---|---|
| `probe_glow` | eframe 0.36, glow (OpenGL). `NativeOptions` copied from `src/ui/app.rs::native_options` (no depth, stencil or MSAA, `Preferred` hardware acceleration, vsync on, borderless 1200x800, centered) | `cargo build --release --bin probe_glow` (default feature) |
| `probe_wgpu` | eframe 0.36, wgpu 30, **DX12 only** (`Backends::DX12`); `PROBE_POWER=low` picks the low-power adapter | `cargo build --release --no-default-features --features wgpu --bin probe_wgpu` |
| `probe_cpu` | no GPU API: `winit` + `egui-winit`, egui meshes rasterized in software straight into a `softbuffer` (GDI DIB) surface | `cargo build --release --no-default-features --features cpu --bin probe_cpu` |

One backend per binary and per feature set, so every exe links exactly one renderer and its size is comparable to
`opentalk.exe`. A bare `cargo run --release` runs `probe_glow`; the others run with
`cargo run --release --no-default-features --features cpu --bin probe_cpu` (or `wgpu`). The windows stay open: close one
or let `measure.ps1` kill it. **All three
build on the GNU toolchain** (`stable-x86_64-pc-windows-gnu`, mingw gcc, no MSVC): wgpu's DX12 backend and softbuffer both
compile and run there.

## Method

- **Page** (`src/page.rs`): 192 px sidebar with six nav rows, a rounded card with a title bar, eight switch rows, one
  row of real egui widgets (button, checkbox). Painter-only like `ui/app.rs`, Segoe UI loaded from `C:\Windows\Fonts` at
  runtime (egui's bundled fonts are off, as in the real app). Static: no animation, no repaint request when idle.
- **Burst then idle** (`src/report.rs`): each probe repaints continuously for 4 s, writes one `key=value` report line to
  `$PROBE_OUT` (frame count, frame interval and per-frame CPU time with the first 10 frames dropped, time to first
  frame from `main`, which of `RTSSHooks64.dll` / `opengl32.dll` / `d3d12.dll` / vendor drivers are loaded, working set
  and private bytes before trimming), then calls `EmptyWorkingSet` (the real Settings window trims 0.5 s after losing
  focus) and stops asking for repaints.
- **`measure.ps1`** starts the exe, waits for the report, waits 2 s, then samples every 250 ms for **20 s**: `PrivateMemorySize64`,
  `WorkingSet64`, idle CPU (`TotalProcessorTime` over the window, as % of all 24 logical processors and of one core), then
  private working set (`Win32_PerfFormattedData_PerfProc_Process.WorkingSetPrivate`; the `Process V2` PDH counters used
  by `bench.ps1` return error 0x800007d2 on this machine), threads, handles and the loaded modules. 3 runs per backend
  (2 for the low-power wgpu variant). `.\measure.ps1 -Exe path\to\probe_cpu.exe [-IdleSec 20] [-Runs 3] [-ExtraEnv PROBE_POWER=low]`.
- **Frame time**: for the eframe backends `Frame::info().cpu_usage` (update + tessellate + paint + submit, the GPU work
  itself excluded); for the CPU painter the whole frame including rasterization and the blit. "Interval" is the time between frames.
- **Machine**: Intel Core i7-14650HX (24 logical processors), 23.7 GB, NVIDIA GeForce RTX 5060 Laptop GPU (32.0.16.1074)
  plus Intel UHD Graphics (Optimus), Windows 11 Home 10.0.26200, **display scaling 175% (168 dpi)**, so the 1200x800
  logical window is 2100x1400 physical pixels (2.9 Mpx). 120 Hz panel (vsync interval 8.3 ms). Laptop on mains, desktop
  locked (LogonUI) during the runs; windows, GL and DX contexts are created normally in that state, as in the Settings
  benchmark. rustc stable 1.99, `opt-level = 3`, fat LTO, codegen-units 1, stripped.

## Results

Medians of the three runs (ranges in brackets). Memory figures are the 20 s idle window, after the trim.

| | glow (as shipped) | wgpu DX12 | CPU painter |
|---|---|---|---|
| **Private bytes** (`PrivateMemorySize64`) | **262 MB** (262-262) | **520 MB** (520-523); low-power adapter 521-543 | **20 MB** (20-20) |
| Working set | 11.0 MB (10.9-11.2) | 15.2 MB (13.4-16.3) | 13.4 MB (13.3-13.4) |
| Private working set | 5.7 MB (5.6-5.8) | 8.0 MB (6.9-8.1) | 12.5 MB (12.4-12.5) |
| Working set before the trim | 145 MB | 242-276 MB | 31-33 MB |
| Idle CPU, 20 s (% of all cores / of one core) | 0.000 / 0.000 (one run 0.003 / 0.08) | 0.006 / 0.15 (range 0.003-0.019 / 0.08-0.47) | 0.000 / 0.000 |
| Frame CPU time, mean (p95) | 0.27 ms (0.37) | 0.67 ms (0.88-1.22) | 10.3 ms (13-26) at 2100x1400, full redraw |
| Frame interval during the burst | 8.3 ms p50 (vsync, 120 Hz) | 8.3 ms p50 | 13-18 ms mean (throttled to 60 Hz, not vsynced) |
| Time to first frame from `main` | 262-444 ms | 1142-2287 ms (low-power adapter: 334-728) | 39-91 ms |
| Threads / handles | 30 / 512 | 26 / 545 | 8 / 192-201 |
| `RTSSHooks64.dll` loaded | **yes** | yes | **yes** |
| GPU modules loaded | `opengl32`, `nvoglv64`, `nvwgf2umx`, `igd10iumd64`, `d3d11`, `dxgi` | `d3d12`, `d3dcompiler_47`, `nvwgf2umx`, `igd12umd64`, `d3d11`, `dxgi` | `gdi32full` only |
| **Probe exe size** (release, fat LTO, stripped) | 4,239,872 B | 7,768,064 B | 3,655,680 B |
| Delta against the glow probe | 0 | **+3,528,192 B** | **-584,192 B** |
| **Estimated real `opentalk.exe`** (now 7,901,184 B, budget 8,000,000 B) | 7.90 MB | **about 11.4 MB, over budget by 3.4 MB** | **about 7.3 MB** before the work listed below; 7.35-7.4 MB with it |

Notes on the numbers:

- `probe_glow` reproduces the problem: 262 MB private bytes against BENCHMARK's 267 MB for the real Settings window, with a
  working set of 11 MB (real: 13-40 MB). The probe is therefore a faithful stand-in for the driver cost, which does not
  depend on what the page draws.
- **`RTSSHooks64.dll` is in all three processes, including the CPU painter that never touches a GPU API and commits 20 MB.**
  The overlay hook is injected into every GUI process on this machine, so it is not what makes the GL process commit
  262 MB; what differs is the graphics stack (the GL process loads both the NVIDIA and the Intel user-mode drivers,
  `nvoglv64.dll` plus `igd10iumd64.dll`). The "plus any overlay hook" remark in BENCHMARK's Settings note should be reworded accordingly.
  (Whether pinning the process to one GPU via Windows graphics settings shrinks the GL commit was not tried: it changes
  per-user system settings.)
- **wgpu on DX12 is worse on every axis**: twice the commit (520 MB, with the DX12 and D3D11 stacks of both adapters
  loaded; the low-power adapter choice does not help), a first frame at 0.3-2.3 s, +3.5 MB of exe. It cannot meet the exe
  budget and does not meet the memory one.
- The CPU painter keeps no GPU context, so its commit is the process baseline, egui's state and fonts and one 11.8 MB
  BGRA surface. Rasterizing straight into the softbuffer DIB instead of a separate framebuffer took it from 31.6 MB to
  20 MB. Its private working set (12.5 MB) is the one figure where it loses to glow (5.7 MB), because the surface is
  really touched where the GL pools are only committed. With the real Settings content (more text, history list, icon
  font) the commit would grow; 30-45 MB is an estimate, not a measurement, and still inside the 80 MB figure.
- The CPU painter spends 9-16 ms per full-window frame at 2100x1400 (about 175% scaling, so it is the worst case this
  laptop produces; at 100% scaling the pixel count, and so roughly the time, is about a third). A page that only repaints on input is fine; sustained 60 Hz
  animation (the real Settings has none while idle) would sit at or over a frame budget. The prototype redraws the whole
  window every frame; damage rectangles (egui reports what changed through the clip rects) and a faster flat-span path
  for rounded rectangles are the obvious next steps and were not attempted. Idle CPU is exactly zero either way, since
  nothing repaints without an event.
- What the real switch would still have to do (not in the probe, and the reason for the size range above): the
  viewport commands `eframe` applies today (`StartDrag`, `BeginResize`, `Maximized`, `Minimized`, `Close`, title,
  icon, cursor icons, focus and `viewport().focused` input), IME and clipboard text through `egui-winit` (already
  linked), DPI changes, bilinear glyph sampling (the probe uses nearest, fine at 1:1 glyph-to-pixel but visibly
  coarser when egui scales), and the 8-bit blend in gamma space (identical to egui's glow path).
- The rasterizer output was checked by eye against the page (`PROBE_BMP=path` dumps the first frame as a BMP).

## Conclusion

The 262 MB commit comes with the OpenGL stack (both vendor drivers load into the process), not with the page and not with
the RivaTuner hook, and eframe's other renderer is no way out: wgpu/DX12 doubles the commit to 520 MB, delays the first
frame by up to 2 s and adds 3.5 MB to an exe that has 0.1 MB left. A CPU painter (egui meshes rasterized into a GDI surface through `softbuffer`)
is the only option measured that meets the 80 MB figure literally: **20 MB private bytes, 13 MB working set, 12.5 MB
private working set, zero idle CPU, no GPU modules loaded, first frame in 40-90 ms, and a probe exe 0.58 MB smaller
than the glow one** (estimated real exe about 7.3-7.4 MB, inside the 8,000,000-byte budget). Its price is CPU time per
repaint (9-16 ms for a full 2100x1400 frame, no damage tracking yet), a private working set about 7 MB above glow's, and
the windowing glue that `eframe` provides today. If the PLAN figure is to be met as private bytes rather than working set, switching Settings to this painter
satisfies both the exe-size and idle-CPU conditions on these numbers; the decision is left to the caller, and the
real backend was not touched.
