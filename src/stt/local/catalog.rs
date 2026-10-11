//! The local speech-to-text models OpenTalk can download and run with the sherpa-onnx runtime.
//!
//! Every model is a sherpa-onnx export: the official int8 archives of the `asr-models` release of
//! k2-fsa/sherpa-onnx, except Qwen3-ASR 1.7B, whose only sherpa-onnx package is a community export
//! on Hugging Face (the official release cannot host files over 2 GB).
//!
//! Tags answer "which one should I pick?" and each rests on published numbers (sources in
//! docs/LOCAL_MODELS.md): English WER is the Hugging Face Open ASR Leaderboard average, speed is
//! the leaderboard's RTFx plus CPU measurements of the int8 exports.

/// How the runtime loads a model (which sherpa-onnx flags, which server).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Whisper encoder/decoder (`--whisper-*`); 30 s windows.
    Whisper,
    /// Moonshine v1 (`--moonshine-*`).
    Moonshine,
    /// NeMo transducer, offline (`--encoder/--decoder/--joiner --model-type=nemo_transducer`).
    NemoTransducer,
    /// Qwen3-ASR (`--qwen3-asr-*`).
    Qwen3Asr,
    /// Cache-aware streaming transducer, run by the online server while you speak.
    StreamingTransducer,
    /// A synthetic CTC model (`--tdnn-model`) for the end-to-end test of the engine.
    #[cfg(test)]
    TestCtc,
}

impl Kind {
    /// Longest stretch of audio one request may carry (s); longer dictations are split at pauses.
    pub fn max_segment_s(self) -> f32 {
        match self {
            // Whisper sees 30 s; leave room for the tail padding sherpa adds.
            Kind::Whisper => 28.0,
            // Moonshine v1 was trained on short utterances.
            Kind::Moonshine => 30.0,
            // The NeMo ONNX export rejects inputs longer than about 200 s.
            Kind::NemoTransducer => 120.0,
            Kind::Qwen3Asr => 120.0,
            Kind::StreamingTransducer => f32::INFINITY,
            #[cfg(test)]
            Kind::TestCtc => 20.0,
        }
    }

    pub fn streams(self) -> bool {
        self == Kind::StreamingTransducer
    }
}

/// Where the files come from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// A `.tar.bz2` of the sherpa-onnx `asr-models` release; `dir` is the folder inside it.
    Release { archive: &'static str, dir: &'static str },
    /// Every file of a Hugging Face model repo at `revision`.
    HuggingFace { repo: &'static str, revision: &'static str },
}

impl Source {
    pub fn urls(&self) -> String {
        match self {
            Source::Release { archive, .. } => format!("{RELEASE_BASE}/{archive}"),
            Source::HuggingFace { repo, .. } => format!("https://huggingface.co/{repo}"),
        }
    }
}

pub const RELEASE_BASE: &str = "https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models";

/// A badge on a model row. The evidence for each is in docs/LOCAL_MODELS.md.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tag {
    /// Parakeet TDT 0.6B v3: 6.32 % average English WER (within 0.6 points of the best here), the
    /// fastest of the accurate models on a CPU, 25 European languages detected automatically.
    BestOverall,
    /// Qwen3-ASR 1.7B: the lowest English WER here (5.76 %) and the best multilingual FLEURS score
    /// (4.90, ahead of Whisper Large-v3's 5.27).
    MostAccurate,
    /// Whisper Large-v3 Turbo: 99 languages at ~7.75 % WER, faster than Large-v3 (RTFx 200 vs 146)
    /// for a 0.3-point loss.
    MostLanguages,
    /// Whisper Small: the most accurate multilingual model under ~600 MB; still faster than real
    /// time on a 2-core CPU (int8 RTF ~0.16).
    LowEnd,
    /// Distil-Whisper Small.en: the lowest WER (8.57 %) of the English-only models, at a third of
    /// Medium.en's size.
    BestEnglishOnly,
    /// Moonshine Base: 9.99 % WER (better than Whisper Base.en's 10.32) in a 239 MB download that
    /// runs ~560x real time.
    LowEndEnglish,
    /// Moonshine Tiny: the fastest model here (RTFx 753) and the smallest download (103 MB), about
    /// Whisper Tiny.en's accuracy (12.65 vs 12.81 %).
    Fastest,
    /// Nemotron 3.5 ASR Streaming: the only streaming model; it transcribes while you talk, so the
    /// text is ready the moment you stop.
    Streaming,
    /// Not packaged by sherpa-onnx itself (a community export).
    Community,
}

impl Tag {
    pub fn label(self) -> &'static str {
        match self {
            Tag::BestOverall => "Best overall",
            Tag::MostAccurate => "Most accurate",
            Tag::MostLanguages => "Most languages",
            Tag::LowEnd => "Low-end PCs",
            Tag::BestEnglishOnly => "Best English-only",
            Tag::LowEndEnglish => "Low-end · English",
            Tag::Fastest => "Fastest",
            Tag::Streaming => "Streaming",
            Tag::Community => "Community build",
        }
    }

    /// A recommendation (coloured) rather than a plain fact.
    pub fn is_recommendation(self) -> bool {
        self != Tag::Community
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LocalModel {
    /// Stable id stored in config.json.
    pub id: &'static str,
    pub name: &'static str,
    pub kind: Kind,
    pub source: Source,
    /// Download size in MB (the archive, or the sum of the repo files).
    pub download_mb: u32,
    /// What it understands, for the row ("English", "99 languages").
    pub languages: &'static str,
    /// English only: the language setting is ignored.
    pub english_only: bool,
    pub tags: &'static [Tag],
}

impl LocalModel {
    pub fn size_label(&self) -> String {
        size_label(self.download_mb)
    }

    /// The secondary line of a model row: "465 MB · 25 languages".
    pub fn summary(&self) -> String {
        format!("{} · {}", self.size_label(), self.languages)
    }
}

/// "103 MB", "1.0 GB".
pub fn size_label(mb: u32) -> String {
    if mb >= 1000 { format!("{:.1} GB", mb as f32 / 1000.0) } else { format!("{mb} MB") }
}

const fn release(archive: &'static str, dir: &'static str) -> Source {
    Source::Release { archive, dir }
}

/// The models, best first: recommendations, then the rest by size.
pub const MODELS: &[LocalModel] = &[
    LocalModel {
        id: "parakeet-tdt-0.6b-v3",
        name: "NVIDIA Parakeet TDT 0.6B v3",
        kind: Kind::NemoTransducer,
        source: release("sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8.tar.bz2", "sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8"),
        download_mb: 465,
        languages: "25 European languages",
        english_only: false,
        tags: &[Tag::BestOverall],
    },
    LocalModel {
        id: "qwen3-asr-1.7b",
        name: "Qwen3-ASR 1.7B",
        kind: Kind::Qwen3Asr,
        source: Source::HuggingFace { repo: "solavr/sherpa-onnx-qwen3-asr-1.7B-int8", revision: "main" },
        download_mb: 2300,
        languages: "52 languages and dialects",
        english_only: false,
        tags: &[Tag::MostAccurate, Tag::Community],
    },
    LocalModel {
        id: "whisper-large-v3-turbo",
        name: "Whisper Large-v3 Turbo",
        kind: Kind::Whisper,
        source: release("sherpa-onnx-whisper-turbo.tar.bz2", "sherpa-onnx-whisper-turbo"),
        download_mb: 538,
        languages: "99 languages",
        english_only: false,
        tags: &[Tag::MostLanguages],
    },
    LocalModel {
        id: "nemotron-3.5-asr-streaming-0.6b",
        name: "Nemotron-3.5-ASR-Streaming 0.6B",
        kind: Kind::StreamingTransducer,
        source: release(
            "sherpa-onnx-nemotron-3.5-asr-streaming-0.6b-560ms-int8-2026-06-11.tar.bz2",
            "sherpa-onnx-nemotron-3.5-asr-streaming-0.6b-560ms-int8-2026-06-11",
        ),
        download_mb: 650,
        languages: "15 languages",
        english_only: false,
        tags: &[Tag::Streaming],
    },
    LocalModel {
        id: "whisper-small",
        name: "Whisper Small",
        kind: Kind::Whisper,
        source: release("sherpa-onnx-whisper-small.tar.bz2", "sherpa-onnx-whisper-small"),
        download_mb: 610,
        languages: "99 languages",
        english_only: false,
        tags: &[Tag::LowEnd],
    },
    LocalModel {
        id: "distil-whisper-small.en",
        name: "Distil-Whisper Small.en",
        kind: Kind::Whisper,
        source: release("sherpa-onnx-whisper-distil-small.en.tar.bz2", "sherpa-onnx-whisper-distil-small.en"),
        download_mb: 433,
        languages: "English",
        english_only: true,
        tags: &[Tag::BestEnglishOnly],
    },
    LocalModel {
        id: "moonshine-base",
        name: "Moonshine Base",
        kind: Kind::Moonshine,
        source: release("sherpa-onnx-moonshine-base-en-int8.tar.bz2", "sherpa-onnx-moonshine-base-en-int8"),
        download_mb: 239,
        languages: "English",
        english_only: true,
        tags: &[Tag::LowEndEnglish],
    },
    LocalModel {
        id: "moonshine-tiny",
        name: "Moonshine Tiny",
        kind: Kind::Moonshine,
        source: release("sherpa-onnx-moonshine-tiny-en-int8.tar.bz2", "sherpa-onnx-moonshine-tiny-en-int8"),
        download_mb: 103,
        languages: "English",
        english_only: true,
        tags: &[Tag::Fastest],
    },
    LocalModel {
        id: "qwen3-asr-0.6b",
        name: "Qwen3-ASR 0.6B",
        kind: Kind::Qwen3Asr,
        source: release("sherpa-onnx-qwen3-asr-0.6B-int8-2026-03-25.tar.bz2", "sherpa-onnx-qwen3-asr-0.6B-int8-2026-03-25"),
        download_mb: 900,
        languages: "52 languages and dialects",
        english_only: false,
        tags: &[],
    },
    LocalModel {
        id: "whisper-large-v3",
        name: "Whisper Large-v3",
        kind: Kind::Whisper,
        source: release("sherpa-onnx-whisper-large-v3.tar.bz2", "sherpa-onnx-whisper-large-v3"),
        download_mb: 1020,
        languages: "99 languages",
        english_only: false,
        tags: &[],
    },
    LocalModel {
        id: "distil-whisper-large-v3",
        name: "Distil-Whisper Large-v3",
        kind: Kind::Whisper,
        source: release("sherpa-onnx-whisper-distil-large-v3.tar.bz2", "sherpa-onnx-whisper-distil-large-v3"),
        download_mb: 505,
        languages: "English",
        english_only: true,
        tags: &[],
    },
    LocalModel {
        id: "distil-whisper-medium.en",
        name: "Distil-Whisper Medium.en",
        kind: Kind::Whisper,
        source: release("sherpa-onnx-whisper-distil-medium.en.tar.bz2", "sherpa-onnx-whisper-distil-medium.en"),
        download_mb: 961,
        languages: "English",
        english_only: true,
        tags: &[],
    },
    LocalModel {
        id: "whisper-medium",
        name: "Whisper Medium",
        kind: Kind::Whisper,
        source: release("sherpa-onnx-whisper-medium.tar.bz2", "sherpa-onnx-whisper-medium"),
        download_mb: 1800,
        languages: "99 languages",
        english_only: false,
        tags: &[],
    },
    LocalModel {
        id: "whisper-base",
        name: "Whisper Base",
        kind: Kind::Whisper,
        source: release("sherpa-onnx-whisper-base.tar.bz2", "sherpa-onnx-whisper-base"),
        download_mb: 198,
        languages: "99 languages",
        english_only: false,
        tags: &[],
    },
    LocalModel {
        id: "whisper-tiny",
        name: "Whisper Tiny",
        kind: Kind::Whisper,
        source: release("sherpa-onnx-whisper-tiny.tar.bz2", "sherpa-onnx-whisper-tiny"),
        download_mb: 111,
        languages: "99 languages",
        english_only: false,
        tags: &[],
    },
];

/// The synthetic model of the end-to-end test (`tools/make_test_ctc.py` builds its archive).
#[cfg(test)]
pub const TEST_MODEL: LocalModel = LocalModel {
    id: "ot-test-ctc",
    name: "Test CTC",
    kind: Kind::TestCtc,
    source: release("ot-test-ctc.tar.bz2", "ot-test-ctc"),
    download_mb: 1,
    languages: "English",
    english_only: true,
    tags: &[],
};

pub fn get(id: &str) -> Option<&'static LocalModel> {
    MODELS.iter().find(|m| m.id == id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_catalog_is_the_requested_fifteen_with_unique_ids() {
        assert_eq!(MODELS.len(), 15);
        let mut ids: Vec<_> = MODELS.iter().map(|m| m.id).collect();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), 15, "ids are unique");
        for name in [
            "Moonshine Tiny",
            "Moonshine Base",
            "Whisper Tiny",
            "Whisper Base",
            "Whisper Small",
            "Distil-Whisper Small.en",
            "Qwen3-ASR 0.6B",
            "NVIDIA Parakeet TDT 0.6B v3",
            "Whisper Large-v3 Turbo",
            "Whisper Large-v3",
            "Distil-Whisper Large-v3",
            "Distil-Whisper Medium.en",
            "Qwen3-ASR 1.7B",
            "Nemotron-3.5-ASR-Streaming 0.6B",
            "Whisper Medium",
        ] {
            assert!(MODELS.iter().any(|m| m.name == name), "{name} is in the catalog");
        }
    }

    #[test]
    fn each_recommendation_goes_to_exactly_one_model() {
        for tag in [Tag::BestOverall, Tag::MostAccurate, Tag::MostLanguages, Tag::LowEnd, Tag::BestEnglishOnly, Tag::LowEndEnglish, Tag::Fastest, Tag::Streaming] {
            assert_eq!(MODELS.iter().filter(|m| m.tags.contains(&tag)).count(), 1, "{tag:?}");
        }
        assert_eq!(get("parakeet-tdt-0.6b-v3").unwrap().tags, &[Tag::BestOverall]);
        assert!(get("qwen3-asr-1.7b").unwrap().tags.contains(&Tag::Community));
        assert!(MODELS.iter().all(|m| m.tags.contains(&Tag::Streaming) == m.kind.streams()), "only the streaming model streams");
    }

    #[test]
    fn english_only_models_say_so() {
        for m in MODELS {
            assert_eq!(m.english_only, m.languages == "English", "{}", m.id);
        }
    }

    #[test]
    fn release_archives_unpack_to_their_folder() {
        for m in MODELS {
            if let Source::Release { archive, dir } = m.source {
                assert_eq!(archive, format!("{dir}.tar.bz2"), "{}", m.id);
                assert!(archive.starts_with("sherpa-onnx-"));
            }
        }
    }

    #[test]
    fn labels() {
        assert_eq!(size_label(103), "103 MB");
        assert_eq!(size_label(1020), "1.0 GB");
        assert_eq!(get("parakeet-tdt-0.6b-v3").unwrap().summary(), "465 MB · 25 European languages");
        assert_eq!(get("nope"), None);
    }
}
