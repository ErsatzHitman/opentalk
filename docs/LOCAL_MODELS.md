# Local speech-to-text models

OpenTalk can transcribe on your PC instead of a cloud provider: **Settings → Speech-to-Text → Use a local model**.
There is no API key, and the audio never leaves the machine. A model is downloaded once. It then runs in a
background process, kept loaded so dictations don't wait for it.

## How it works

- **Runtime:** the open-source [sherpa-onnx](https://github.com/k2-fsa/sherpa-onnx) engine (Apache-2.0),
  version 1.13.8.
  - **First download:** the engine is fetched once (about 27 MB), pinned by SHA-256. The executables come from the
    `sherpa-onnx-bin` PyPI wheel and `onnxruntime.dll` from the `sherpa-onnx-win-x64` npm package, both of the same
    release.
  - **No Visual C++ runtime needed:** the C runtime is linked statically.
  - **`onnxruntime.dll` renamed:** it is installed as `ot-onnxrt.dll`, and the executables' import tables are patched
    to load it. This avoids the older `onnxruntime.dll` that Windows 11 ships in System32, a fix taken from
    OpenWhispr.
- **Models:** the official int8 exports of the sherpa-onnx `asr-models` release.
  - Qwen3-ASR 1.7B is the exception: it is a community export on Hugging Face, because GitHub cannot host its files.
  - Archives are unpacked with Windows' own `tar.exe`, and only the files the model uses are kept.
- **Inference:** sherpa-onnx's websocket server on `127.0.0.1`, started for the chosen model and kept warm.
  - The server is in a kill-on-close job object, so it ends with OpenTalk.
  - Long dictations are cut at pauses: 28 s pieces for Whisper, 120 s for Parakeet and Qwen.
  - Nemotron streams while you speak.
- **Files:** everything lives under `%APPDATA%\OpenTalk\local`. Delete a model from its row (the × on hover).
- **Mirror:** `OPENTALK_LOCAL_MIRROR=https://your.mirror/folder` downloads the same file names from a mirror, for
  offline or managed installs.

## The tags, and why each model got its tag

English accuracy is the average word error rate (WER) on the
[Hugging Face Open ASR Leaderboard](https://huggingface.co/spaces/hf-audio/open-asr-leaderboard) (lower is better).
Speed is the leaderboard's RTFx (seconds of audio per second of compute, higher is faster) plus CPU measurements of
the int8 exports. Each recommendation goes to exactly one model.

| Tag | Model | Why |
|---|---|---|
| **Best overall** | NVIDIA Parakeet TDT 0.6B v3 | 6.32 % average English WER, within 0.6 points of the most accurate model here, at RTFx 3,333. On a CPU the int8 export ran ~30× real time on a desktop and stayed ahead of Whisper Small int8 on a 2-vCPU box (RTF 0.099 vs 0.160). It covers 25 European languages with automatic detection, in a 465 MB download. Best balance of accuracy, speed and size. |
| **Most accurate** | Qwen3-ASR 1.7B | Lowest English WER of the 15 (5.76 %; 5.59 % in a later snapshot). Best multilingual FLEURS score, 4.90 vs Whisper Large-v3's 5.27. It covers 52 languages and dialects. The cost: a 2.3 GB download, the slowest decode here (an LLM-style decoder) and a community package. |
| **Most languages** | Whisper Large-v3 Turbo | All 99 Whisper languages at ~7.75 % WER, only ~0.3 points behind Large-v3, but faster (RTFx ~200 vs 146) and half the download (538 MB vs 1.0 GB). |
| **Low-end PCs** | Whisper Small | The most accurate multilingual model under ~600 MB. Its int8 build still runs faster than real time on a 2-core CPU (RTF ~0.16). |
| **Best English-only** | Distil-Whisper Small.en | Lowest WER of the English-only models (8.57 %), ahead of Distil-Whisper Medium.en (8.77 %) at under half its size. |
| **Low-end · English** | Moonshine Base | 9.99 % WER, better than Whisper Base.en (10.32 %), in a 239 MB download that runs ~566× real time. |
| **Fastest** | Moonshine Tiny | Highest throughput here (RTFx 753) and the smallest download (103 MB), at about Whisper Tiny.en's accuracy (12.65 vs 12.81 %). For very old hardware. |
| **Streaming** | Nemotron-3.5-ASR-Streaming 0.6B | The only streaming model. It transcribes while you speak (560 ms chunks), so the text is ready the moment you stop. Multilingual, with automatic detection. It isn't ranked on the leaderboard yet. |
| *Community build* | Qwen3-ASR 1.7B | A fact, not a recommendation: not packaged by sherpa-onnx itself. |

**Untagged:**
- **Whisper Large-v3** is beaten by Turbo on speed for ~0.3 points of accuracy.
- **Distil-Whisper Large-v3 and Medium.en** are beaten by Parakeet on English.
- **Qwen3-ASR 0.6B** has a strong 6.31 % English WER but is 900 MB with an autoregressive decoder, so Parakeet is
  faster at the same English accuracy. It is still the pick for Chinese and Asian languages on a mid-range PC.
- **Whisper Medium, Base and Tiny** are dominated by Turbo, Small and Moonshine.

## Other models worth adding

All of these are packaged by sherpa-onnx and would run on the same engine:

- **Parakeet Unified EN 0.6B (add):** 5.91 % average English WER in a 631 MB int8 download, close to Qwen3-ASR
  1.7B's accuracy at Parakeet speed. It would take "Best English" outright.
- **Cohere Transcribe 2B (add, optional):** the most accurate model sherpa-onnx packages (5.42 % average WER),
  covering 14 languages. 2.7 GB, and the language must be set (no detection).
- **SenseVoice Small (add):** tiny and very fast, with excellent Chinese, Cantonese, Japanese and Korean. Fills the
  Asian-language gap for low-end PCs that the Whisper models cover poorly.
- **Canary 180M Flash (add):** 180M parameters for English, Spanish, German and French. A low-end pick for the
  big European languages.
- **Distil-Whisper Large-v3.5 (replace Large-v3):** 7.21 % WER vs 7.44 % for Whisper Large-v3. sherpa-onnx exports
  it, so it can replace Distil-Whisper Large-v3 (English-only, same family).
- **Nemotron Speech Streaming EN 0.6B (optional):** an English-only streaming sibling (~7.1 % WER), for
  English-only users who want streaming.
- **Consider dropping Whisper Medium and Whisper Tiny:** Turbo beats Medium on every axis, and Moonshine Tiny
  (English) or Whisper Base (multilingual) cover Tiny's niche. Keep them only for familiarity.

## Sources

- Open ASR Leaderboard and paper: <https://huggingface.co/spaces/hf-audio/open-asr-leaderboard>,
  <https://arxiv.org/abs/2510.06961>.
- Qwen3-ASR technical report and README (FLEURS, languages): <https://arxiv.org/abs/2601.21337>,
  <https://github.com/QwenLM/Qwen3-ASR>.
- Parakeet TDT 0.6B v3 / Canary paper: <https://arxiv.org/abs/2509.14128>. CPU measurements:
  <https://vidclean.net/blog/parakeet-vs-whisper-cpu-transcription>,
  <https://github.com/mil-ad/parakeet-tdt-0.6b-v3-fastapi-openai>.
- Moonshine: <https://arxiv.org/abs/2410.15608>, <https://arxiv.org/abs/2602.12241>.
- Distil-Whisper: <https://github.com/huggingface/distil-whisper>, <https://speechtotext.dev/model/distil-whisper/>.
- Nemotron Speech / 3.5 ASR streaming: <https://huggingface.co/nvidia/nemotron-speech-streaming-en-0.6b>,
  <https://huggingface.co/blog/nvidia/fine-tuning-nemotron-35-asr>.
- sherpa-onnx packages and docs: <https://github.com/k2-fsa/sherpa-onnx/releases/tag/asr-models>,
  <https://k2-fsa.github.io/sherpa/onnx/qwen3-asr/index.html>. Qwen3-ASR 1.7B status:
  <https://github.com/k2-fsa/sherpa-onnx/issues/3535>. Community package:
  <https://huggingface.co/solavr/sherpa-onnx-qwen3-asr-1.7B-int8>.
- OpenWhispr's model registry (sizes, Parakeet Unified and Cohere WER):
  <https://github.com/OpenWhispr/openwhispr/blob/main/src/models/modelRegistryData.json>.

Numbers come from different snapshots and hardware; treat them as rankings, not guarantees.

## Model licences

The models are downloaded by you from their publishers and are not part of OpenTalk:
- **Whisper and Distil-Whisper:** MIT.
- **Moonshine v1:** MIT.
- **Qwen3-ASR:** Apache-2.0.
- **Parakeet TDT v3:** CC-BY-4.0.
- **Nemotron 3.5 ASR:** NVIDIA's open model licence.
