//! Navigation vocabulary shared by the tray popup pages, the IPC page request and `--settings`.
//!
//! [`Route`] names a view of the single tray popup (Home or a page); the popup keeps a stack of
//! routes and Back pops it. [`Section`] is a Settings page, [`PickerId`] a list picker (the
//! popup has no native dropdowns). `Route::parse` reads the `--settings [page]` argument and
//! `encode`/`decode` pack a route into the `wparam` of `WM_OT_OPEN_PAGE` (the 64-bit message
//! parameter; x86_64 only, asserted below).
//!
//! Pure: no Win32, no I/O.

const _: () = assert!(usize::BITS >= 64, "Route::encode packs a 48-bit payload into a usize");

/// A Settings page (the index lists them in this order, grouped by [`Section::groups`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub enum Section {
    #[default]
    Preferences,
    Hotkeys,
    SpeechToText,
    /// Cleanup & AI: filler/fuzzy/line-break options, AI cleanup provider, and the "AI rewrite"
    /// group (voice rewrite trigger) first.
    Cleanup,
    Privacy,
    /// Version, data/logs folders, reset (formerly "System").
    About,
}

impl Section {
    pub const ALL: [Section; 6] = [Section::Preferences, Section::Hotkeys, Section::SpeechToText, Section::Cleanup, Section::Privacy, Section::About];

    pub fn label(self) -> &'static str {
        match self {
            Section::Preferences => "Preferences",
            Section::Hotkeys => "Hotkeys",
            Section::SpeechToText => "Speech-to-Text",
            Section::Cleanup => "Cleanup & AI",
            Section::Privacy => "Privacy & Data",
            Section::About => "About",
        }
    }

    /// Settings index groups: (group label, sections).
    pub fn groups() -> [(&'static str, &'static [Section]); 3] {
        [
            ("App", &[Section::Preferences, Section::Hotkeys]),
            ("AI Models", &[Section::SpeechToText, Section::Cleanup]),
            ("System", &[Section::Privacy, Section::About]),
        ]
    }

    /// Parse the `settings:<section>` suffix or a bare section name (case-insensitive). Keeps the
    /// old aliases: `system` is About, `stt` is Speech-to-Text.
    pub fn parse(s: &str) -> Option<Section> {
        match s.trim().to_lowercase().as_str() {
            "preferences" | "prefs" | "general" => Some(Section::Preferences),
            "hotkeys" | "hotkey" | "shortcuts" => Some(Section::Hotkeys),
            "speech-to-text" | "stt" | "speechtotext" | "transcription" => Some(Section::SpeechToText),
            "cleanup" | "cleanup-ai" | "cleanup&ai" | "intelligence" | "llm" | "ai" | "rewrite" => Some(Section::Cleanup),
            "privacy" | "privacy-data" | "privacydata" => Some(Section::Privacy),
            "about" | "system" => Some(Section::About),
            _ => None,
        }
    }
}

/// A list picker route (`Item::Choice` opens it; picking applies and goes Back).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PickerId {
    /// Transcription language (61 entries, with a filter field).
    Language,
    /// Input device (System default + devices; has a Refresh button).
    InputDevice,
    /// Keep the microphone warm (Off, 30 s, 1 min, 5 min, 15 min).
    MicWarm,
    /// Keep transcripts for (forever, 7 d, 30 d, 90 d, 1 year).
    Retention,
    /// Speech-to-text provider.
    SttProvider,
    /// AI cleanup / rewrite provider.
    LlmProvider,
    /// AI cleanup model suggestions for the active LLM provider.
    LlmModel,
    /// AI rewrite model suggestions for the active LLM provider.
    LlmRewriteModel,
}

impl PickerId {
    pub const ALL: [PickerId; 8] = [
        PickerId::Language,
        PickerId::InputDevice,
        PickerId::MicWarm,
        PickerId::Retention,
        PickerId::SttProvider,
        PickerId::LlmProvider,
        PickerId::LlmModel,
        PickerId::LlmRewriteModel,
    ];
}

/// A view of the tray popup. `Home` is the existing 288 px menu; every other route is a 380 px page.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub enum Route {
    #[default]
    Home,
    History,
    /// One history entry (detail page); the id is `core::history::Entry::id` (unix ms).
    HistoryEntry(i64),
    /// Dictionary words.
    Dictionary,
    Snippets,
    /// Edit snippet `Some(index in Config.snippets)`, or `None` for a new one.
    SnippetEdit(Option<usize>),
    /// The list of Settings sections.
    SettingsIndex,
    Settings(Section),
    Picker(PickerId),
    /// Speech-to-Text > Bring your own key (cloud provider, key, model).
    SttCloud,
    /// Speech-to-Text > Use a local model (the model list).
    SttLocal,
}

/// Marks a wparam as a route code, so a stray message with wparam 0 is not read as a route.
const VALID: usize = 1 << 8;
const TAG_MASK: usize = 0xFF;
const PAYLOAD_SHIFT: u32 = 16;
/// Largest payload (`HistoryEntry` ids are unix ms, about 41 bits).
const PAYLOAD_MAX: u64 = (1u64 << 47) - 1;

impl Route {
    /// `--settings [page]`: None | "home" | "history" | "dictionary" | "snippets" |
    /// "settings[:section]" (`settings/section` also) | a bare section name such as `stt`. Matching
    /// is case-insensitive and ignores leading dashes and a `page=` prefix. `settings` alone and
    /// `settings:<unknown>` open the Settings index; anything unrecognised opens Home.
    pub fn parse(page: Option<&str>) -> Route {
        let raw = page.unwrap_or("").trim().trim_start_matches('-').to_lowercase();
        let raw = raw.strip_prefix("page=").unwrap_or(&raw);
        match raw {
            "" | "home" | "menu" => Route::Home,
            "history" => Route::History,
            "dictionary" | "dict" | "words" => Route::Dictionary,
            "snippets" => Route::Snippets,
            "settings" => Route::SettingsIndex,
            "stt:cloud" | "stt-cloud" | "byok" | "settings:stt:cloud" => Route::SttCloud,
            "stt:local" | "stt-local" | "local" | "local-models" | "settings:stt:local" => Route::SttLocal,
            s => {
                if let Some(sec) = s.strip_prefix("settings:").or_else(|| s.strip_prefix("settings/")) {
                    Section::parse(sec).map_or(Route::SettingsIndex, Route::Settings)
                } else {
                    Section::parse(s).map_or(Route::Home, Route::Settings)
                }
            }
        }
    }

    fn tag_payload(self) -> (usize, u64) {
        match self {
            Route::Home => (0, 0),
            Route::History => (1, 0),
            // A negative id cannot be a real entry: it falls back to the list.
            Route::HistoryEntry(id) if (0..=PAYLOAD_MAX as i64).contains(&id) => (2, id as u64),
            Route::HistoryEntry(_) => (1, 0),
            Route::Dictionary => (3, 0),
            Route::Snippets => (4, 0),
            Route::SnippetEdit(None) => (5, 0),
            Route::SnippetEdit(Some(i)) => (5, (i as u64).saturating_add(1).min(PAYLOAD_MAX)),
            Route::SettingsIndex => (6, 0),
            Route::Settings(s) => (7, Section::ALL.iter().position(|x| *x == s).unwrap_or(0) as u64),
            Route::Picker(p) => (8, PickerId::ALL.iter().position(|x| *x == p).unwrap_or(0) as u64),
            Route::SttCloud => (9, 0),
            Route::SttLocal => (10, 0),
        }
    }

    /// Pack into the `wparam` of `WM_OT_OPEN_PAGE`: bit 8 = valid, bits 0..8 = route tag, bits
    /// 16..64 = payload (history id, snippet index + 1, section/picker index).
    pub fn encode(self) -> usize {
        let (tag, payload) = self.tag_payload();
        VALID | tag | (payload as usize) << PAYLOAD_SHIFT
    }

    /// Inverse of [`Route::encode`]; `None` for anything that is not a valid code.
    pub fn decode(wparam: usize) -> Option<Route> {
        if wparam & VALID == 0 || wparam & 0xFE00 != 0 {
            return None;
        }
        let payload = (wparam >> PAYLOAD_SHIFT) as u64;
        let no_payload = |r: Route| (payload == 0).then_some(r);
        match wparam & TAG_MASK {
            0 => no_payload(Route::Home),
            1 => no_payload(Route::History),
            2 => Some(Route::HistoryEntry(payload as i64)),
            3 => no_payload(Route::Dictionary),
            4 => no_payload(Route::Snippets),
            5 => Some(Route::SnippetEdit(payload.checked_sub(1).map(|i| i as usize))),
            6 => no_payload(Route::SettingsIndex),
            7 => Section::ALL.get(payload as usize).map(|s| Route::Settings(*s)),
            8 => PickerId::ALL.get(payload as usize).map(|p| Route::Picker(*p)),
            9 => no_payload(Route::SttCloud),
            10 => no_payload(Route::SttLocal),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn every_route() -> Vec<Route> {
        let mut v = vec![
            Route::Home,
            Route::History,
            Route::HistoryEntry(0),
            Route::HistoryEntry(1_760_000_000_123),
            Route::Dictionary,
            Route::Snippets,
            Route::SnippetEdit(None),
            Route::SnippetEdit(Some(0)),
            Route::SnippetEdit(Some(41)),
            Route::SettingsIndex,
        ];
        v.extend(Section::ALL.iter().map(|s| Route::Settings(*s)));
        v.extend(PickerId::ALL.iter().map(|p| Route::Picker(*p)));
        v.extend([Route::SttCloud, Route::SttLocal]);
        v
    }

    #[test]
    fn speech_to_text_sub_pages_parse() {
        assert_eq!(Route::parse(Some("stt:local")), Route::SttLocal);
        assert_eq!(Route::parse(Some("settings:stt:cloud")), Route::SttCloud);
        assert_eq!(Route::parse(Some("byok")), Route::SttCloud);
        assert_eq!(Route::parse(Some("stt")), Route::Settings(Section::SpeechToText), "the chooser");
    }

    #[test]
    fn section_parse_aliases() {
        assert_eq!(Section::parse("stt"), Some(Section::SpeechToText));
        assert_eq!(Section::parse("Hotkeys"), Some(Section::Hotkeys));
        assert_eq!(Section::parse("privacy-data"), Some(Section::Privacy));
        assert_eq!(Section::parse("system"), Some(Section::About), "System was renamed About");
        assert_eq!(Section::parse("About"), Some(Section::About));
        assert_eq!(Section::parse("cleanup"), Some(Section::Cleanup));
        assert_eq!(Section::parse("nope"), None);
    }

    #[test]
    fn groups_cover_every_section_once() {
        let mut seen: Vec<Section> = Section::groups().iter().flat_map(|(_, s)| s.iter().copied()).collect();
        seen.sort_by_key(|s| *s as u8);
        let mut all = Section::ALL.to_vec();
        all.sort_by_key(|s| *s as u8);
        assert_eq!(seen, all);
    }

    #[test]
    fn page_arguments() {
        assert_eq!(Route::parse(None), Route::Home);
        assert_eq!(Route::parse(Some("")), Route::Home);
        assert_eq!(Route::parse(Some("home")), Route::Home);
        assert_eq!(Route::parse(Some("history")), Route::History);
        assert_eq!(Route::parse(Some("Dictionary")), Route::Dictionary);
        assert_eq!(Route::parse(Some("snippets")), Route::Snippets);
        assert_eq!(Route::parse(Some("settings")), Route::SettingsIndex);
        assert_eq!(Route::parse(Some("settings:stt")), Route::Settings(Section::SpeechToText));
        assert_eq!(Route::parse(Some("settings/hotkeys")), Route::Settings(Section::Hotkeys));
        assert_eq!(Route::parse(Some("settings:system")), Route::Settings(Section::About));
        assert_eq!(Route::parse(Some("settings:bogus")), Route::SettingsIndex);
        assert_eq!(Route::parse(Some("cleanup")), Route::Settings(Section::Cleanup));
        assert_eq!(Route::parse(Some("system")), Route::Settings(Section::About));
        assert_eq!(Route::parse(Some("--page=dictionary")), Route::Dictionary);
        assert_eq!(Route::parse(Some("  --Settings:Privacy ")), Route::Settings(Section::Privacy));
        assert_eq!(Route::parse(Some("garbage")), Route::Home);
    }

    #[test]
    fn route_code_round_trips_every_route() {
        for r in every_route() {
            let c = r.encode();
            assert_eq!(Route::decode(c), Some(r), "{r:?} -> {c:#x}");
        }
        // Distinct routes never share a code.
        let mut codes: Vec<usize> = every_route().into_iter().map(Route::encode).collect();
        let n = codes.len();
        codes.sort_unstable();
        codes.dedup();
        assert_eq!(codes.len(), n);
    }

    #[test]
    fn route_code_covers_everything_parse_accepts() {
        for (i, arg) in [None, Some("home"), Some("history"), Some("dictionary"), Some("snippets"), Some("settings"), Some("settings:stt"), Some("settings:cleanup"), Some("about")]
            .into_iter()
            .enumerate()
        {
            let r = Route::parse(arg);
            assert_eq!(Route::decode(r.encode()), Some(r), "{i}");
        }
    }

    #[test]
    fn route_code_rejects_junk() {
        assert_eq!(Route::decode(0), None, "no valid bit");
        assert_eq!(Route::decode(1), None);
        assert_eq!(Route::decode(VALID | 11), None, "unknown tag");
        assert_eq!(Route::decode(VALID | 9 | 1 << 16), None, "the STT sub-pages take no payload");
        assert_eq!(Route::decode(VALID | 0xFF), None);
        assert_eq!(Route::decode(VALID | 1 << 9), None, "stray flag bits");
        assert_eq!(Route::decode(VALID | 7 | (Section::ALL.len()) << 16), None, "unknown section");
        assert_eq!(Route::decode(VALID | 8 | (PickerId::ALL.len()) << 16), None, "unknown picker");
        assert_eq!(Route::decode(VALID | 1 | 5 << 16), None, "payload on a unit route");
        assert_eq!(Route::decode(usize::MAX), None);
    }

    #[test]
    fn out_of_range_history_id_falls_back_to_the_list() {
        assert_eq!(Route::decode(Route::HistoryEntry(-5).encode()), Some(Route::History));
        assert_eq!(Route::decode(Route::HistoryEntry(i64::MAX).encode()), Some(Route::History));
    }
}
