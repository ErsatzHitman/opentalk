# OpenTalk

Ultra-light voice dictation for Windows. Press a hotkey anywhere, talk, press it again, and the cleaned-up text appears
where your cursor is. Speech-to-text is **cloud only** (bring your own API key), so there is no model to download and the
whole app is one small native executable.

OpenTalk re-implements the dictation experience of [OpenWhispr](https://github.com/OpenWhispr/openwhispr) in Rust, with
the same floating recording pill, and adds a deterministic filler-word cleaner, an active-app bubble and a much smaller footprint.

## Highlights

- **Tiny and idle.** The daemon is plain Win32 + WASAPI. No GPU, no web view, no polling: ~0% CPU when idle (see
  [docs/BENCHMARK.md](docs/BENCHMARK.md)). The settings window is a separate process that only exists while it is open.
- **The OpenWhispr pill.** A 40 px circle that grows into a capsule with a live 11-bar waveform, a signal glow while it
  transcribes, cancel button, tooltip and error card, plus a round bubble showing the app you are dictating into.
  The surfaces are **liquid glass**: they frost and refract the screen behind them. Because that needs a clean copy of the
  screen, the pill is excluded from screen capture, so it does **not appear in screenshots, screen recordings or screen
  sharing** (Windows 10 2004 or newer; on older builds, or when a capture fails, it falls back to neutral frosted glass).
  The capture only runs while the pill is visible: a small copy of the screen around it, about 4 times a second while
  recording. While it rests there is no timer at all: the circle's glass is refreshed only when something happens (it
  appears, moves or changes state, you hover it, or another window is activated, moved, minimised or restored), so its
  frost can lag a little behind the screen (see "Pill surface material" in [docs/SPEC.md](docs/SPEC.md)).
- **Liquid-glass control panel.** History, Dictionary and Settings are frosted, refracting glass panels over an
  ambient colour mesh and the Windows 11 acrylic backdrop (opaque page when acrylic is unavailable, when Windows'
  Transparency effects are off, or with *Reduce transparency* on). Design notes: [docs/GLASS.md](docs/GLASS.md).
- **Fast.** Mic stream prepared in advance, TLS connection pre-warmed at key press, silence trimmed before upload, streaming
  transport for Deepgram / AssemblyAI / OpenAI Realtime / Gemini Live. Local post-processing is a few milliseconds.
- **Clean text.** Deterministic filler removal (um, uh, "you know", stutters, loops, hallucinated "Thanks for watching"),
  punctuation repair, dictionary casing and fuzzy fixes, snippets; optional AI polish through any BYOK LLM that never
  blocks the paste (4 s timeout, falls back to the deterministic text).
- **Many providers.** OpenAI, Groq, xAI, Mistral, Gemini, Deepgram, AssemblyAI, Corti, Tinfoil (plain HTTPS), OpenRouter,
  Custom / Azure OpenAI. Cleanup LLMs: OpenAI, Anthropic, Gemini, Groq, OpenRouter, custom OpenAI-compatible, Azure OpenAI, Bedrock, Vertex.
- **Careful paste.** Full clipboard snapshot and restore, excluded from Windows clipboard history, waits for held modifier keys,
  Ctrl+Shift+V for terminals, tells you when the target app runs as administrator.

## Download

Get `opentalk.exe` from [Releases](https://github.com/ErsatzHitman/opentalk/releases): **latest** is rebuilt from `main` on
every push, tagged versions (`v*`) are fixed releases. It is one portable file with no installer: put it anywhere and run it.
The exe is not code-signed, so Windows SmartScreen may warn on the first run (**More info > Run anyway**); compare it with
`opentalk.exe.sha256` if you want to check the download. Built by `.github/workflows/release.yml` (cross-compiled on Linux,
see below). Windows 10/11 x64.

## Build

Requirements: Windows 10/11 x64, Rust stable with the **GNU** target, and mingw-w64 (for `dlltool` and `windres`).
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
It also turns `opengl32.dll` into a delay-loaded import with `dlltool`, so the daemon never loads OpenGL (only the
Settings window draws with it); `OPENTALK_NO_DELAYLOAD=1` keeps the normal import.
`cargo run --example make_icon` regenerates the icon from the OpenWhispr logo geometry.
`build.rs` looks for dlltool as `$DLLTOOL`, then (when the host is not Windows) `x86_64-w64-mingw32-dlltool`, then plain
`dlltool`; if none works it warns "dlltool unavailable" and keeps the load-time import.

Desktop and live tests are `#[ignore]`d and named `desktop_*` / `live_*`:

```powershell
cargo test -- --ignored desktop_            # needs an interactive desktop
cargo test -- --ignored live_               # OpenRouter ":free" models only; set OPENTALK_SECRET_OPENROUTER
powershell -File tests\e2e.ps1              # daemon + injected WAV + real paste (Notepad, or a scratch text box if Notepad is open)
powershell -File bench\bench.ps1            # CPU / memory / latency benchmark -> docs\BENCHMARK.md
```

### Cross-compile and test on Linux

The whole build and most of the test suite run on Linux, cross-compiled to the same GNU Windows target and executed
under Wine with a virtual X display. This is how the liquid-glass work was merged and audited; it does not replace a
Windows desktop for the checks in [Run on Windows](#run-on-windows).

```sh
sudo apt install mingw-w64 wine64 xvfb          # mingw-w64 provides x86_64-w64-mingw32-{gcc,dlltool,windres}
rustup target add x86_64-pc-windows-gnu
Xvfb :99 -screen 0 1920x1080x24 &               # any X server works; the pill and egui tests need a display
export CARGO_TARGET_X86_64_PC_WINDOWS_GNU_RUNNER=wine WINEDEBUG=-all DISPLAY=:99

cargo build --release --target x86_64-pc-windows-gnu    # target/x86_64-pc-windows-gnu/release/opentalk.exe
cargo test --target x86_64-pc-windows-gnu               # unit + headless end-to-end tests, run under Wine
cargo clippy --target x86_64-pc-windows-gnu --all-targets
```

No dlltool wrapper is needed: `build.rs` finds `x86_64-w64-mingw32-dlltool` itself (see Build).

Wine differs from Windows in two places, and the affected tests say so instead of failing. They detect Wine precisely
(`ntdll!wine_get_version`, `src/testenv.rs`), so on Windows they run unchanged:

- `daemon::appicon::tests::friendly_name_from_version_info` prints `SKIPPED (Wine: ...)` when Wine's `notepad.exe` has no
  `FileDescription` in its version resource.
- `daemon::pill::preview::golden::goldens_match` reports the GDI-text goldens (the error cards; the tooltips usually still
  match) as `SKIPPED (Wine: "Segoe UI" is not installed ...)` when the font is missing, because `DrawTextW` then
  substitutes another face. Every other golden is still asserted.

Visual checks of the pill over the backdrop fixtures (`tests/fixtures/pill_backdrops/`), light and dark:

```sh
EXE=target/x86_64-pc-windows-gnu/debug/opentalk.exe     # cargo build --target x86_64-pc-windows-gnu
wine $EXE --render-pill recording 1500 rec.png --backdrop busy-text --composite \
     --app-icon tests/golden/fixtures/explorer_icon.png --upscale 2
wine $EXE --render-pill error 400 err.png --dark --backdrop gradient --composite
# states: idle hover tooltip recording recording-tooltip thinking unavailable cancel cancel-hot error error-noretry error-long
# backdrops: white black gradient busy-text (or a PNG path)
```

Headless screenshots of every Settings / History / Dictionary page (egui rendered on the CPU, a synthetic wallpaper in
place of acrylic; icon glyphs show as `?` because Segoe Fluent Icons has no free stand-in):

```sh
OPENTALK_SHOT_DIR=docs/screenshots/glass/offscreen cargo test --target x86_64-pc-windows-gnu --bin opentalk offscreen_shots
```

What does **not** work under Wine: `WDA_EXCLUDEFROMCAPTURE` (so the pill's glass falls back to its neutral backdrop and
the `desktop_pill_*backdrop*` / `capture_*` desktop tests fail), out-of-context WinEvents from other processes, DWM
acrylic, real audio devices, and the network from the Windows binaries when the host only has an HTTPS proxy (the app
does not read `HTTPS_PROXY`), so the `live_*` tests cannot reach OpenRouter there.

## Run on Windows

Everything below needs a real, unlocked Windows 10/11 desktop (not Wine, not a locked session) and has not been run on the
merged liquid-glass build:

- **Every `desktop_*` test** (`cargo test -- --ignored --test-threads=1 desktop_`; list them with
  `cargo test -- --list --ignored | findstr desktop_`):
  - autostart: `desktop_real_key_round_trip_restores_state`, `desktop_scratch_enable_disable_cycle`
  - IPC: `desktop_post_reaches_message_only_window`, `desktop_post_reaches_top_level_window`,
    `desktop_settings_page_post_reaches_window`
  - app icon: `desktop_foreground_app_icon`, `desktop_packaged_icons_load_no_shell_dlls`,
    `desktop_win32_icon_loads_no_shell_dlls`
  - audio: `desktop_record_1s`, `desktop_list_devices_from_sta_and_mta_threads`
  - hotkey: `desktop_hook_smoke`, `desktop_hook_stop_then_start_is_reliable`, `desktop_injected_paste_keys_are_filtered`,
    `desktop_main_key_not_typed_into_focused_app`, `desktop_start_menu_mask`, `desktop_start_menu_opens_without_mask`
  - media: `desktop_media_roundtrip_is_fast`, `desktop_media_roundtrip_restores_playback`
  - paste: `desktop_capture_foreground`, `desktop_clipboard_history_flags`, `desktop_clipboard_snapshot_roundtrip`,
    `desktop_paste_elevated_target_copies_only`, `desktop_paste_notepad`, `desktop_paste_restores_focus`,
    `desktop_prefetch_keeps_delay_rendered_clipboard_off_the_paste_path`, `desktop_restore_outlasts_a_busy_clipboard`
  - pill: `desktop_pill_backdrop_follows_the_window_when_dragged`,
    `desktop_pill_backdrop_is_captured_only_while_shown_and_hash_gated`, `desktop_pill_cancel_button_and_error_card`,
    `desktop_pill_capture_cost`, `desktop_pill_capture_excludes_the_pill`, `desktop_pill_click_drag_hover_hittest`,
    `desktop_pill_falls_back_to_neutral_glass_when_capture_is_unavailable`, `desktop_pill_frame_cost`,
    `desktop_pill_lifecycle_and_idle_is_silent`, `desktop_pill_real_app_icon_bubble`,
    `desktop_pill_resting_circle_runs_no_timer_and_refreshes_on_events`,
    `desktop_pill_steady_recording_presents_only_the_dirty_lane`, `desktop_pill_thinking_glow_presents_partial_frames`
  - sound / tray: `desktop_sound_plays`, `desktop_tray_add_remove`
  - streaming: `desktop_session_cpu_budget`, `desktop_tls_loopback_roundtrip`
  - Settings hotkey capture: `desktop_capture_ctrl_win`, `desktop_capture_escape_cancels_and_f8_works`
  - end to end (`tests/e2e.rs`): `desktop_cancel_while_the_mic_starts_drops_the_dictation`, `desktop_e2e_notepad`,
    `desktop_e2e_notepad_streaming`
- `powershell -File bench\bench.ps1`: re-measure every PLAN section 1 budget on the glass build (idle CPU with the
  resting glass circle on screen, recording CPU with the 4 Hz backdrop capture, daemon working set / private bytes with
  the capture buffers, Settings working set with the glass pages, startup, latency) and regenerate
  [docs/BENCHMARK.md](docs/BENCHMARK.md).
- `powershell -File tests\e2e.ps1` (`-Stt mock` and `-Stt mock-stream`): the real paste into Notepad.
- Live Settings window screenshots over real acrylic (`docs/screenshots/glass/live-*.png` predate the page migration):
  every page in light and dark, and with *Reduce transparency* / Transparency effects off.
- The GDI-text goldens (error cards, tooltips) in `goldens_match`, with Segoe UI installed.
- The pill's capture exclusion: `desktop_pill_capture_excludes_the_pill`, and a manual check that the pill, bubble,
  tooltip and error card are absent from a screenshot / screen share while the glass still shows the screen behind.
- The resting circle's event-driven refresh: switch, move and minimise windows behind it and confirm the frost follows
  within about half a second, with no periodic timer (Process Explorer / `bench.ps1` idle CPU at 0).
- DPI 125 %, 150 % and 200 %, and dragging the pill between monitors with different scaling (the glass picture must be
  re-captured at the new scale; the neutral glass shows until it lands).
- The `live_*` tests (OpenRouter `:free` models only, `OPENTALK_SECRET_OPENROUTER`), from a machine that can reach
  `openrouter.ai`.

## Using it

1. Run `opentalk.exe`. It lives in the system tray. The first launch opens **Settings > Speech-to-Text** so you can pick a
   provider and paste an API key.
2. Focus any text field and press **Ctrl + Win** (default). Speak. Press it again to finish; the text is pasted.
   - **Tap to talk** (default): press to start, press to stop.
   - **Push to talk**: hold to record (starts after 150 ms), release to stop; a shorter press does nothing.
3. **Esc** cancels while recording or transcribing. The pill's `x` button and the tray menu do the same.
4. Click the pill to toggle recording, right-click it for *Hide for now*. Left-click the tray icon (or launch the exe
   again) to open the app window with History, Dictionary and Settings.
5. If a transcription fails (network, quota) the error card offers **Retry** with the audio that was kept in memory.

Command line (the exe is a GUI-subsystem program, so run these from a terminal and capture the output with a pipe):

| Command | What it does |
|---|---|
| `opentalk.exe --hidden` | Start straight into the tray (what launch-at-login uses) |
| `opentalk.exe --settings [history\|dictionary\|settings[:section]]` | Open the control panel |
| `opentalk.exe --transcribe-file x.wav [--provider id] [--model m] [--llm\|--no-llm]` | Run the full pipeline headless, print JSON `{raw, text, provider, model, stt_ms, cleanup_ms, total_ms}` |
| `opentalk.exe --render-pill <state> <t_ms> out.png [--dark] [--backdrop white\|black\|gradient\|busy-text [--composite]] [--upscale N]` | Render a pill state to PNG (visual goldens), optionally over a backdrop fixture (`--composite` shows the window over the fixture it sits on) |
| `opentalk.exe --bench ...` | Benchmark helpers: `toggle`, `dictate <n> <wav>`, `latency`, `mock-stt`, `info` (see `bench/bench.ps1`) |

Automation hooks: `OPENTALK_HOME` (data directory), `OPENTALK_INJECT_WAV` (play a WAV instead of the microphone),
`OPENTALK_STT_BASE_URL` (point every STT provider at a local server), `OPENTALK_SECRET_<NAME>` (API key without touching
the secret store), `OPENTALK_LOG=debug`.

## Settings

| Section | What you can change |
|---|---|
| Preferences | Theme, reduce transparency (opaque window instead of the acrylic glass backdrop), dictation sounds, pause media while dictating, automatic pasting, keep text on the clipboard, floating icon (auto-hide, position, app bubble), language, microphone, launch at login |
| Hotkeys | Dictation hotkey (modifier-only combos allowed), Tap / Push to talk, Start-menu suppression for Win combos |
| Speech-to-Text | Provider, model, API key (masked), custom endpoint, streaming preference, **Test** button |
| Cleanup | Filler removal (Off / Standard / Aggressive), fuzzy dictionary, spoken line breaks, AI cleanup provider / model / key / prompt |
| Dictionary | Words that bias recognition and fix casing; snippets (trigger -> replacement) |
| History | Searchable, with app, provider and latency per entry; copy, delete, retry |
| Privacy & Data | History on/off and retention, keep failed audio for Retry, save discarded dictations |

Changes are written to `%APPDATA%\OpenTalk\config.json` and the running daemon re-applies them immediately (hotkey, pill,
theme, cleanup, microphone) without a restart.

## Privacy

- Audio is recorded **only** between your start and stop (or while you hold the key) and sent to the speech-to-text
  provider you chose, over HTTPS (or WSS for streaming). Nothing is sent anywhere else, and there is no telemetry or account.
- Audio is never written to disk. After a *failed* transcription the last recording stays in memory so Retry works, and is
  dropped by the next dictation that succeeds or fails, when you turn *keep failed audio* off in Privacy & Data, or
  when you quit.
- API keys are encrypted with Windows DPAPI (current user) in `secrets.json`; they are never logged.
- History (text, app name, provider, timing) is a local `history.jsonl` you can clear or auto-prune.
- With AI cleanup on, the *transcribed text* (not audio) is sent to the LLM provider you chose.
- Dictation text goes through the clipboard for pasting; it is excluded from Win+V clipboard history and the previous
  clipboard contents are restored afterwards (unless *Keep transcription in clipboard* is on).

## Layout

```
src/main.rs            mode dispatch (daemon | --settings | --transcribe-file | --render-pill | --bench)
src/daemon/app/        state machine, message window, dictation worker, pipeline, latency log, CLI modes
src/daemon/            hotkey hook, WASAPI capture + VAD, paste, tray, pill, sounds, media pause
src/stt/ src/llm/      cloud providers (batch + streaming), error classification, cleanup LLMs
src/core/              config, DPAPI secrets, history, deterministic cleanup, dictionary, HTTP
src/ui/                egui control panel (separate process)
bench/ tests/          benchmark and end-to-end scripts, fixtures
docs/                  PLAN, SPEC, BENCHMARK
```

## Benchmark

Measured with `bench/bench.ps1` on an i7-14650HX (24 logical cores), release build, local mock STT so the network does not
enter the numbers. Full report: [docs/BENCHMARK.md](docs/BENCHMARK.md).

**These Windows figures predate the liquid glass** (pill backdrop capture, glass Settings pages). The rows marked
*stale* must be re-measured with `bench.ps1` on Windows (see [Run on Windows](#run-on-windows)); the executable size is
from the merged tree, cross-built on Linux.

| | OpenTalk | Target | Spokenly |
|---|---|---|---|
| Idle CPU (60 s) *stale* | 0.0% | ~0% | ~3% |
| Recording CPU (30 s, real mic) *stale* | 0.03% of all cores (0.78% of one core; ~0.2% projected on 4 cores) | <= 1.5% | ~3% |
| Daemon memory, idle *stale* | 17.9 MB working set, 4.4 MB private bytes | <= 25 MB | ~110 MB |
| Daemon memory, recording / 10 dictations (peak) *stale* | 19.1 MB WS (app bubble off), 23.2-23.4 MB WS with the app bubble and no foreground app; **37-40 MB WS with a packaged app such as Windows Terminal in front** (misses, see below) | <= 25 MB | ~110 MB |
| Settings window *stale* | 12.4 MB working set, separate process, ~0% CPU idle (commits ~267 MB of GL driver pools, documented) | <= 80 MB | - |
| Executable | 7.51 MB (7,505,920 bytes), one file (merged glass tree, 2026-10-10) | <= 8 MB (8,000,000 bytes) | - |
| Start-up | daemon ready in ~21 ms | - | - |
| Hotkey -> pill / -> first mic packet | 1.6 ms / 20.5 ms (median) | <= 16 / <= 50 ms | - |
| Local overhead stop -> paste (excluding STT) | ~1 ms before paste; ~18 ms including a real `SendInput` paste (earlier unlocked-desktop run) | <= 40 ms | - |

Live checks use OpenRouter `:free` models only: AI cleanup works through `nvidia/nemotron-3.5-lightning:free` (~1 s); no free
audio model returned a transcript during these runs (gated to "agentic harnesses", or upstream capacity errors).

**Not yet verified.** Every benchmark run so far had the workstation locked, where Windows denies clipboard access and
`SendInput`. Still to run on an unlocked desktop: the real paste into Notepad (`tests/e2e.ps1`), the `desktop_*` hotkey and
paste tests (including Excel stop-to-paste and clipboard restore, and delay-rendered clipboard owners), the daemon memory
with Notepad or Explorer in front, and Settings private bytes with RTSS exited. Live speech-to-text stays UNVERIFIED: no free
OpenRouter model delivers the audio, and a paid run needs the owner's decision (PLAN section 12b). Details:
[docs/BENCHMARK.md, "Not yet verified"](docs/BENCHMARK.md).
