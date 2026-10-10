# OpenTalk — Implementation Plan

Personal, ultra-light Windows dictation app. Written in Rust. Uses cloud speech-to-text only (no local models). Inspired by OpenWhispr (`D:\openwhispr`, v1.10.3).
Companion spec with exact pixel and colour values: `docs/SPEC.md`.

---

## 0. Status before implementation

| Item | State |
|---|---|
| Codebase analysis | Done. Five parallel Sonnet agents (high effort) covered the pill, filler cleanup, Settings/History/Dictionary UI, the cloud STT pipeline, and Windows hotkey/paste/tray. Findings are distilled into `docs/SPEC.md`. |
| Toolchain | Rust 1.99, `stable-x86_64-pc-windows-gnu`. WinLibs mingw-w64 is at `C:\Users\aksha\tools\mingw64` and provides `dlltool`. MSVC is not installed. A dependency smoke test passed: eframe 0.36 (glow), cpal 0.18, ureq 2 + native-tls (schannel), tiny-skia 0.12. |
| Project | `D:\opentalk` has `cargo init` and a one-line smoke `main.rs` only. Nothing else is implemented. |
| Test credentials | OpenRouter key only (scratchpad, session-only). The Groq key was deleted and Groq will not be used. Two baseline Groq STT calls ran before that instruction arrived. |
| Reference image | Spokenly bubble: dark rounded card, app icon plus app name ("Windows Explorer"), dotted level line. We take only the round app-icon bubble from it. |

---

## 1. Targets (must meet or beat Spokenly: ~3% CPU, 110 MB RAM)

| Metric | Target | How we measure |
|---|---|---|
| Idle CPU (daemon) | ≈0.0% (no timers, no polling) | `bench.ps1` samples `TotalProcessorTime` over 60 s |
| Recording CPU | ≤1.5% (60 fps pill plus capture) | Same, during a scripted 30 s recording |
| Daemon RAM | ≤25 MB working set, ≤15 MB private | `Get-Process` WS/PrivateMemorySize, idle and recording |
| Settings window RAM | ≤80 MB, and only while open | Separate process that exits on close |
| Disk | Single exe ≤8 MB, no installer runtime | File size; history and config are KB-scale |
| Hotkey to pill visible | ≤16 ms | Timestamped debug log (QPC) |
| Hotkey to mic capturing | ≤50 ms | Log: hook event → first WASAPI packet |
| Stop to text pasted | Network RTT + ≤40 ms local overhead | Log: stop → request sent → response → paste done |
| Upload size | Silence-trimmed 16 kHz mono (FLAC optional, see Phase 7) | Bytes logged per request |

---

## 2. Architecture

```
opentalk.exe            (daemon, default: Win32 only, no GPU, no WebView)
 ├─ main thread         hidden message window, tray, pill window, state machine
 ├─ hook thread         WH_KEYBOARD_LL → PostMessage(WM_APP_*)
 ├─ capture thread      WASAPI event-driven, started on hotkey only
 ├─ anim thread         high-resolution waitable timer, posts WM_APP_TICK only while animating
 └─ worker thread(s)    HTTP: prewarm, transcribe, cleanup

opentalk.exe --settings [--page history|dictionary|settings]
                        (egui/eframe glow process, spawned on demand, exits on close)
```

- **Daemon and settings talk through files plus window messages.**
  - `%APPDATA%\OpenTalk\config.json` holds settings. Secrets are DPAPI-encrypted with `CryptProtectData`, matching OpenWhispr's safeStorage.
  - `history.jsonl` is append-only.
  - The settings process posts `WM_APP_RELOAD` and `WM_APP_SUSPEND_HOTKEYS` to the daemon window (`FindWindowW("OpenTalkDaemon")`).
- **Single instance.** Named mutex `Local\OpenTalk.Daemon`. A second launch tells the daemon to open Settings, then exits.
- **Why two processes.** The egui/OpenGL stack, which costs about 40–70 MB, is never resident while you dictate. The daemon touches only Win32, WASAPI, schannel and tiny-skia.

### Crates (all pure Rust, so they build with the GNU toolchain and need no C compiler)

| Need | Crate | Note |
|---|---|---|
| Win32 / COM / WASAPI / DPAPI | `windows` 0.62 | Talk to WASAPI directly instead of through cpal. `AUTOCONVERTPCM` gives 16 kHz mono i16 from the OS resampler, so we write no resampler. |
| Pill rendering | `tiny-skia` | Rendered into a premultiplied BGRA DIB, then `UpdateLayeredWindow` |
| Pill text (tooltip, error card) | GDI `DrawTextW` into a mask DIB | Native Segoe UI, no font crate |
| HTTP | `ureq` 2 + `native-tls` (schannel) | Persistent agent, keep-alive, own User-Agent (Cloudflare blocks default UAs) |
| WebSocket (streaming providers) | `tungstenite` + native-tls | Phase 6 only |
| JSON | `serde`, `serde_json` | |
| Settings UI | `eframe` 0.36 (glow, default fonts off) | Fonts load from `C:\Windows\Fonts` (Segoe UI Variable plus Segoe Fluent Icons for glyphs) |
| Resources | `embed-resource` (windres from mingw) | Icon plus PerMonitorV2 DPI manifest |

---

## 3. Module layout

```
src/main.rs            arg dispatch (daemon | --settings | --bench | --transcribe-file)
src/core/config.rs     schema, defaults, migration, load/save, change notify
src/core/secrets.rs    DPAPI encrypt/decrypt for API keys
src/core/history.rs    jsonl append, read, delete, retention prune
src/core/cleanup.rs    deterministic filler/hallucination/stutter/punctuation pipeline (unit-tested)
src/core/dictionary.rs dictionary prompt building, case-fix, fuzzy term correction, snippets
src/stt/mod.rs         Provider trait, request builder, response parser, error classifier
src/stt/{openai,groq,xai,mistral,gemini,deepgram,assemblyai,corti,tinfoil,openrouter,custom}.rs
src/llm/mod.rs         cleanup LLM providers + OpenWhispr prompt + output validation
src/daemon/app.rs      state machine Idle→Arming→Recording→Thinking→Pasting→Idle (+Error, Cancel)
src/daemon/hotkey.rs   LL hook, combo matcher, Start-menu mask, Esc cancel, injected filter
src/daemon/audio.rs    WASAPI capture, RMS meter, ring buffer, VAD trim
src/daemon/pill.rs     layered window, layout/animation, hit-testing, drag, menu
src/daemon/glow.rs     signal-glow (conic ring, halo, comet), pre-blurred sprites
src/daemon/appicon.rs  foreground exe → icon (UWP aware), cache
src/daemon/paste.rs    clipboard snapshot/restore, SendInput, terminal detection
src/daemon/tray.rs     Shell_NotifyIcon + menu
src/daemon/sound.rs    synthesized cues → in-memory WAV → PlaySound
src/ui/…               egui app: shell, theme tokens, widgets, history, dictionary, settings
tools/mock_stt.py      local mock for every provider's wire format (contract tests)
bench/bench.ps1        CPU/RAM/latency benchmark harness
```

---

## 4. Cloud BYOK providers (full OpenWhispr parity)

### 4a. Speech-to-text

| Provider | Transport | Endpoint and auth | Dictionary param | Models (default first) | Phase |
|---|---|---|---|---|---|
| OpenAI | HTTP multipart (SSE when `stream=true`) | `api.openai.com/v1/audio/transcriptions`, Bearer | `prompt` (8000 chars for gpt-4o*, 900 for whisper); `keywords[]` for gpt-transcribe | gpt-4o-mini-transcribe, gpt-4o-transcribe, gpt-transcribe, whisper-1 | 2 |
| Groq | HTTP multipart | `api.groq.com/openai/v1/audio/transcriptions`, Bearer | `prompt` ≤890 | whisper-large-v3-turbo, whisper-large-v3 | 2 (not tested) |
| xAI | HTTP multipart | `api.x.ai/v1/stt`, Bearer, no `model` field | `keyterm` ×≤100 | grok-stt | 2 |
| Mistral | HTTP multipart | `api.mistral.ai/v1/audio/transcriptions`, `x-api-key` | `context_bias` ×≤100 | voxtral-mini-latest | 2 |
| Gemini | HTTP JSON (base64 audio) | `generativelanguage.googleapis.com/v1beta/interactions`, `x-goog-api-key` | `custom_vocabulary` | gemini-3.5-transcribe (+ -live, streaming) | 2 / 6 |
| Deepgram | WebSocket (OpenWhispr streams only), plus REST `/v1/listen` as a batch fallback | `wss://api.deepgram.com/v1/listen`, `Token <key>` | `keyterm` (nova-3) / `keywords` | nova-3 | 6 (REST in 2) |
| AssemblyAI | WebSocket v3 (temp token `GET /v3/token`) | `streaming.assemblyai.com/v3/ws` | `keyterms_prompt` ≤100 | universal-3-5-pro, universal-streaming-en/multi | 6 |
| Corti | OAuth client-credentials, then interaction/upload/transcript/delete | `api.<env>.corti.app/v2` | none | corti-transcribe | 6 |
| Tinfoil | Attested TLS client in OpenWhispr | `inference.tinfoil.sh/v1` | `prompt` | voxtral-small-24b (batch) | Decision needed (§11) |
| OpenAI Realtime | WebSocket, 24 kHz PCM | `wss://api.openai.com/v1/realtime?intent=transcription` | none | gpt-4o(-mini)-transcribe | 6 |
| Custom / Azure | HTTP multipart, OpenAI-compatible | User URL; Bearer, or `api-key` header for Azure | `prompt` | Free text (default whisper-1) | 2 |
| **OpenRouter** (new) | Chat completions with `input_audio` | `openrouter.ai/api/v1/chat/completions`, Bearer | Dictionary in system prompt | User-chosen audio-capable model | 2. This is the test path. |

- **Shared provider trait.** `fn build(&Audio, &Opts) -> Request`, `fn parse(Response) -> Result<Text, SttError>`.
- **Error classifier.** A port of `providerHttpErrors.js`, covering `PROVIDER_AUTH_FAILED`, `QUOTA_EXHAUSTED`, `RATE_LIMITED`, `PAYLOAD_TOO_LARGE`, `UNREACHABLE` and the rest.
- **Shared rules.**
  - Language param is omitted when set to `auto`.
  - Dictionary prompt budgets as in OpenWhispr `dictionaryPromptCap.js`.
  - Dictionary-echo filter (90% / 70% rule).
  - API-key bodies are redacted from logs.

### 4b. Cleanup LLM (optional AI polish)

The cleanup LLM supports the same BYOK set as OpenWhispr's `dictationCleanup` scope, minus local:

| Provider | Endpoint |
|---|---|
| OpenAI | Responses API |
| Anthropic | Messages API |
| Gemini | |
| Groq | |
| OpenRouter | |
| Custom OpenAI-compatible (LAN / self-hosted) | |

- **Prompt.** OpenWhispr's `cleanupPrompt` verbatim, with the `<transcript>` wrapper, the language instruction and the dictionary suffix.
- **Request.** Temperature 0, `requireCompleteOutput`.
- **Improvements over OpenWhispr.**
  - 4 s timeout with no retries, instead of 30 s plus 3 retries.
  - On failure, use the deterministic result instead of raw text.
  - Validation adds a duplicate-halves check (OpenWhispr's), an output/input length ratio between 0.3 and 3, and preamble stripping.
- **Enterprise clouds** (Azure OpenAI, Bedrock with SigV4, Vertex) are in Phase 8. Azure OpenAI works earlier through the Custom provider.

---

## 5. Recording indicator: exact OpenWhispr reproduction

Every number below comes from `docs/SPEC.md`.

- **Window.** Layered 208×120 at DPI scale, with `WS_EX_LAYERED|TOPMOST|TOOLWINDOW|NOACTIVATE`. It is placed at the work area's bottom-right with a 4 px margin; the pill sits 16 px from the work-area edge. Position is selectable (bottom-right, center, bottom-left) like `panelStartPosition`. On each hotkey press it moves to the display under the cursor.
  - Transparent pixels pass clicks through, because layered hit-testing uses alpha. That is better than OpenWhispr, whose whole 208×120 box blocks the mouse.
- **States.** idle, hover, processing (warm-up pulse), recording, thinking (signal glow), unavailable, cancel button, tooltip, error card.
  - There is no success state.
  - The idle 40 px circle stays visible by default. Auto-hide is a setting.
- **Animation timeline.**
  1. 0–260 ms: circle in recording colours.
  2. 260 ms: expand to 98×36 over 300 ms with `cubic-bezier(.2,0,0,1)`.
  3. 660 ms: waveform fades in over 200 ms.
  4. On stop: shrink to 40×40 over 300 ms and fade the glow in over 220 ms.
- **Waveform.**
  - 11 bars, each 2 px wide with 3 px gaps, height 4–22 px.
  - The capture thread computes RMS (about 43 ms window) and publishes it through an atomic.
  - Bar height is `4 + min(1,(rms·8)^0.75)·18`.
  - Every 80 ms the bars shift left by one, newest on the right, and each bar eases over 75 ms.
- **Signal glow.** tiny-skia has no conic gradients or blur, so it is drawn by hand:
  - Ring: computed analytically per pixel (about 2.5k pixels).
  - Halo and comet: pre-blurred once at start-up into 64 px sprites. Each frame rotates the comet and scales and breathes the halo.
  - Timing: spin period 1.6 s, breathe period 1.9 s.
- **Logo.** Ring of r=10 with stroke 2, plus three bars, drawn as tiny-skia paths at 22 px.
- **Theme.** Follows the Windows app theme (`AppsUseLightTheme`), with the light and dark tokens from SPEC.
- **Rendering cost.** Rendering happens only while animating (recording or thinking) at 60 fps, using a cached background. Idle does no work.
- **Verification.**
  - `--render-pill <state> <t_ms> out.png` produces golden PNGs.
  - A side-by-side check against OpenWhispr's real pill in each state, captured by running OpenWhispr dev, or by rendering its React component in a browser if the app can't run.

### Spokenly-style active-app bubble (new)
- A circle to the left of the pill with an 8 px gap. Its **diameter equals the pill's current height**: 36 while recording and 40 while thinking. It animates with the pill.
- Chrome matches the pill (surface-1 fill, border-hover stroke, dark shadow). The app icon is about 22 px, crisp at the current DPI.
- **Icon source.**
  1. `GetForegroundWindow` → PID → `QueryFullProcessImageNameW`.
  2. For UWP, resolve `ApplicationFrameHost` to the child `CoreWindow` PID.
  3. Get the icon with `SHGetFileInfoW` or `IShellItemImageFactory` at 32/48 px.
  4. Cache it per exe path.
- **Timing.** Captured at the hotkey press, which is also the paste target, so it matches the window we will paste into. It fades in with the expansion and fades out after the paste.
- **Hover tooltip.** Shows the app name, e.g. "Windows Explorer", using the friendly name from `FileDescription`.

---

## 6. Filler-word removal: preserve and improve

OpenWhispr has **no deterministic filler removal**. It relies entirely on the LLM prompt, and pastes raw text if the LLM is off or fails. OpenTalk keeps the LLM path (now optional) and adds a fast, deterministic, unit-tested pipeline that always runs (under 1 ms).

1. **Non-speech tags.** Strip `[BLANK_AUDIO]`, `[Music]`, `(silence)`, `♪…`, and the whole-string `[blank_audio]` marker.
2. **Hallucination guard.** Drop phrases like "Thank you for watching", "Please subscribe" and "Продолжение следует" only when the VAD found under 0.6 s of speech. A real "Thank you." still survives.
3. **Loop collapse.** If an n-gram (n = 1–6) repeats four or more times in a row, keep one copy.
4. **Hesitation fillers**, regex with the `uh-huh`, `mm-hmm` and `uh-oh` back-channels excluded:
   - English: `um+, uh+, uhm, erm, er, hmm+, mm+`.
   - German: äh/ähm/öhm. French: euh. Italian: ehm. Japanese: えーと/えっと/あのー. Chinese: 呃.
   - The language comes from the setting or the detected language.
5. **Context fillers.** "like" and "you know" are removed only when comma-bounded (`, like,` or `, you know,`), at the start of a sentence before a comma, or right after another filler. "I like it", "looks like", "would like", "like a" and "do you know" stay.
6. **Aggressive mode** (opt-in): also removes comma-bounded "I mean", "basically", "actually", "literally", "sort of" and "kind of".
7. **Stutters and false starts.** Collapse "the the", "I I I", "I- I" and "wh- what" for a fixed list of function words. "had had", "that that" and "very very" are kept.
8. **Punctuation repair.** Remove orphan commas and ellipses, fix `", ."` → `"."`, collapse spaces, and re-capitalise sentence starts. In English, lone `i` becomes `I`.
9. **Dictionary case-fix** (new). A case-insensitive exact match takes the dictionary casing, e.g. "openai" → "OpenAI".
10. **Conservative fuzzy correction** (new, can be turned off). A 1–3-token span within edit distance 1 (≤2 for terms of 9 or more characters) of a dictionary term is replaced, unless the span is a single common English word. Example: "open whisper" → "OpenWhispr".
11. **Spoken line breaks** (opt-in): "new line" and "new paragraph".
12. **Snippet expansion** runs last. It ports OpenWhispr's `snippets.ts` matching (longest-first, Unicode boundaries, Turkish i handling) and fixes its trailing-punctuation quirk.

- **Tests.** All OpenWhispr fixtures are ported: cleanupOutput, snippets, dictionaryEcho, transcriptionResultParsing. Add 60 or more filler fixtures, among them the Windows TTS sample, whose raw Whisper output is `"Um, so I think, uh, we should like ship the new release on Friday, you know, because the the client is waiting."`.
- **Settings.** Filler removal Off / Standard (default) / Aggressive. AI cleanup is a separate toggle and runs after the deterministic pass.

---

## 7. Settings, History and Dictionary

OpenWhispr already has History (its Home page) and a Dictionary page with Snippets. We keep both as dedicated pages, plus the Settings modal. All are built in egui with OpenWhispr's tokens.

- **Shell.**
  - Window 1200×800, frameless, with custom title controls.
  - Sidebar 192 px: Home/History, Dictionary, then Settings and Support at the bottom.
  - Top bar 48 px: title, a search pill (Ctrl+K), and window controls.
  - Content card has a 12 px radius on the `surface-window` backdrop.
- **History.**
  - Rows grouped by day (Today, Yesterday, or "Oct 9, 2026"), each group in one card with sticky headers.
  - Each row shows the time, the text, and hover actions: Copy, a ⋯ menu (View raw transcript, Retry if audio was kept, Delete).
  - Header actions: "Clear all" and "Show discarded".
  - Additions beyond OpenWhispr: a real search box, the target app icon and name on each row, and the latency, provider and model on each row.
  - Empty state: "Start your first dictation" with a hotkey hint.
- **Dictionary.**
  - Pill tabs: Dictionary and Snippets.
  - An add input that doubles as a filter ("Add words separated by commas…"), bulk import and export, a vertical list with hover edit and remove, and the default agent-name row.
  - Warning when the prompt exceeds the provider budget.
  - Snippets: trigger → replacement, with a create panel and an edit dialog.
- **Settings modal**, 896 px × 85vh, with a 208 px grouped sidebar. Sections:
  - **Preferences:** Theme (Light/Dark/Auto), Dictation sounds, Pause media, Automatic pasting, Keep transcription in clipboard, Floating icon (auto-hide, start position), Transcription language, Launch at login, Start minimized, Microphone (device and keep-warm).
  - **Hotkeys:** Dictation hotkey (default `Ctrl+Win`, modifier-only combos allowed), Tap/Hold, cancel key.
  - **Speech-to-Text:** provider pill tabs, API key field with a "Get key →" link and masked display, model card list, custom endpoint fields, and a Test button (new) that sends a 1 s TTS sample.
  - **Cleanup:** filler mode, "Enable AI cleanup", LLM provider/model/key, Prompt Studio (view, edit and reset the prompt).
  - **Privacy & Data:** history on/off, transcript retention, audio retention (default off, to save disk), Save discarded dictations.
  - **System:** version, open logs folder, reset app data.
  - Account, billing, notes, meetings, agent, connectors and local models are dropped.
- **Hotkey capture.** The settings process installs a temporary LL hook while capturing and tells the daemon to suspend its hook.

---

## 8. Windows integration details

- **Hotkey.**
  - `WH_KEYBOARD_LL` on its own thread. A port of `windows-key-listener.c` matching: modifier superset, any press order, a self-heal for missed key-ups, and left/right Win.
  - Improvements:
    - Ignore `LLKHF_INJECTED` events.
    - Swallow Esc only while recording or thinking.
    - Suppress the Start menu for `Ctrl+Win` by sending mask key `0xE8` before the Win key-up is passed on.
  - Modes:
    - **Tap** (default): press toggles.
    - **Hold**: start after 150 ms, release stops, a shorter press cancels, safety cap 5 min.
- **Paste.**
  - Snapshot all HGLOBAL clipboard formats.
  - Write the text with `ExcludeClipboardContentFromMonitorProcessing` and `CanIncludeInClipboardHistory=0` (new), so dictation doesn't flood Win+V history.
  - Restore the captured HWND if focus moved: `AttachThreadInput` + `SetForegroundWindow`.
  - Wait up to 1.5 s for physically held modifiers to be released (no fake key-ups).
  - `SendInput` Ctrl+V, or Ctrl+Shift+V for terminals. Terminals are matched by OpenWhispr's class list (`ConsoleWindowClass`, `CASCADIA_HOSTING_WINDOW_CLASS`, mintty, …) plus exe list (termius, tabby, wave, rio).
  - Restore the clipboard after 500 ms, only if `GetClipboardSequenceNumber` is unchanged.
  - Smart trailing space (OpenWhispr `smartSpacing`).
  - Detect an elevated target (UIPI) and show a "Text copied — target app is running as admin" error card instead of failing silently (new).
- **Tray.**
  - Static icon.
  - Menu: Start/Stop listening, Show/Hide floating icon, Open OpenTalk, History, Dictionary, Retry last, Quit.
  - Left click opens Settings.
- **Autostart.** HKCU `Run` value `OpenTalk` = `"<exe>" --hidden`. The state reads `StartupApproved\Run` too, mirroring OpenWhispr's `executableWillLaunchAtLogin` lesson.
- **Sounds.** OpenWhispr cues synthesised once:
  - Start: C5→E5. Stop: D5→A4.
  - Each note 90 ms with a 25 ms gap; gain 0.2.
  - Played with `PlaySoundW(SND_MEMORY|SND_ASYNC)`.

---

## 9. Latency strategy

1. **Show the pill first.** The hook posts to the main thread, which repaints the cached idle frame. Target under 16 ms.
2. **Pre-built capture.** Keep `IMMDeviceEnumerator` and the device resolved at start-up, and re-resolve on `IMMNotificationClient` default-device change. The `IAudioClient` is created at the hotkey press, which is about 10–20 ms. An optional "keep mic warm" setting is offered, mirroring `micWarmHoldSeconds`, default off, because Windows shows the mic indicator while it is warm.
3. **TLS pre-warm at hotkey press.** A background `HEAD`/`GET` opens the TLS connection to the provider host while you speak, so the upload reuses a warm keep-alive socket. That saves about 50–150 ms.
4. **Trim leading and trailing silence** (energy VAD with 150 ms padding) before upload. Smaller body, fewer hallucinations.
5. **Upload format.** 16 kHz mono PCM WAV. Phase 7 measures FLAC (`flacenc`, pure Rust): adopt it if it saves at least 30 ms on a median connection.
6. **Streaming providers** (Deepgram, AssemblyAI, Realtime) send audio during recording, so stop-to-text is roughly the finalize time (about 150–300 ms).
7. **Local post-processing:** deterministic cleanup under 1 ms. Paste is about 10 ms delay plus `SendInput`.
8. **History write** happens after the paste, on the worker thread.

---

## 10. Testing (OpenRouter only, free models only)

| Layer | Method |
|---|---|
| Unit | `cargo test`: cleanup pipeline (all ported OpenWhispr fixtures plus new ones), snippets, dictionary prompt caps, echo filter, hotkey parser and matcher, provider request builders (golden multipart bodies), response parsers, error classifier, config migration. |
| Provider contracts | `tools/mock_stt.py` replays each provider's documented request and response shape (OpenAI SSE, xAI, Mistral, Gemini JSON, Deepgram REST and WS, AssemblyAI WS, Corti flow). Every provider's request is asserted byte-for-byte against OpenWhispr's format. No real keys are needed. |
| Live STT | **Best effort, reported honestly; not an exit gate.** OpenRouter lists exactly three `:free` models with audio input (re-checked 2026-10-10): `thinkingmachines/inkling-small:free` and `inkling:free` (403 "only available on agentic harnesses") and `nvidia/nemotron-3-nano-omni-30b-a3b-reasoning:free`. The last mostly answers 502 ResourceExhausted; when it answers, `usage.prompt_tokens_details.audio_tokens` is 0 and the model says it needs the audio file, so the free NVIDIA endpoint discards the audio (our `input_audio`/wav request shape is correct). No free model currently yields a transcript, so live STT is **UNVERIFIED** and STT correctness rests on the mock contract tests. The `live_*` tests print `VERIFIED` (a transcript with the spoken words), `UNVERIFIED` (limits, gated model, audio dropped) or `FAILED` (bad key or request, always a failure). `OPENTALK_LIVE_REQUIRE=1` turns `UNVERIFIED` into a failure, for runs that have a model able to verify. Retries and backoff on 429/5xx. |
| Live cleanup LLM | OpenRouter `:free` text models (`nvidia/nemotron-3.5-lightning:free`, `google/gemma-4-31b-it:free`, `liquid/lfm-2.5-2.6b:free`). Check output validation and the 4 s timeout fallback. |
| End-to-end | `opentalk.exe --transcribe-file x.wav` runs the full pipeline headless. A scripted dictation sends `SendInput` of the hotkey, plays TTS into the mic (or uses `--inject-wav` to bypass the mic), and pastes into Notepad. Assert the Notepad text. |
| Visual | Golden PNGs per pill state and time sample, plus a screenshot of the real layered window. Compare against OpenWhispr reference captures. |
| Groq | Not used. The Groq provider is verified only through the mock. |

---

## 11. Benchmark (`bench/bench.ps1`)

- **Scenarios:**
  - idle for 60 s;
  - 10 dictations of 10 s each (injected WAV);
  - a 30 s continuous recording;
  - Settings open and idle.
- **Metrics:**
  - CPU % (process time delta / wall / cores);
  - working set and private bytes (peak and steady);
  - handle and thread count;
  - exe size;
  - startup time;
  - each latency from §1, read from the QPC-stamped log.
- **Output:** a Markdown table in `docs/BENCHMARK.md`, with Spokenly's figures (3% CPU, 110 MB) alongside and OpenWhispr's measured on the same machine if it runs.

---

## 12. Phases

| # | Phase | Deliverable | Exit check |
|---|---|---|---|
| 1 | Core skeleton | Daemon loop, config/secrets/history, tray, single instance, logging, `--transcribe-file` | Builds; tray works; config round-trips; DPAPI works |
| 2 | STT batch providers | OpenAI, Groq, xAI, Mistral, Gemini, Deepgram REST, Custom/Azure, OpenRouter; error classifier | Mock contract tests pass; live OpenRouter transcription is reported VERIFIED/UNVERIFIED (see decision below), never a gate |
| 3 | Capture, hotkey, paste | WASAPI capture plus VAD trim, LL hook (Tap/Hold/Esc/Start mask), paste plus clipboard restore, sounds | Scripted dictation lands in Notepad; latency logged |
| 4 | Pill and app bubble | Exact pill states and animations, glow, tooltip, error card, cancel, drag, menu, app-icon bubble | Golden images match SPEC; recording CPU ≤1.5% |
| 5 | Cleanup | Deterministic pipeline, LLM cleanup (OpenAI, Anthropic, Gemini, Groq, OpenRouter, Custom), validation, snippets, dictionary fixes | Fixture suite green; OpenRouter free-model cleanup works with fallback |
| 6 | Streaming and remaining BYOK | Deepgram WS, AssemblyAI WS, OpenAI Realtime, Gemini Live, Corti, Tinfoil per decision | Mock WS tests pass |
| 7 | Settings UI | egui shell, History, Dictionary/Snippets, Settings modal, hotkey capture, provider Test button | Every setting persists and the daemon applies it live |
| 8 | Hardening and bench | Enterprise (Bedrock/Vertex), FLAC experiment, DPI and multi-monitor, UIPI, bench report, polish | `BENCHMARK.md` meets the §1 targets |

---

## 12a. Risks

- **egui fidelity.** Tailwind subtleties (blur, gradients, shadows) need custom painting. The plan is to paint shadows and gradients by hand where they matter (cards, buttons) and accept minor differences elsewhere.
- **No real keys for most providers.** Correctness rests on the contract tests against OpenWhispr's exact wire formats. Live verification is limited to OpenRouter, and its free tier currently cannot verify STT at all (see §12b decision 0).
- **OpenRouter free tier is rate-limited** (Gemma returned 429). Live tests retry with backoff and rotate across free models.
- **Start-menu mask key** can interact with other hook tools such as AutoHotkey. It is behind a setting, default on.

## 12b. Open decisions (for the user)

0. **Live STT verification (provisional default, owner may override).** No free OpenRouter model delivers audio to its model, so a live STT transcript is unobtainable at zero cost. Options were: (a) allow one paid OpenRouter audio model or another provider key for a single live run, or (b) accept mock-only STT verification. **Recorded default: (b), pending the owner's confirmation (nothing paid is spent without it).** §10 and the Phase 2 exit check are amended accordingly: Phase 2 passes on the mock contract tests; the live tests stay in the suite and report `UNVERIFIED` rather than green. The strict policy (OpenRouter, `:free` only) is unchanged. If the owner later supplies a paid key and approves one run, that run is made with `OPENTALK_LIVE_REQUIRE=1` and a model override, the `:free` assertion in the live tests is relaxed for that run only, and this entry is updated to record the verified model and date.

1. **Tinfoil.** OpenWhispr uses an attested enclave client. Choose one: (a) plain HTTPS without attestation, (b) skip Tinfoil, or (c) port attestation later. Recommendation: (b), unless you use it.
2. **Corti and enterprise clouds** (Bedrock SigV4, Vertex). Keep in Phase 6/8, or drop for a personal build?
3. **Default STT provider for your daily use**, once a production key exists. Testing stays on OpenRouter.
