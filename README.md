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
- **Fast.** Mic stream prepared in advance, TLS connection pre-warmed at key press, silence trimmed before upload, streaming
  transport for Deepgram / AssemblyAI / OpenAI Realtime / Gemini Live. Local post-processing is a few milliseconds.
- **Clean text.** Deterministic filler removal (um, uh, "you know", stutters, loops, hallucinated "Thanks for watching"),
  punctuation repair, dictionary casing and fuzzy fixes, snippets; optional AI polish through any BYOK LLM that never
  blocks the paste (4 s timeout, falls back to the deterministic text).
- **Many providers.** OpenAI, Groq, xAI, Mistral, Gemini, Deepgram, AssemblyAI, Corti, Tinfoil (plain HTTPS), OpenRouter,
  Custom / Azure OpenAI. Cleanup LLMs: OpenAI, Anthropic, Gemini, Groq, OpenRouter, custom OpenAI-compatible, Azure OpenAI, Bedrock, Vertex.
- **Careful paste.** Full clipboard snapshot and restore, excluded from Windows clipboard history, waits for held modifier keys,
  Ctrl+Shift+V for terminals, tells you when the target app runs as administrator.

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

Desktop and live tests are `#[ignore]`d and named `desktop_*` / `live_*`:

```powershell
cargo test -- --ignored desktop_            # needs an interactive desktop
cargo test -- --ignored live_               # OpenRouter ":free" models only; set OPENTALK_SECRET_OPENROUTER
powershell -File tests\e2e.ps1              # daemon + injected WAV + paste into Notepad
powershell -File bench\bench.ps1            # CPU / memory / latency benchmark -> docs\BENCHMARK.md
```

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
| `opentalk.exe --render-pill <state> <t_ms> out.png [--dark]` | Render a pill state to PNG (visual goldens) |
| `opentalk.exe --bench ...` | Benchmark helpers: `toggle`, `dictate <n> <wav>`, `latency`, `mock-stt`, `info` (see `bench/bench.ps1`) |

Automation hooks: `OPENTALK_HOME` (data directory), `OPENTALK_INJECT_WAV` (play a WAV instead of the microphone),
`OPENTALK_STT_BASE_URL` (point every STT provider at a local server), `OPENTALK_SECRET_<NAME>` (API key without touching
the secret store), `OPENTALK_LOG=debug`.

## Settings

| Section | What you can change |
|---|---|
| Preferences | Theme, dictation sounds, pause media while dictating, automatic pasting, keep text on the clipboard, floating icon (auto-hide, position, app bubble), language, microphone, launch at login |
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
  dropped when you dictate again or quit (turn it off in Privacy & Data).
- API keys are encrypted with Windows DPAPI (current user) in `secrets.json`; they are never logged.
- History (text, app name, provider, timing) is a local `history.jsonl` you can clear or auto-prune.
- With AI cleanup on, the *transcribed text* (not audio) is sent to the LLM provider you chose.
- Dictation text goes through the clipboard for pasting; it is excluded from Win+V clipboard history and the previous
  clipboard contents are restored afterwards.

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

| | OpenTalk | Target | Spokenly |
|---|---|---|---|
| Idle CPU (60 s) | 0.0% | ~0% | ~3% |
| Recording CPU (30 s, real mic) | 0.41% of all cores (9.9% of one core; ~2.5% projected on 4 cores) | <= 1.5% | ~3% |
| Daemon memory, idle | 17.9 MB working set, 4.6 MB private bytes, ~3 MB private working set | <= 25 MB | ~110 MB |
| Daemon memory, recording | 19.5 MB WS (app bubble off) / 23-39 MB WS with the app bubble (shared shell DLLs; private WS ~5.5 MB) | <= 25 MB | ~110 MB |
| Settings window | 13 MB working set, separate process, 0% CPU idle | <= 80 MB | - |
| Executable | 7.51 MB, one file | <= 8 MB | - |
| Start-up | daemon ready in ~20 ms | - | - |
| Hotkey -> pill / -> first mic packet | 1.4 ms / 19.9 ms (median) | <= 16 / <= 50 ms | - |
| Local overhead stop -> paste (excluding STT) | ~1 ms before paste, ~18 ms including a real `SendInput` paste | <= 40 ms | - |

Live checks use OpenRouter `:free` models only: AI cleanup works through `nvidia/nemotron-3.5-lightning:free` (~1 s); no free
audio model returned a transcript during these runs (gated to "agentic harnesses", or upstream capacity errors).
