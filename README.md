# OpenTalk

**Voice dictation for Windows, in one small native executable.** Press a hotkey in any app, speak, press it again,
and clean text is typed where your cursor is.

- **Light:** a single ~4 MB portable `.exe` with no installer, no runtime and no web view. It uses close to 0% CPU when idle.
- **Cloud or local:** bring your own key for a cloud provider (OpenAI, Groq, Deepgram, Gemini and more), or download
  one of 15 local models (Parakeet, Whisper, Qwen3-ASR, Moonshine and more) that run on your PC with no key, so audio
  never leaves the machine. There is no account and no telemetry.
- **Clean output:** filler words ("um", "you know"), stutters and transcription glitches are removed locally in a few
  milliseconds. Optional AI cleanup and a spoken *"rewrite this concisely"* command run on an AI model of your choice.
- **Pleasant to use:** a floating glass pill shows a live waveform while you talk, and everything else lives in a
  compact tray popup.

OpenTalk is a Rust re-implementation of [OpenWhispr](https://github.com/OpenWhispr/openwhispr)'s dictation experience,
focused on a tiny footprint. It runs on Windows 10 and Windows 11, x64.

## Install

1. Download `opentalk.exe` from the [latest release](https://github.com/ErsatzHitman/opentalk/releases/latest).
2. Put it anywhere you like (for example `C:\Tools\OpenTalk\`) and double-click it.

That's it: there is no installer, and nothing else is needed.

- **SmartScreen warning:** the exe is not code-signed, so Windows may warn the first time. Click **More info → Run anyway**.
- **Checking the download:** each release lists the file's SHA-256 next to it in `opentalk.exe.sha256`:
  `Get-FileHash .\opentalk.exe -Algorithm SHA256`.
- **Start with Windows:** turn on **Settings → Preferences → Launch at login**.
- **Uninstall:** quit OpenTalk from its tray menu, delete the exe, and optionally delete `%APPDATA%\OpenTalk`, which
  holds your settings, history and encrypted keys.

## Set up (one minute)

On first launch OpenTalk opens **Settings → Speech-to-Text** in the tray popup. Choose one option:

- **Bring your own key** (cloud):
  1. **Pick a provider:** Groq, OpenAI, Deepgram, Gemini, Mistral, xAI, AssemblyAI, Corti, Tinfoil, OpenRouter, or a
     custom / Azure OpenAI endpoint.
  2. **Paste your API key** and press **Save**. Keys are encrypted with Windows DPAPI for your user account.
  3. **Press ▶ Test** to check the key and model.
- **Use a local model** (on this PC, no key):
  1. **Tap a model** to download it once. Badges show which to pick: **Best overall** (Parakeet TDT 0.6B v3),
     **Most accurate**, **Low-end PCs**, **Fastest** and others. See [docs/LOCAL_MODELS.md](docs/LOCAL_MODELS.md) for
     the evidence behind each.
  2. **Wait for the download.** OpenTalk switches to the model when the download finishes, and the first one also
     fetches the 27 MB engine.
  3. **Press ▶ Test** on the same page to check it.

Optional, under **Settings → Cleanup & AI**:

- **AI cleanup:** polishes every dictation (grammar, punctuation) with an AI model. If the AI call fails, your
  transcript is pasted anyway and History marks it *STT fallback*.
- **AI rewrite:** end a dictation with *"rewrite this concisely"* (or a phrase you choose) to paste a shorter version.
- **AI provider:** both features share one provider and key (OpenAI, Anthropic, Gemini, Groq, OpenRouter, Azure
  OpenAI, Bedrock, Vertex or any OpenAI-compatible endpoint). Each feature picks its own model, and each switches
  on or off independently.
- **On-device cleanup:** filler-word removal (Off / Standard / Aggressive), fuzzy dictionary fixes and spoken
  "new line" / "new paragraph". This runs locally and is always available.

## Use it

| Do this | To |
|---|---|
| **Ctrl + Win** (default hotkey) | Start dictating; press again to stop and paste |
| Hold the hotkey (with *Push to talk*) | Record while held; release to paste |
| **Esc**, or the pill's **✕** | Cancel the current dictation |
| Click the pill / right-click it | Start or stop / hide it for now |
| Click the **tray icon** | Open the popup: status, History, Dictionary, Settings, recent dictations |
| End with *"…rewrite this concisely"* | Paste a concise AI rewrite instead of what you said |
| *"…rewrite this concisely and make it formal"* | Rewrite following your extra instructions |

**While you record,** the pill shows the app the text will go to and a live waveform. The waveform follows your voice
and breathes gently when you're silent, so you can always tell the microphone is live. When you stop, the pill glows
while the text is transcribed, then the text is pasted into that app. Your clipboard is restored afterwards, and the
dictated text never appears in Win + V clipboard history.

**If nothing can take text** (the desktop, a button, a web page with no field selected), the text is not pasted
into nowhere: it is copied to the clipboard and a small **Copied** bubble pops out of the pill. Press Ctrl + V where
you want it.

**If a transcription fails** (network, quota), the pill's error card offers **Retry** with the audio kept in memory.

**History** (tray popup) lists every dictation with its app icon, and is searchable. Open an entry to copy the final
text, the raw transcript or the text before a rewrite. **Dictionary** holds words that improve recognition and fix
casing. **Snippets** expand a spoken trigger into longer text.

### The rewrite phrase

Say it after your message, for example: *"Let's move the meeting to Friday afternoon because the
client is out on Thursday, rewrite this concisely."*

- **Extra instructions:** keep talking after the phrase and those words go to the AI model as extra instructions,
  for example *"… rewrite this concisely and translate it to Spanish"* or *"… rewrite this concisely as bullet
  points"*. Only the text before the phrase is rewritten; the phrase and the instructions are never pasted. This
  works with any phrase you set.
- A trailing "please" or "thanks" is fine. Case, punctuation and "re-write" spellings don't matter.
- Only the text before the phrase is sent, as text and never audio. Bodies of three words or fewer are pasted as spoken.
- If the AI call fails, your text is pasted as dictated, without the phrase, and a card tells you why.
- You can change the phrase or switch AI rewrite off in **Settings → Cleanup & AI**.

## Configure

Everything is in the tray popup under **Settings**, and changes apply immediately:

| Page | Options |
|---|---|
| Preferences | Theme (light / dark / system), sounds, pause media, mute system audio while dictating, auto-paste, keep text on the clipboard, floating pill (auto-hide, position, app bubble), language, microphone, launch at login |
| Hotkeys | Dictation hotkey (modifier-only combos such as Ctrl + Win allowed), Tap or Push to talk, Start-menu suppression |
| Speech-to-Text | **Bring your own key**: provider, model, API key, custom endpoint, streaming, test. **Use a local model**: download, use, test or delete any of the 15 local models |
| Cleanup & AI | AI rewrite and AI cleanup, each with its own model (and cleanup's editable prompt), the AI provider they share, on-device cleanup |
| Privacy & Data | History on/off and retention, keep failed audio for Retry, clear history |
| About | Version, data and log folders, reset app data |

Settings are saved to `%APPDATA%\OpenTalk\config.json`. Running `opentalk.exe --settings` opens the popup, and so does
launching the exe a second time.

## Privacy

- **Audio:** OpenTalk records **only** between start and stop. With a local model it never leaves your PC.
  With a cloud provider it goes over HTTPS / WSS to the provider you picked, nowhere else. Audio is never written to disk. After a failed transcription it
  stays in memory for Retry until the next dictation or until you quit.
- **Text:** your transcribed text, never audio, goes to your AI model only when AI cleanup is on or when you say the
  rewrite phrase.
- **Keys:** API keys are encrypted with Windows DPAPI in `%APPDATA%\OpenTalk\secrets.json` and are never logged.
- **History:** history is a local file (`history.jsonl`) that you can turn off, auto-prune or clear.
- **No telemetry:** there is no telemetry, no analytics and no account. Local model downloads come from the
  sherpa-onnx releases (GitHub), PyPI, npm and Hugging Face.
- **Screen capture:** the pill is hidden from screenshots and screen sharing. Its glass effect needs a clean copy of
  the screen behind it.

## Troubleshooting

- **Nothing is pasted into an app running as administrator.** Windows blocks this. OpenTalk tells you, and the text is
  left on the clipboard.
- **Terminals:** OpenTalk pastes with Ctrl + Shift + V where that is the convention.
- **The hotkey opens the Start menu.** Keep **Settings → Hotkeys → Start-menu suppression** on for Win combos.
- **Logs:** they are in the folder shown on **Settings → About**. Set `OPENTALK_LOG=debug` for more detail.

## Build from source

You need Rust (stable) with the `x86_64-pc-windows-gnu` target and mingw-w64. MSVC is not needed.

```powershell
rustup target add x86_64-pc-windows-gnu
cargo build --release --target x86_64-pc-windows-gnu   # → target\x86_64-pc-windows-gnu\release\opentalk.exe
cargo test  --target x86_64-pc-windows-gnu
```

The whole build and most tests also run on Linux, cross-compiled and executed under Wine. Every push to `main` and
every `v*` tag is built by [GitHub Actions](.github/workflows/release.yml) and published to Releases.
See **[docs/DEVELOPMENT.md](docs/DEVELOPMENT.md)** for the Linux setup, desktop and live tests, the command-line tools,
the source layout and the benchmark. The design spec is in [docs/SPEC.md](docs/SPEC.md).

## Contributing

Issues and pull requests are welcome. Before opening a PR, please:

- run `cargo test` and `cargo clippy --all-targets`;
- update the relevant goldens or snapshots if the UI changes on purpose (see docs/DEVELOPMENT.md);
- never commit API keys. Live tests read them from the environment (`OPENTALK_SECRET_<PROVIDER>`), and `.env` is
  git-ignored.

To report a security issue privately, see [SECURITY.md](SECURITY.md).

## License

[MIT](LICENSE) © ErsatzHitman and the OpenTalk contributors.

OpenTalk re-implements behaviour and design from [OpenWhispr](https://github.com/OpenWhispr/openwhispr) (MIT,
© 2024 OpenWhispr Team). Its licence notice is reproduced in [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md).
