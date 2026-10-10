# OpenTalk

Ultra-light voice dictation for Windows. Press a hotkey anywhere, talk, press it again, and the cleaned-up text appears
where your cursor is. Speech-to-text is **cloud only** (bring your own API key), so there is no model to download and the
whole app is one small native executable.

OpenTalk re-implements the dictation experience of [OpenWhispr](https://github.com/OpenWhispr/openwhispr) in Rust, with
the same floating recording pill, and adds a deterministic filler-word cleaner, an active-app bubble and a much smaller footprint.

## Highlights

- **Tiny and idle.** The daemon is plain Win32 + WASAPI. No GPU, no web view, no polling: ~0% CPU when idle (see
  [docs/BENCHMARK.md](docs/BENCHMARK.md)). There is no app window: everything lives in the tray popup, which exists only
  while it is open.
- **The OpenWhispr pill.** A 40 px circle that grows into a capsule with a live 11-bar waveform, a signal glow while it
  transcribes, tooltip and error card, plus a round bubble showing the app you are dictating into and, while you hover
  the pill, a round cancel button that pops out of it on hover like a drop (a liquid neck that pinches off, then a springy bounce; the bubble's twin, on the right). The bubble is ready the instant you press the hotkey (the app's icon is looked up when it comes to the front, with no polling) and follows you if you switch apps while dictating: **the text is pasted into the app the bubble shows when it is ready**. The taskbar, Start, tray flyouts, Alt-Tab and OpenTalk's own pill and tray menu never count as a target; they keep the previous app. (With the bubble turned off nothing is tracked and the paste goes to the window the hotkey went down in.)
  The surfaces are **liquid glass**: they frost and refract the screen behind them. Because that needs a clean copy of the
  screen, the pill is excluded from screen capture, so it does **not appear in screenshots, screen recordings or screen
  sharing** (Windows 10 2004 or newer; on older builds, or when a capture fails, it falls back to neutral frosted glass).
  The capture only runs while the pill is visible, and only on events: a small copy of the screen around it when the pill
  appears, moves or changes state, while you hover it, or when another window is activated, moved, minimised or restored
  (while recording nothing is captured periodically, so the waveform never waits for it; transcribing and animations
  refresh it a few times a second). The frost can therefore lag a little behind the screen (see "Pill surface material" in [docs/SPEC.md](docs/SPEC.md)).
- **Everything in the tray popup.** Clicking the tray icon opens a small flat popup next to the tray (solid surfaces, 1 px
  borders, light / dark / system theme; the glass is only on the pill) with Dictation, History, Dictionary, Settings and a
  few shortcuts. History, Dictionary, Snippets and every Settings page open inside the same popup, with a back button
  (see "Using it"). There is no separate window or process.
- **Fast.** Mic stream prepared in advance, TLS connection pre-warmed at key press, silence trimmed before upload, streaming
  transport for Deepgram / AssemblyAI / OpenAI Realtime / Gemini Live. Local post-processing is a few milliseconds.
- **Clean text.** Deterministic filler removal (um, uh, "you know", stutters, loops, hallucinated "Thanks for watching"),
  punctuation repair, dictionary casing and fuzzy fixes, snippets; optional AI polish through any BYOK LLM that never
  blocks the paste (4 s timeout, falls back to the deterministic text).
- **Voice rewrite.** End a dictation with a phrase you choose ("rewrite this concisely" by default) and the text you
  dictated is rewritten more concisely by your AI provider before it is pasted.
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
Windows desktop for the checks in [Run on Windows](#run-on-windows).

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
- `daemon::pill::preview::golden::goldens_match` reports the GDI-text goldens (the error cards; the tooltips usually still
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

## Run on Windows

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
  [docs/BENCHMARK.md](docs/BENCHMARK.md).
- `powershell -File tests\e2e.ps1` (`-Stt mock` and `-Stt mock-stream`): the real paste into Notepad.
- The tray popup on a real desktop (the `docs/screenshots/tray-popup/` pictures come from `--render-tray-popup`): left
  click, right click and Enter on the icon, also with the icon in the hidden-icons overflow; the position with the
  taskbar at the bottom, top, left and right, on a second monitor, and at 125 / 150 / 200 % DPI; Esc, arrow keys, Enter,
  click outside and Alt+Tab close or drive it; the second click on the icon closes it instead of reopening; the
  open time and that the closed popup costs nothing (no window, no timer). Every page in light and dark with Segoe UI;
  typing, IME composition and clipboard paste in the text fields; hotkey capture on the Hotkeys page; the popup taking
  the keyboard when opened by `--settings`, a second launch or the first run (otherwise a click on it does); the window
  resize when a page opens and closes.
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
3. **Esc** cancels while recording or transcribing. The pill's `x` button does the same.
4. Click the pill to toggle recording, right-click it for *Hide for now*.
5. **Click the tray icon** (left or right click, or Enter / Space on it from the keyboard; it works from the hidden-icons
   overflow too) to open a small popup at the bottom-right, next to the tray. It shows OpenTalk's status (Ready /
   Recording / Transcribing), a **Start / Stop dictation** button with the hotkey, **History**, **Dictionary**,
   **Settings**, your last two dictations (click one to copy it), **Show / Hide floating pill**, **Retry last dictation**
   (only after a failure) and **Quit**. Use the mouse, or the arrow keys, Home / End, Tab, Enter and Esc. It closes when
   you pick a row, press Esc or click anywhere else; clicking the tray icon again closes it too. While it is open it follows
   the dictation (a hotkey press or a finished transcription updates the Start / Stop row in place). **Start dictation**
   here is the same as the hotkey: same sounds, mute, app bubble and paste target (the app you were in before you clicked
   the tray). **History**, **Dictionary** and **Settings** open as pages inside the popup (380 px wide, as tall as their content up to 600 px; longer pages scroll with the wheel or keys); the arrow at the
   top left, Backspace, Alt+Left or Esc go back. Settings lists Preferences, Hotkeys, Speech-to-Text, Cleanup & AI,
   Privacy & Data and About (one icon row each); choices such as the language or provider open a list page;
   the Dictionary page links to Snippets. Changes apply at once. Actions are small icon buttons (the Windows 11 icon font,
   Segoe Fluent Icons; Segoe MDL2 Assets on Windows 10) that name themselves on hover: edit / delete on a saved API key,
   play to run a test, copy beside each text of a dictation. History rows show the Windows icon of the app each dictation
   went to. Launching the exe a second time opens the popup.
6. If a transcription fails (network, quota) the error card offers **Retry** with the audio that was kept in memory.
7. **Voice rewrite.** Dictate your message and finish with the rewrite phrase, e.g. *"Let's move the meeting to Friday
   afternoon because the client is out on Thursday, rewrite this concisely."* OpenTalk sends the text (without the phrase)
   to your AI cleanup provider, model and key and pastes the shorter version instead. It works even when *Enable AI
   cleanup* is off, as long as a key is set.
   - The phrase must be the **last** thing you say; one "please" / "thanks" / "thank you" after it is fine, and an
     "and", "then", "so", "please" or "also" right before it is dropped. Case, punctuation and "re-write" spellings do not matter; there is
     no fuzzy matching ("write this concisely" does not trigger it).
   - Change or switch off the phrase in **Settings > Cleanup & AI**: it needs two words (six letters), at most 80 characters,
     and no filler words (they are removed from dictations before matching). An empty phrase turns the feature off.
     A sentence that really ends with your phrase is rewritten too; say one more word, or turn it off.
   - The Thinking glow stays up while the provider answers (up to 10 s, `ai_cleanup.rewrite_timeout_ms` in config.json);
     **Esc** cancels and pastes nothing. If the call fails, times out or the answer looks wrong (a refusal, longer than
     your text), your text is pasted as dictated, without the phrase, and a card says why (*Rewrite timed out*, *Check your
     AI key*, *Add an AI key to rewrite*, ...). Saying only the phrase pastes nothing (*Nothing to rewrite*). Bodies of three
     words or fewer are pasted as spoken, without a call. Snippets expand after the rewrite.
   - History keeps the concise text as the entry text, the full recognised speech as the raw text and the text before the
     rewrite as `original`, with `rewritten: true`.

Command line (the exe is a GUI-subsystem program, so run these from a terminal and capture the output with a pipe):

| Command | What it does |
|---|---|
| `opentalk.exe --hidden` | Start straight into the tray (what launch-at-login uses) |
| `opentalk.exe --settings [page]` | Open the tray popup at a page: `settings` (default), `settings:<preferences\|hotkeys\|stt\|cleanup\|privacy\|about>`, `history`, `dictionary`, `snippets` or `home`. A running daemon shows it; otherwise this starts the daemon and opens it |
| `opentalk.exe --transcribe-file x.wav [--provider id] [--model m] [--llm\|--no-llm] [--rewrite-trigger "phrase"\|--no-rewrite]` | Run the full pipeline headless, print JSON `{raw, text, provider, model, stt_ms, cleanup_ms, total_ms, ai_cleaned, rewrite, rewritten, rewrite_error, original}` |
| `opentalk.exe --render-pill <state> <t_ms> out.png [--dark] [--backdrop white\|black\|gradient\|busy-text [--composite]] [--upscale N]` | Render a pill state to PNG (visual goldens), optionally over a backdrop fixture (`--composite` shows the window over the fixture it sits on) |
| `opentalk.exe --render-tray-popup out.png [--dark] [--recording\|--transcribing] [--recent] [--retry] [--hover <row>] [--focus] [--page <page>]` | Render the tray popup (Home, or a page) to PNG (screenshots, structural goldens) |
| `opentalk.exe --bench ...` | Benchmark helpers: `toggle`, `start`, `stop`, `cancel`, `retry`, `reload`, `quit`, `settings [page]`, `dictate <n> <wav>`, `latency`, `mock-stt`, `info` (see `bench/bench.ps1`) |

Automation hooks: `OPENTALK_HOME` (data directory), `OPENTALK_INJECT_WAV` (play a WAV instead of the microphone),
`OPENTALK_STT_BASE_URL` (point every STT provider at a local server), `OPENTALK_LLM_BASE_URL` (the same for the cleanup /
rewrite LLM; `--bench mock-stt` also answers `/chat/completions`, see `--llm-text`, `--llm-status`, `--llm-delay-ms`),
`OPENTALK_SECRET_<NAME>` (API key without touching the secret store), `OPENTALK_LOG=debug`,
`OPENTALK_NO_GLASS_CAPTURE=1` (no screen capture for the pill's glass, for A/B timing).

## Settings

All in the tray popup (**Settings** row, or `opentalk.exe --settings`):

| Page | What you can change |
|---|---|
| Preferences | Theme, dictation sounds, pause media while dictating, mute system audio while dictating (on by default), automatic pasting, keep text on the clipboard, floating icon (auto-hide, position, app bubble), language, microphone, launch at login |
| Hotkeys | Dictation hotkey (modifier-only combos allowed), Tap / Push to talk, Start-menu suppression for Win combos |
| Speech-to-Text | Provider, model, API key (masked), custom endpoint, streaming preference, **Test** button |
| Cleanup & AI | AI rewrite (on/off, trigger phrase) and AI cleanup (on/off, prompt): two independent switches, either, both or neither; then the AI model both use (provider, key, model, test); then on-device cleanup (filler removal Off / Standard / Aggressive, fuzzy dictionary, spoken line breaks). If AI cleanup fails, the transcript is pasted anyway and History marks the entry *STT fallback* |
| Privacy & Data | History on/off and retention, keep failed audio for Retry, save discarded dictations, clear history |
| About | Version, data and log folders, reset app data |
| Dictionary (Home row) | Words that bias recognition and fix casing (add, edit, filter, import from / copy to the clipboard); Snippets (trigger -> replacement) |
| History (Home row) | Searchable, with app, provider and latency per entry; detail page with the text, the text before a rewrite and the raw transcript; copy, delete, retry |

Changes are applied by the daemon at once (hotkey, pill, theme, cleanup, microphone) and saved to
`%APPDATA%\OpenTalk\config.json`; an external edit of that file is picked up with `opentalk.exe --bench reload`.
Dictating into a popup text field is not supported: the popup is OpenTalk's own window, so the text goes to the app you
were in before.

## Privacy

- Audio is recorded **only** between your start and stop (or while you hold the key) and sent to the speech-to-text
  provider you chose, over HTTPS (or WSS for streaming). Nothing is sent anywhere else, and there is no telemetry or account.
- Audio is never written to disk. After a *failed* transcription the last recording stays in memory so Retry works, and is
  dropped by the next dictation that succeeds or fails, when you turn *keep failed audio* off in Privacy & Data, or
  when you quit.
- API keys are encrypted with Windows DPAPI (current user) in `secrets.json`; they are never logged.
- *Mute system audio while dictating* mutes the default playback device while you record and restores it the moment the
  recording ends (stop, cancel, error or quit). It never touches a device that was already muted, and while muted
  it keeps a tiny `muted-device.txt` (the device id) in `%APPDATA%\OpenTalk` so a crash cannot leave you muted.
- History (text, app name, provider, timing) is a local `history.jsonl` you can clear or auto-prune.
- With AI cleanup on, or when you end a dictation with your rewrite phrase, the *transcribed text* (not audio) is sent to the
  LLM provider you chose. Voice rewrite is on by default but only sends text when you say the phrase; turn it off in
  Settings > Cleanup & AI.
- Dictation text goes through the clipboard for pasting; it is excluded from Win+V clipboard history and the previous
  clipboard contents are restored afterwards (unless *Keep transcription in clipboard* is on).

## Layout

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
enter the numbers. Full report: [docs/BENCHMARK.md](docs/BENCHMARK.md).

**These Windows figures predate the liquid glass** (pill backdrop capture), **the flat Settings / tray popup rework, the live
app bubble (foreground tracker), mute-while-dictating, the detached cancel button, the vsync-paced animation, the
real-time waveform and the tray-only UI (the egui window is gone)**. The rows marked
*stale* must be re-measured with `bench.ps1` on Windows (see [Run on Windows](#run-on-windows)); the executable size is
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
[docs/BENCHMARK.md, "Not yet verified"](docs/BENCHMARK.md).
