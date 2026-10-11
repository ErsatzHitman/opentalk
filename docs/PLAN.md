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
| Settings UI RAM | Only while the tray popup is open: one layered window on the daemon's UI thread (the egui Settings process and its ~265 MB GL driver commit are gone since round 3). A closed popup holds no window, bitmap or timer; an open page adds its card bitmap (about 1 MB at 1x, 4 MB at 2x) and the page models, freed on close. Counts toward the daemon's budget above | `bench.ps1` daemon WS / private bytes with a page open |
| Disk | Single exe ≤8 MB, where MB is decimal: **8,000,000 bytes** (7.63 MiB). The release exe measured 7,248,896 bytes (7.25 MB) on 2026-10-10 after the Settings/network crates moved to `opt-level = "s"` (`Cargo.toml`), about 751 KB of headroom; with the liquid-glass Settings pages and pill merged it measured 7,505,920 bytes (cross-built on Linux, 2026-10-10), about 494 KB of headroom; after the flat Settings window, tray popup, live app bubble, mute, detached cancel button and paced animation were merged it measured 7,578,624 bytes (cross-built on Linux, 2026-10-10), about 421 KB of headroom; after round 3 removed the egui Settings window (eframe, egui, winit, glow) it measured 4,177,408 bytes (cross-built on Linux, 2026-10-10), about 3.8 MB of headroom (it moves with every commit; `bench.ps1` has the current figure). No installer runtime | `bench.ps1` checks the file size against 8,000,000 bytes; history and config are KB-scale |
| Hotkey to pill visible | ≤16 ms | Timestamped debug log (QPC) |
| Hotkey to mic capturing | ≤50 ms | Log: hook event → first WASAPI packet |
| Stop to text pasted | Network RTT + ≤40 ms local overhead | Log: stop → request sent → response → paste done |
| Upload size | Silence-trimmed 16 kHz mono PCM WAV (FLAC measured and not adopted, see §9.5) | Bytes logged per request |

---

## 2. Architecture

```
opentalk.exe            (daemon, default: Win32 only, no GPU, no WebView)
 ├─ main thread         hidden message window, tray, pill window, state machine
 ├─ hook thread         WH_KEYBOARD_LL → PostMessage(WM_APP_*)
 ├─ capture thread      WASAPI event-driven, started on hotkey only
 ├─ anim thread         vblank-timed (one-shot waitable timer to just after the next vblank) at full rate, periodic timer for the slow cadence and as fallback; posts WM_APP_TICK only while animating
 └─ worker thread(s)    HTTP: prewarm, transcribe, cleanup

opentalk.exe --settings [page]
                        (not a window of its own: posts OpenPage to the running daemon, or starts the daemon at that page)
```

- **One process, one UI thread.** Settings, History, Dictionary and Snippets are pages of the tray popup, a layered window on the daemon's UI thread drawn with the pill's software rasteriser and GDI text (no GPU, no egui; round 3 removed the separate egui/OpenGL Settings process).
  - `%APPDATA%\OpenTalk\config.json` holds settings. Secrets are DPAPI-encrypted with `CryptProtectData`, matching OpenWhispr's safeStorage.
  - `history.jsonl` is append-only.
  - A page edits a working copy of the config; its changes reach the daemon through an in-process mailbox (`WM_TRAY_POPUP_ACT`), which applies them live (`apply_config`) and saves the file.
- **Single instance.** Named mutex `Local\OpenTalk.Daemon`. A second launch (or `--settings [page]`) posts `WM_OT_OPEN_PAGE` to the daemon window (`FindWindowW("OpenTalkDaemon")`), which opens the popup, then exits. A login launch (`--hidden`) opens nothing; the first run opens Settings > Speech-to-Text; any other interactive launch opens the Home popup.

### Crates (all pure Rust, so they build with the GNU toolchain and need no C compiler)

| Need | Crate | Note |
|---|---|---|
| Win32 / COM / WASAPI / DPAPI | `windows` 0.62 | Talk to WASAPI directly instead of through cpal. `AUTOCONVERTPCM` gives 16 kHz mono i16 from the OS resampler, so we write no resampler. |
| Pill rendering | `tiny-skia` | Rendered into a premultiplied BGRA DIB, then `UpdateLayeredWindow` |
| Pill text (tooltip, error card) | GDI `DrawTextW` into a mask DIB | Native Segoe UI, no font crate |
| HTTP | `ureq` 2 + `native-tls` (schannel) | Persistent agent, keep-alive, own User-Agent (Cloudflare blocks default UAs) |
| WebSocket (streaming providers) | `tungstenite` + native-tls | Phase 6 only |
| JSON | `serde`, `serde_json` | |
| Settings UI | none (tray popup pages: `tiny-skia` + GDI text, custom text editor) | Same rasteriser and fonts as the pill |
| Resources | `embed-resource` (windres from mingw) | Icon plus PerMonitorV2 DPI manifest |

---

## 3. Module layout

```
src/main.rs            arg dispatch (daemon | --settings | --bench | --transcribe-file | --render-pill | --render-tray-popup)
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
src/daemon/pill/glass.rs, capture.rs   liquid-glass surface material of the pill (frosted, refracted screen capture; SPEC "Pill surface material")
src/daemon/appicon.rs  foreground exe → icon (UWP aware), cache
src/daemon/paste.rs    clipboard snapshot/restore, SendInput, terminal detection
src/daemon/tray.rs     Shell_NotifyIcon; tray/popup*.rs: the flat tray popup (model + placement, software rendering, window)
src/daemon/sound.rs    synthesized cues → in-memory WAV → PlaySound
src/daemon/tray/page/… popup pages: renderer (layout, draw, controls, text editor, scroll, session, pager) and page models (settings/*, history, dictionary); flat solid surfaces, no glass
src/core/{nav,settings_logic,dictview,histview,timefmt}.rs   pure page logic: routes, choice tables, provider tests, view helpers
`--bench mock-stt`      in-repo mock STT server (Rust, replaces the planned tools/mock_stt.py); per-provider wire-format mocks are in-process test servers
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
  - The capture thread runs a streaming meter (DC removal, 20 ms power every 10 ms, adaptive noise floor, gate with hysteresis) and publishes a 0..1 display level through an atomic, plus the peak since the pill last looked. A silent room publishes exactly 0.0.
  - Bar height is `4 + level·18`.
  - The bars are a delay line over one continuous envelope (12 ms attack, 120 ms release): the newest bar is the live envelope every frame, older bars scroll right to left (80 ms per slot, interpolated). Silence is a flat 4 px line and costs no frames.
- **Signal glow.** tiny-skia has no conic gradients or blur, so it is drawn by hand:
  - Ring: computed analytically per pixel (about 2.5k pixels).
  - Halo and comet: pre-blurred once at start-up into 64 px sprites. Each frame rotates the comet and scales and breathes the halo.
  - Timing: spin period 1.6 s, breathe period 1.9 s.
- **Logo.** Ring of r=10 with stroke 2, plus three bars, drawn as tiny-skia paths at 22 px.
- **Theme.** Follows the Windows app theme (`AppsUseLightTheme`), with the light and dark tokens from SPEC.
- **Rendering cost.** Rendering happens only while animating (recording or thinking) at 60 fps, using a cached background. Idle does no work.
- **Frame pacing.** At full rate the anim thread reads the compositor's vblank clock (`DwmGetCompositionTimingInfo`, non-blocking) and sleeps on a one-shot high-resolution waitable timer until just after the next vblank, instead of a free-running 16 ms timer that beats against the display's refresh. It ticks at every vblank up to 100 Hz and every second to fourth one above (144 Hz runs at 72 fps), and tells the window when the frame will be shown; the animation is sampled at that time, so a late tick cannot slow it. It never waits *for* the compositor (`DwmFlush`): DWM does not compose an unchanged desktop, so a flush blocks until something else on screen moves, which froze the waveform of a silent pill (and kept it deaf to the wake on speech) until the pointer moved over it. Without timing info (remote session, Wine) it drops back to the fixed timer. The slow cadence (a silent recording) and the idle pill are unchanged: no timer while idle.
- **Transition frames.** The expand and shrink change the capsule's size every frame, so nothing cached by size can help: the refraction field is evaluated per pixel by table lookup (depth profile, per-column / per-row corner factor; within 0.1 px of the transcribed formula), the shadow uses a CDF table, nothing is baked or stored, and the backdrop capture is held back until the motion ends. A shape seen twice in a row (a steady pill) is baked as before.
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
- **Timing.** The icon is resolved ahead of time, when an app comes to the front (one foreground WinEvent hook + one idle-blocked low-priority worker + a 16-entry LRU), so the bubble is there with the pill at the hotkey press. It follows the foreground app during recording and thinking (180 ms cross-fade), and the paste goes to the app it shows when the text is ready; OpenTalk's own pill/tray windows and transient shell surfaces keep the previous target (SPEC "App bubble"). It fades in with the expansion and fades out after the paste.
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
- **Voice rewrite.** A dictation that ends with the configured phrase ("rewrite this concisely") is detected on the deterministic text (`core::rewrite`), its body is sent to the cleanup provider with a rewrite prompt and validator (`llm::rewrite`, 30 s, no retries) and the answer replaces the cleanup call for that dictation; any failure pastes the body as dictated with an informational card. Adds one LLM round trip only on trigger dictations; recording CPU, idle CPU and RAM budgets are unaffected (all work happens after stop). See docs/SPEC.md "Voice rewrite".

---

## 7. Settings, History and Dictionary

> **Round 3 (owner decision: "Remove the massive application window entirely"):** the egui window described below is gone. Every feature is a 380 px page of the tray popup with back navigation (Settings index and its six pages, list pickers instead of dropdowns, History with a detail page, Dictionary, Snippets and the snippet editor), the hotkey is captured by the daemon's own hook, and the Test buttons run on a worker. Layout and tokens: docs/SPEC.md "Tray popup". The section is kept for the feature inventory, which the pages still cover.

OpenWhispr already has History (its Home page) and a Dictionary page with Snippets. We keep both as dedicated pages, plus the Settings modal. (Originally built in egui with OpenWhispr's tokens.)

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
  - **Preferences:** Theme (Light/Dark/Auto), Dictation sounds, Pause media, Mute system audio, Automatic pasting, Keep transcription in clipboard, Floating icon (auto-hide, start position), Transcription language, Launch at login, Start minimized, Microphone (device and keep-warm).
  - **Hotkeys:** Dictation hotkey (default `Ctrl+Win`, modifier-only combos allowed), Tap/Hold, cancel key.
  - **Speech-to-Text:** provider pill tabs, API key field with a "Get key →" link and masked display, model card list, custom endpoint fields, and a Test button (new) that sends a 1 s TTS sample.
  - **Cleanup:** filler mode, "Enable AI cleanup", LLM provider/model/key, Prompt Studio (view, edit and reset the prompt).
  - **Privacy & Data:** history on/off, transcript retention, audio retention (default off, to save disk), Save discarded dictations.
  - **System:** version, open logs folder, reset app data.
  - Account, billing, notes, meetings, agent, connectors and local models are dropped.
- **Hotkey capture.** The daemon's own LL hook switches to a capture mode while a hotkey field is active (allocation-free, swallows the keys, times out after 2 minutes); the popup ends or re-arms it.

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
  - Static icon (`NOTIFYICON_VERSION_4`, so Enter / Space on the icon arrive as `NIN_KEYSELECT` / `NIN_SELECT`, also from the hidden overflow).
  - Any activation (left click, right click, keyboard) toggles a small flat popup instead of a classic menu: no egui, no GPU, no timers.
    The window is created when the icon is activated and destroyed when it closes (layered, topmost, tool window, activated so it takes
    the arrow / Enter / Esc keys; it closes when it loses activation). It is rendered with the pill's software rasteriser and GDI text.
    It sits at the bottom-right next to the tray: anchored on `Shell_NotifyIconGetRect` when the icon is visible, else in the corner
    of the work area of the taskbar's edge (any of the four; auto-hidden bars via `ABM_GETTASKBARPOS`), per monitor and per-monitor DPI.
  - Rows: status header (Ready / Recording / Transcribing), Start / Stop dictation with the hotkey, History, Dictionary, Settings,
    the last two dictations (click to copy), Show / Hide floating pill, Retry last dictation (only after a failure), Quit.
- **Autostart.** HKCU `Run` value `OpenTalk` = `"<exe>" --hidden`. The state reads `StartupApproved\Run` too, mirroring OpenWhispr's `executableWillLaunchAtLogin` lesson.
- **Sounds.** OpenWhispr cues synthesised once:
  - Start: C5→E5. Stop: D5→A4.
  - Each note 90 ms with a 25 ms gap; gain 0.2.
  - Played with `PlaySoundW(SND_MEMORY|SND_ASYNC)`.
- **Mute system audio.** `src/daemon/muteaudio.rs`.
  - One lazily started worker thread (MTA) owns a pure state machine; the UI thread only sends `Begin`/`End` messages. The default playback endpoint comes from the shared `IMMDeviceEnumerator` (`eRender`/`eConsole`), `IAudioEndpointVolume::SetMute` does the work. No timer while idle; the only wait is a `recv_timeout` for the length of the start cue (about 0.22 s + 60 ms) during a dictation.
  - Restore rules: only mute what was not muted, restore that device id, leave it alone when the user already unmuted it. The unmute runs before the stop cue and before media resumes; the UI thread waits at most 100 ms (800 ms on quit).
  - Crash safety: `muted-device.txt` (device id) is written before the mute and removed after the restore; at start-up, if it exists and the device is still muted, it is unmuted.

---

## 9. Latency strategy

1. **Show the pill first.** The hook posts to the main thread, which repaints the cached idle frame. Target under 16 ms.
2. **Pre-built capture.** Keep `IMMDeviceEnumerator` and the device resolved at start-up, and re-resolve on `IMMNotificationClient` default-device change. The `IAudioClient` is created at the hotkey press, which is about 10–20 ms. An optional "keep mic warm" setting is offered, mirroring `micWarmHoldSeconds`, default off, because Windows shows the mic indicator while it is warm.
3. **TLS pre-warm at hotkey press.** A background `HEAD`/`GET` opens the TLS connection to the provider host while you speak, so the upload reuses a warm keep-alive socket. That saves about 50–150 ms.
4. **Trim leading and trailing silence** (energy VAD with 150 ms padding) before upload. Smaller body, fewer hallucinations.
5. **Upload format.** 16 kHz mono PCM WAV. **Decision (Phase 8 experiment, 2026-10-10): keep WAV, do not adopt FLAC.** `flacenc` 0.5.1 (pure Rust) shrinks the upload by 37-57% (a clean TTS clip 271,244 -> 128,297 bytes; the same clip with a mic-like noise floor -> 158,109 bytes) and encodes in about 1.8-1.9 ms per second of audio (~15 ms for 8.5 s). Net of that encode time, the saving is below the 30 ms bar at a 20 Mbps uplink for a typical 5 s dictation (17-25 ms) and falls to 1-8 ms at 50 Mbps; it only clears 30 ms below roughly 13 Mbps (5 s) or 20 Mbps (8.5 s). The price is +248,320 bytes (about 242 KiB) in the executable: 3.2% of the 7,877,632-byte build measured then (7.51 MiB), and more than twice the 122,368 bytes that were left under the 8,000,000-byte budget (§1), so it would not have fit, a per-provider format-acceptance matrix, and extra CPU on the hot path. Full numbers: `BENCHMARK.md`, section "FLAC upload experiment". Revisit only as an opt-in for slow uplinks (below about 10 Mbps), not as the default.
6. **Streaming providers** (Deepgram, AssemblyAI, Realtime) send audio during recording, so stop-to-text is roughly the finalize time (about 150–300 ms).
7. **Local post-processing:** deterministic cleanup under 1 ms. Paste is about 10 ms delay plus `SendInput`.
8. **History write** happens after the paste, on the worker thread.

---

## 10. Testing (OpenRouter only, free models only)

| Layer | Method |
|---|---|
| Unit | `cargo test`: cleanup pipeline (all ported OpenWhispr fixtures plus new ones), snippets, dictionary prompt caps, echo filter, hotkey parser and matcher, provider request builders (golden multipart bodies), response parsers, error classifier, config migration. |
| Provider contracts | In-process Rust mock servers (`std::net::TcpListener`, plus `opentalk --bench mock-stt` for the e2e and bench runs; they replace the planned `tools/mock_stt.py`) replay each provider's documented request and response shape (OpenAI SSE, xAI, Mistral, Gemini JSON, Deepgram REST and WS, AssemblyAI WS, Corti flow). Every provider's request is asserted byte-for-byte against OpenWhispr's format. No real keys are needed. |
| Live STT | **Best effort, reported honestly; not an exit gate.** OpenRouter lists exactly three `:free` models with audio input (re-checked 2026-10-10): `thinkingmachines/inkling-small:free` and `thinkingmachines/inkling:free` answer HTTP 403 "only available on agentic harnesses", and `nvidia/nemotron-3-nano-omni-30b-a3b-reasoning:free` reports `usage.prompt_tokens` = 28 with or without a 6 s WAV attached, for both the `input_audio` and the `audio_url` content shapes: the free endpoint drops the audio before the model, so the request shape is not the cause and live STT cannot be verified under the free-only policy. Live STT is therefore **UNVERIFIED** and STT correctness rests on the mock contract tests (measurements: `BENCHMARK.md`, "Live STT on the free tier (evidence)"). The `live_*` tests print `VERIFIED` (a transcript with the spoken words), `UNVERIFIED` (limits, gated model, a "No audio detected" error or a reply without the spoken words, reported as "free endpoint dropped the audio (prompt_tokens=28 …)") or `FAILED` (bad key or request, always a failure). `OPENTALK_LIVE_REQUIRE=1` turns `UNVERIFIED` into a failure, for runs that have a model able to verify. Only 429, 5xx and timeouts are retried with backoff; a gated or audio-dropping model gets one attempt (`tests/common/live.rs`). |
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
| 7 | Settings UI | Tray popup pages (round 3; the egui shell is removed), History, Dictionary/Snippets, Settings, hotkey capture, provider Test button | Every setting persists and the daemon applies it live |
| 8 | Hardening and bench | Enterprise (Bedrock/Vertex), FLAC experiment (done: not adopted, §9.5), DPI and multi-monitor, UIPI, bench report, polish | `BENCHMARK.md` meets the §1 targets |

---

## 12a. Risks

- **Popup fidelity.** The pages are custom-drawn (no toolkit): text fields, IME placement, focus and DPI are ours to get right and need a real Windows desktop to confirm.
- **No real keys for most providers.** Correctness rests on the contract tests against OpenWhispr's exact wire formats. Live verification is limited to OpenRouter, and its free tier currently cannot verify STT at all (see §12b decision 0).
- **OpenRouter free tier is rate-limited** (Gemma returned 429). Live tests retry with backoff and rotate across free models.
- **Start-menu mask key** can interact with other hook tools such as AutoHotkey. It is behind a setting, default on.

## 12b. Open decisions (for the user)

0. **Live STT verification (provisional default, owner may override).** No free OpenRouter model delivers audio to its model (evidence, 2026-10-10: both inkling models answer 403 "only available on agentic harnesses"; `nvidia/nemotron-3-nano-omni-30b-a3b-reasoning:free` reports `prompt_tokens` = 28 with or without the WAV, for `input_audio` and `audio_url`; see §10 and `BENCHMARK.md`), so a live STT transcript is unobtainable at zero cost. Options were: (a) allow one paid OpenRouter audio model or another provider key for a single live run, or (b) accept mock-only STT verification. **Recorded default: (b), pending the owner's confirmation (nothing paid is spent without it); the owner's decision itself is still open.** §10 and the Phase 2 exit check are amended accordingly: Phase 2 passes on the mock contract tests; the live tests stay in the suite and report `UNVERIFIED` rather than green. The strict policy (OpenRouter, `:free` only) is unchanged. If the owner later supplies a paid key and approves one run, that run is made with `OPENTALK_LIVE_REQUIRE=1` and a model override, the `:free` assertion in the live tests is relaxed for that run only, and this entry is updated to record the verified model and date.

1. **Tinfoil.** OpenWhispr uses an attested enclave client. Choose one: (a) plain HTTPS without attestation, (b) skip Tinfoil, or (c) port attestation later. Recommendation: (b), unless you use it.
2. **Corti and enterprise clouds** (Bedrock SigV4, Vertex). Keep in Phase 6/8, or drop for a personal build?
3. **Default STT provider for your daily use**, once a production key exists. Testing stays on OpenRouter.
