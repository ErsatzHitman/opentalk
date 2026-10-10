//! Pure history view logic shared by the tray pages: filtering, day grouping, stats and the
//! number/meta formatting. No Win32 and no file access; everything is unit-tested below.
use super::history::{Entry, Status};
use super::timefmt;

/// One day's worth of rows (indices into the entry list).
#[derive(Debug, PartialEq, Eq)]
pub struct Group {
    pub label: String,
    pub items: Vec<usize>,
}

/// Lower-cased words of a search query.
pub fn query_words(query: &str) -> Vec<String> {
    query.split_whitespace().map(|s| s.to_lowercase()).collect()
}

/// Whether `e` contains every one of `words` (from [`query_words`]) in its text, raw transcript,
/// app, provider, model or error, ignoring case. No words match everything.
pub fn matches(e: &Entry, words: &[String]) -> bool {
    if words.is_empty() {
        return true;
    }
    let hay = format!("{} {} {} {} {} {}", e.text, e.raw, e.app_name.as_deref().unwrap_or(""), e.provider, e.model, e.error.as_deref().unwrap_or("")).to_lowercase();
    words.iter().all(|w| hay.contains(w))
}

/// Indices of entries to show: discarded rows only when asked for; the query matches text, raw
/// transcript, app, provider, model and error (case-insensitive substring over all words).
pub fn visible(entries: &[Entry], query: &str, show_discarded: bool) -> Vec<usize> {
    let words = query_words(query);
    entries.iter().enumerate().filter(|(_, e)| show_discarded || e.status != Status::Discarded).filter(|(_, e)| matches(e, &words)).map(|(i, _)| i).collect()
}

/// Group consecutive visible rows (newest first) by local calendar day.
pub fn group_by_day(entries: &[Entry], idx: &[usize], now_ms: i64, offset_of: &dyn Fn(i64) -> i64) -> Vec<Group> {
    let mut groups: Vec<Group> = Vec::new();
    for &i in idx {
        let label = timefmt::day_label(entries[i].ts, now_ms, offset_of);
        match groups.last_mut() {
            Some(g) if g.label == label => g.items.push(i),
            _ => groups.push(Group { label, items: vec![i] }),
        }
    }
    groups
}

#[derive(Debug, PartialEq, Eq, Default)]
pub struct Stats {
    pub dictations: usize,
    /// Entries that failed (not counted in `dictations`).
    pub failed: usize,
    pub words: usize,
    /// Mean end-to-end latency over successful rows that recorded one (ms).
    pub avg_latency_ms: Option<u64>,
}

pub fn word_count(s: &str) -> usize {
    s.split_whitespace().count()
}

pub fn stats(entries: &[Entry]) -> Stats {
    let mut acc = StatsAcc::default();
    entries.iter().for_each(|e| acc.add(e));
    acc.finish()
}

/// [`stats`] one entry at a time, for scans that must not hold the whole history in memory.
#[derive(Default)]
pub struct StatsAcc {
    stats: Stats,
    latency_sum: u64,
    latency_n: u64,
}

impl StatsAcc {
    pub fn add(&mut self, e: &Entry) {
        match e.status {
            Status::Ok => {
                self.stats.dictations += 1;
                self.stats.words += word_count(&e.text);
                if e.latency_ms > 0 {
                    self.latency_sum += e.latency_ms;
                    self.latency_n += 1;
                }
            }
            Status::Failed => self.stats.failed += 1,
            Status::Discarded => {}
        }
    }

    pub fn finish(mut self) -> Stats {
        self.stats.avg_latency_ms = (self.latency_n > 0).then(|| self.latency_sum / self.latency_n);
        self.stats
    }
}

/// "12 dictations · 1,204 words · avg 1.2 s · 2 failed" (parts that do not apply are skipped).
pub fn stats_line(s: &Stats) -> String {
    let mut parts = vec![format!("{} {}", thousands(s.dictations), if s.dictations == 1 { "dictation" } else { "dictations" })];
    parts.push(format!("{} {}", thousands(s.words), if s.words == 1 { "word" } else { "words" }));
    if let Some(ms) = s.avg_latency_ms {
        parts.push(format!("avg {}", format_latency(ms)));
    }
    if s.failed > 0 {
        parts.push(format!("{} failed", thousands(s.failed)));
    }
    parts.join(" · ")
}

pub fn format_latency(ms: u64) -> String {
    if ms >= 10_000 { format!("{:.0} s", ms as f64 / 1000.0) } else if ms >= 1000 { format!("{:.1} s", ms as f64 / 1000.0) } else { format!("{ms} ms") }
}

/// "1,234" style thousands grouping.
pub fn thousands(n: usize) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// "OpenAI · gpt-4o-mini-transcribe · 840 ms" (empty parts are skipped).
pub fn meta_line(e: &Entry) -> String {
    let provider = crate::stt::registry::PROVIDERS
        .iter()
        .find(|p| p.name.eq_ignore_ascii_case(&e.provider) || format!("{:?}", p.id).eq_ignore_ascii_case(&e.provider))
        .map(|p| p.name.to_string())
        .unwrap_or_else(|| e.provider.clone());
    let mut parts: Vec<String> = Vec::new();
    if !provider.is_empty() {
        parts.push(provider);
    }
    if !e.model.is_empty() {
        parts.push(e.model.clone());
    }
    if e.latency_ms > 0 {
        parts.push(format_latency(e.latency_ms));
    }
    parts.join(" · ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: i64, ts: i64, text: &str, status: Status) -> Entry {
        Entry { id, ts, text: text.into(), raw: format!("raw {text}"), status, ..Default::default() }
    }

    fn day_ms(y: i32, m: u32, d: u32, h: i64) -> i64 {
        (timefmt::days_from_civil(y, m, d) * 86_400 + h * 3600) * 1000
    }

    #[test]
    fn filter_hides_discarded_and_matches_all_words() {
        let mut es = vec![
            entry(3, 3, "Send the report to Priya", Status::Ok),
            entry(2, 2, "discarded noise", Status::Discarded),
            entry(1, 1, "Buy milk", Status::Failed),
        ];
        es[0].app_name = Some("Outlook".into());
        assert_eq!(visible(&es, "", false), vec![0, 2]);
        assert_eq!(visible(&es, "", true), vec![0, 1, 2]);
        assert_eq!(visible(&es, "report priya", false), vec![0]);
        assert_eq!(visible(&es, "OUTLOOK", false), vec![0]);
        assert_eq!(visible(&es, "raw buy", false), vec![2], "raw transcript is searchable");
        assert!(visible(&es, "zzz", true).is_empty());
    }

    #[test]
    fn grouping_splits_on_local_day() {
        let now = day_ms(2026, 10, 9, 12);
        let es = vec![
            entry(5, day_ms(2026, 10, 9, 11), "a", Status::Ok),
            entry(4, day_ms(2026, 10, 9, 1), "b", Status::Ok),
            entry(3, day_ms(2026, 10, 8, 20), "c", Status::Ok),
            entry(2, day_ms(2026, 10, 2, 9), "d", Status::Ok),
            entry(1, day_ms(2026, 10, 2, 8), "e", Status::Ok),
        ];
        let idx: Vec<usize> = (0..es.len()).collect();
        let g = group_by_day(&es, &idx, now, &|_| 0);
        assert_eq!(g.iter().map(|g| g.label.as_str()).collect::<Vec<_>>(), vec!["Today", "Yesterday", "Oct 2, 2026"]);
        assert_eq!(g[0].items, vec![0, 1]);
        assert_eq!(g[2].items, vec![3, 4]);
    }

    #[test]
    fn grouping_respects_timezone_offset() {
        let now = day_ms(2026, 10, 9, 12);
        let es = vec![entry(1, day_ms(2026, 10, 8, 23) + 1_800_000, "x", Status::Ok)];
        let g = group_by_day(&es, &[0], now, &|_| 3600);
        assert_eq!(g[0].label, "Today");
    }

    #[test]
    fn stats_only_count_successful_rows() {
        let mut a = entry(1, 1, "one two three", Status::Ok);
        a.latency_ms = 800;
        let mut b = entry(2, 2, "four five", Status::Ok);
        b.latency_ms = 1200;
        let c = entry(3, 3, "ignored words here", Status::Failed);
        let s = stats(&[a, b, c]);
        assert_eq!(s, Stats { dictations: 2, failed: 1, words: 5, avg_latency_ms: Some(1000) });
        assert_eq!(stats(&[]), Stats::default());
    }

    #[test]
    fn stats_line_reads_naturally() {
        let s = Stats { dictations: 1, failed: 0, words: 1, avg_latency_ms: None };
        assert_eq!(stats_line(&s), "1 dictation · 1 word");
        let s = Stats { dictations: 1204, failed: 2, words: 15_300, avg_latency_ms: Some(1200) };
        assert_eq!(stats_line(&s), "1,204 dictations · 15,300 words · avg 1.2 s · 2 failed");
        assert_eq!(stats_line(&Stats::default()), "0 dictations · 0 words");
    }

    #[test]
    fn number_formatting() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1234), "1,234");
        assert_eq!(thousands(1_234_567), "1,234,567");
        assert_eq!(format_latency(840), "840 ms");
        assert_eq!(format_latency(1500), "1.5 s");
        assert_eq!(format_latency(12_000), "12 s");
    }

    #[test]
    fn meta_line_skips_empty_parts() {
        let mut e = Entry::default();
        assert_eq!(meta_line(&e), "");
        e.provider = "openai".into();
        e.model = "whisper-1".into();
        e.latency_ms = 640;
        assert_eq!(meta_line(&e), "OpenAI · whisper-1 · 640 ms");
        e.provider = "weird".into();
        e.latency_ms = 0;
        assert_eq!(meta_line(&e), "weird · whisper-1");
    }
}
