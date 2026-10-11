# Developing OpenTalk

How to build, test and benchmark OpenTalk, and what still has to be checked on a real Windows desktop. For using the
app, see the [README](../README.md).

## Build on Windows

Requirements: Windows 10/11 x64, Rust stable with the **GNU** target, and mingw-w64 (for `windres` and the linker).
No MSVC and no C runtime install are needed.

```powershell
rustup toolchain install stable-x86_64-pc-windows-gnu
rustup default stable-x86_64-pc-windows-gnu
# mingw-w64 (WinLibs build is fine) must be on PATH:
$env:PATH = "C:\path\to\mingw64\bin;$env:PATH"

cargo build --release          # target\release\opentalk.exe (single file, icon + manifest embedded)
cargo test                     # unit and headless end-to-end tests (no network, no desktop)
```

`build.rs` embeds the icon (`assets/opentalk.ico`), the manifest (per-monitor-v2 DPI, `asInvoker`, Windows 10/11,
long paths, UTF-8 code page) and version info with `windres`. Set `OPENTALK_NO_RESOURCES=1` to skip that step.
`cargo run --example make_icon` regenerates the icon from the OpenWhispr logo geometry.

Desktop and live tests are `#[ignore]`d and named `desktop_*` / `live_*`:

```powershell
cargo test -- --ignored desktop_            # needs an interactive desktop
cargo test -- --ignored live_               # OpenRouter ":free" models only; set OPENTALK_SECRET_OPENROUTER
powershell -File tests\e2e.ps1              # daemon + injected WAV + real paste (Notepad, or a scratch text box if Notepad is open)
powershell -File bench\bench.ps1            # CPU / memory / latency benchmark -> docs\BENCHMARK.md
```

### Cross-compile and test on Linux

The whole build and most of the test suite run on Linux, cross-compiled to the same GNU Windows target and executed
under Wine with a virtual X display. This is how the work is merged and audited; it does not replace a
Windows desktop for the checks in [the desktop checks](#checks-that-need-a-real-windows-desktop).

```sh
sudo apt install mingw-w64 wine64 xvfb          # mingw-w64 provides x86_64-w64-mingw32-{gcc,windres}
rustup target add x86_64-pc-windows-gnu
Xvfb :99 -screen 0 1920x1080x24 &               # any X server works; the pill and popup tests need a display
export CARGO_TARGET_X86_64_PC_WINDOWS_GNU_RUNNER=wine WINEDEBUG=-all DISPLAY=:99

cargo build --release --target x86_64-pc-windows-gnu    # target/x86_64-pc-windows-gnu/release/opentalk.exe
cargo test --target x86_64-pc-windows-gnu               # unit + headless end-to-end tests, run under Wine
cargo clippy --target x86_64-pc-windows-gnu --all-targets
```

Wine differs from Windows in two places, and the affected tests say so instead of failing. They detect Wine precisely
(`ntdll!wine_get_version`, `src/testenv.rs`), so on Windows they run unchanged:

- `daemon::appicon::tests::friendly_name_from_version_info` prints `SKIPPED (Wine: ...)` when Wine's `notepad.exe` has no
  `FileDescription` in its version resource.
- `daemon::pill::preview::golden::goldens_match` reports the GDI-text goldens (the toasts; the tooltips usually still
  match) as `SKIPPED (Wine: "Segoe UI" is not installed ...)` when the font is missing, because `DrawTextW` then
  substitutes another face. Every other golden is still asserted.

Visual checks of the pill over the backdrop fixtures (`tests/fixtures/pill_backdrops/`), light and dark:

```sh
EXE=target/x86_64-pc-windows-gnu/debug/opentalk.exe     # cargo build --target x86_64-pc-windows-gnu
wine $EXE --render-pill recording 1500 rec.png --backdrop busy-text --composite \
     --app-icon tests/golden/fixtures/explorer_icon.png --upscale 2
wine $EXE --render-pill error 400 err.png --dark --backdrop gradient --composite
# states: idle hover tooltip recording recording-tooltip thinking unavailable cancel cancel-hot cancel-out error error-noretry error-long
# cancel: t = ms since hover began (the button slides out); cancel-out: t = ms since the pointer left (it retracts)
# backdrops: white black gradient busy-text (or a PNG path)
```

Headless screenshots of the tray popup: Home, and every page with `--page` (Wine has no Segoe UI, so the text uses
its fallback face). Set `OPENTALK_HOME` to a data directory to render its history, dictionary and snippets:

```sh
wine $EXE --render-tray-popup popup.png [--dark] [--recording|--transcribing] [--recent] [--retry] [--hover History] [--focus]
wine $EXE --render-tray-popup page.png --page settings:cleanup [--dark] [--scroll px] [--hover id] [--edit id] [--toast text]
# pages: settings, settings:<preferences|hotkeys|stt|cleanup|privacy|about>, history, entry:<id>, dictionary, snippets,
#        snippet:new, snippet:<index>, picker:<language|inputdevice|micwarm|retention|sttprovider|llmprovider|llmmodel>
```

What does **not** work under Wine: the tray popup's real window (`Shell_NotifyIconGetRect`, `ABM_GETTASKBARPOS`),
`WDA_EXCLUDEFROMCAPTURE` (so the pill's glass falls back to its neutral backdrop and
the `desktop_pill_*backdrop*` / `capture_*` desktop tests fail), out-of-context WinEvents from other processes, DWM
acrylic, real audio devices, and the network from the Windows binaries when the host only has an HTTPS proxy (the app
does not read `HTTPS_PROXY`), so the `live_*` tests cannot reach OpenRouter there.

## Checks that need a real Windows desktop

Everything below needs a real, unlocked Windows 10/11 desktop (not Wine, not a locked session) and has not been run on the
merged build:

- **Every `desktop_*` test** (`cargo test -- --ignored --test-threads=1 desktop_`; list them with
  `cargo test -- --list --ignored | findstr desktop_`):
  - autostart: `desktop_real_key_round_trip_restores_state`, `desktop_scratch_enable_disable_cycle`
  - IPC: `desktop_post_reaches_message_only_window`, `desktop_post_reaches_top_level_window`
  - app icon: `desktop_foreground_app_icon`, `desktop_packaged_icons_load_no_shell_dlls`,
    `desktop_win32_icon_loads_no_shell_dlls`
  - audio: `desktop_record_1s`, `desktop_list_devices_from_sta_and_mta_threads`
  - hotkey: `desktop_hook_smoke`, `desktop_hook_stop_then_start_is_reliable`, `desktop_injected_paste_keys_are_filtered`,
    `desktop_main_key_not_typed_into_focused_app`, `desktop_start_menu_mask`, `desktop_start_menu_opens_without_mask`
  - media / mute: `desktop_media_roundtrip_is_fast`, `desktop_media_roundtrip_restores_playback`,
    `desktop_mute_roundtrip_restores_the_device`
  - paste: `desktop_capture_foreground`, `desktop_clipboard_history_flags`, `desktop_clipboard_snapshot_roundtrip`,
    `desktop_paste_elevated_target_copies_only`, `desktop_paste_notepad`, `desktop_paste_restores_focus`,
    `desktop_prefetch_keeps_delay_rendered_clipboard_off_the_paste_path`, `desktop_restore_outlasts_a_busy_clipboard`
  - pill: `desktop_pill_backdrop_follows_the_window_when_dragged`,
    `desktop_pill_backdrop_is_captured_only_while_shown_and_hash_gated`, `desktop_pill_cancel_button_and_error_card`,
    `desktop_pill_capture_cost`, `desktop_pill_capture_excludes_the_pill`, `desktop_pill_click_drag_hover_hittest`,
    `desktop_pill_expand_and_shrink_frames_arrive_on_time`,
    `desktop_pill_falls_back_to_neutral_glass_when_capture_is_unavailable`, `desktop_pill_frame_cost`,
    `desktop_pill_lifecycle_and_idle_is_silent`, `desktop_pill_real_app_icon_bubble`,
    `desktop_pill_resting_circle_runs_no_timer_and_refreshes_on_events`,
    `desktop_pill_steady_recording_presents_only_the_dirty_lane`, `desktop_pill_thinking_glow_presents_partial_frames`,
    `desktop_pill_waveform_diagnostics_with_the_real_microphone`
  - app bubble tracker: `desktop_tracker_prefetches_publishes_and_ignores_the_taskbar`
  - sound / tray: `desktop_sound_plays`, `desktop_tray_add_remove`, `desktop_tray_popup_opens_and_closes`,
    `desktop_popup_opens_a_page_navigates_and_closes`
  - streaming: `desktop_session_cpu_budget`, `desktop_tls_loopback_roundtrip`
  - hotkey capture in the popup: `desktop_matcher_capture_ctrl_win_and_f8`
  - end to end (`tests/e2e.rs`): `desktop_cancel_while_the_mic_starts_drops_the_dictation`, `desktop_cancel_during_rewrite`,
    `desktop_e2e_notepad`, `desktop_e2e_notepad_streaming`
- `powershell -File bench\bench.ps1`: re-measure every PLAN section 1 budget (idle CPU with the
  resting glass circle on screen, recording CPU with the glass captured on events only while recording, daemon working
  set / private bytes with the capture buffers and with a popup page open, startup, latency, and the executable size now
  that the egui window is gone) and regenerate
  [docs/BENCHMARK.md](BENCHMARK.md).
- `powershell -File tests\e2e.ps1` (`-Stt mock` and `-Stt mock-stream`): the real paste into Notepad.
- The tray popup on a real desktop (the `docs/screenshots/tray-popup/` pictures come from `--render-tray-popup`): left
  click, right click and Enter on the icon, also with the icon in the hidden-icons overflow; the position with the
  taskbar at the bottom, top, left and right, on a second monitor, and at 125 / 150 / 200 % DPI; Esc, arrow keys, Enter,
  click outside and Alt+Tab close or drive it; the second click on the icon closes it instead of reopening; the
  open time and that the closed popup costs nothing (no window, no timer). Every page in light and dark with Segoe UI;
  typing, IME composition and clipboard paste in the text fields; hotkey capture on the Hotkeys page; the popup taking
  the keyboard when opened by `--settings`, a second launch or the first run (otherwise a click on it does); the window
  resize when a page opens and closes.
- The GDI-text goldens (toasts, tooltips) in `goldens_match`, with Segoe UI installed.
- The pill's capture exclusion: `desktop_pill_capture_excludes_the_pill`, and a manual check that the pill, bubble,
  tooltip and toasts are absent from a screenshot / screen share while the glass still shows the screen behind.
- The resting circle's event-driven refresh: switch, move and minimise windows behind it and confirm the frost follows
  within about half a second, with no periodic timer (Process Explorer / `bench.ps1` idle CPU at 0).
- DPI 125 %, 150 % and 200 %, and dragging the pill between monitors with different scaling (the glass picture must be
  re-captured at the new scale; the neutral glass shows until it lands).
- The `live_*` tests (OpenRouter `:free` models only, `OPENTALK_SECRET_OPENROUTER`), from a machine that can reach
  `openrouter.ai`.


## Local models: end-to-end test

`stt::local::tests::local_engine_end_to_end` (ignored) exercises the whole local engine on the real sherpa-onnx
Windows runtime:
- **Install:** the download with hash checks, unpacking, the import patch and the model install.
- **Run:** the server start, the websocket protocol, and splitting and joining a long dictation.

It uses a synthetic CTC model that spells "hello world" for any audio, built by `tools/make_test_ctc.py`. Serve a
folder holding that archive and the two runtime packages (file names as in `src/stt/local/install.rs`), then:

```sh
pip install onnx numpy && python3 tools/make_test_ctc.py mirror/
# + mirror/sherpa_onnx_bin-1.13.8-py3-none-win_amd64.whl and mirror/sherpa-onnx-win-x64-1.13.8.tgz
python3 -m http.server 8765 --directory mirror &
OPENTALK_TEST_LOCAL_MIRROR=http://127.0.0.1:8765 cargo test -- --ignored local_engine_end_to_end
```

On Windows the system `tar.exe` unpacks. Under Wine there is none: set `OPENTALK_TAR` to a `.cmd` that runs a host
`bsdtar` (Wine's `start /wait /unix` does not wait, so the wrapper polls for a marker file). The real models'
accuracy is not part of this test; run the app with each model on Windows.

## Command line

Command line (the exe is a GUI-subsystem program, so run these from a terminal and capture the output with a pipe):

| Command | What it does |
|---|---|
| `opentalk.exe --hidden` | Start straight into the tray (what launch-at-login uses) |
| `opentalk.exe --settings [page]` | Open the tray popup at a page: `settings` (default), `settings:<preferences\|hotkeys\|stt\|cleanup\|privacy\|about>`, `history`, `dictionary`, `snippets` or `home`. A running daemon shows it; otherwise this starts the daemon and opens it |
| `opentalk.exe --transcribe-file x.wav [--provider id] [--model m] [--local <model id>] [--llm\|--no-llm] [--rewrite-trigger "phrase"\|--no-rewrite]` | Run the full pipeline headless, print JSON `{raw, text, provider, model, stt_ms, cleanup_ms, total_ms, ai_cleaned, rewrite, rewritten, rewrite_error, original}` |
| `opentalk.exe --render-pill <state> <t_ms> out.png [--dark] [--backdrop white\|black\|gradient\|busy-text [--composite]] [--upscale N]` | Render a pill state to PNG (visual goldens), optionally over a backdrop fixture (`--composite` shows the window over the fixture it sits on) |
| `opentalk.exe --render-tray-popup out.png [--dark] [--recording\|--transcribing] [--recent] [--retry] [--hover <row>] [--focus] [--page <page>]` | Render the tray popup (Home, or a page) to PNG (screenshots, structural goldens) |
| `opentalk.exe --bench ...` | Benchmark helpers: `toggle`, `start`, `stop`, `cancel`, `retry`, `reload`, `quit`, `settings [page]`, `dictate <n> <wav>`, `latency`, `mock-stt`, `info` (see `bench/bench.ps1`) |

Automation hooks: `OPENTALK_HOME` (data directory), `OPENTALK_INJECT_WAV` (play a WAV instead of the microphone),
`OPENTALK_STT_BASE_URL` (point every STT provider at a local server), `OPENTALK_LLM_BASE_URL` (the same for the cleanup /
rewrite LLM; `--bench mock-stt` also answers `/chat/completions`, see `--llm-text`, `--llm-status`, `--llm-delay-ms`),
`OPENTALK_SECRET_<NAME>` (API key without touching the secret store), `OPENTALK_LOG=debug`,
`OPENTALK_NO_GLASS_CAPTURE=1` (no screen capture for the pill's glass, for A/B timing).

## Source layout

```
src/main.rs            mode dispatch (daemon | --settings | --transcribe-file | --render-pill | --render-tray-popup | --bench)
src/daemon/app/        state machine, message window, dictation worker, pipeline, latency log, CLI modes
src/daemon/            hotkey hook, WASAPI capture + VAD, paste, pill, sounds, media pause
src/daemon/tray/       tray icon and popup: Home menu, page renderer (layout, text editor, scrolling), page models
src/stt/ src/llm/      cloud providers (batch + streaming), error classification, cleanup LLMs
src/core/              config, DPAPI secrets, history, deterministic cleanup, dictionary, HTTP
bench/ tests/          benchmark and end-to-end scripts, fixtures
docs/                  PLAN, SPEC, BENCHMARK
```

## Benchmark

Measured with `bench/bench.ps1` on an i7-14650HX (24 logical cores), release build, local mock STT so the network does not
enter the numbers. Full report: [docs/BENCHMARK.md](BENCHMARK.md).

**These Windows figures predate the liquid glass** (pill backdrop capture), **the flat Settings / tray popup rework, the live
app bubble (foreground tracker), mute-while-dictating, the detached cancel button, the vsync-paced animation, the
real-time waveform and the tray-only UI (the egui window is gone)**. The rows marked
*stale* must be re-measured with `bench.ps1` on Windows (see [the desktop checks](#checks-that-need-a-real-windows-desktop)); the executable size is
from the merged tree, cross-built on Linux.

| | OpenTalk | Target | Spokenly |
|---|---|---|---|
| Idle CPU (60 s) *stale* | 0.0% | ~0% | ~3% |
| Recording CPU (30 s, real mic) *stale* | 0.03% of all cores (0.78% of one core; ~0.2% projected on 4 cores) | <= 1.5% | ~3% |
| Daemon memory, idle *stale* | 17.9 MB working set, 4.4 MB private bytes | <= 25 MB | ~110 MB |
| Daemon memory, recording / 10 dictations (peak) *stale* | 19.1 MB WS (app bubble off), 23.2-23.4 MB WS with the app bubble and no foreground app; **37-40 MB WS with a packaged app such as Windows Terminal in front** (misses, see below) | <= 25 MB | ~110 MB |
| Executable | 4.18 MB (4,177,408 bytes), one file (cross-built on Linux, 2026-10-10, after round 3 removed the egui window; was 7,578,624 bytes) | <= 8 MB (8,000,000 bytes) | - |
| Start-up | daemon ready in ~21 ms | - | - |
| Hotkey -> pill / -> first mic packet | 1.6 ms / 20.5 ms (median) | <= 16 / <= 50 ms | - |
| Local overhead stop -> paste (excluding STT) | ~1 ms before paste; ~18 ms including a real `SendInput` paste (earlier unlocked-desktop run) | <= 40 ms | - |

Live checks use OpenRouter `:free` models only: AI cleanup works through `nvidia/nemotron-3.5-lightning:free` (~1 s); no free
audio model returned a transcript during these runs (gated to "agentic harnesses", or upstream capacity errors).

**Not yet verified.** Every benchmark run so far had the workstation locked, where Windows denies clipboard access and
`SendInput`. Still to run on an unlocked desktop: the real paste into Notepad (`tests/e2e.ps1`), the `desktop_*` hotkey and
paste tests (including Excel stop-to-paste and clipboard restore, and delay-rendered clipboard owners), the daemon memory
with Notepad or Explorer in front. Live speech-to-text stays UNVERIFIED: no free
OpenRouter model delivers the audio, and a paid run needs the owner's decision (PLAN section 12b). Details:
[docs/BENCHMARK.md, "Not yet verified"](BENCHMARK.md).

## Design documents

- [SPEC.md](SPEC.md): the visual and behavioural spec (pill, tray popup, pages, animations).
- [PLAN.md](PLAN.md): the original plan, budgets and decisions.
- [BENCHMARK.md](BENCHMARK.md): the full benchmark report.
