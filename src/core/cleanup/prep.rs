//! Stages 1-3: non-speech tag stripping, hallucination guard, n-gram loop collapse.
use super::tokens::{Doc, Kind};
use std::borrow::Cow;

/// Bracketed content that is a STT annotation, not speech (lower-cased, `_`/`-` as spaces).
fn is_nonspeech_label(content: &str) -> bool {
    let label: String = content
        .chars()
        .map(|c| if matches!(c, '_' | '-') { ' ' } else { c })
        .flat_map(char::to_lowercase)
        .collect();
    let label = label.split_whitespace().collect::<Vec<_>>().join(" ");
    if label.is_empty() {
        // "[ __ ]" (a bleeped word) leaves no letters at all.
        return !content.trim().is_empty() && content.chars().all(|c| c == '_' || c.is_whitespace());
    }
    matches!(
        label.as_str(),
        "blank audio" | "blank" | "music" | "music playing" | "music plays" | "silence" | "silent" | "applause"
            | "laughter" | "laughs" | "laughing" | "noise" | "background noise" | "inaudible" | "no speech"
            | "sound" | "sounds" | "cough" | "coughs" | "coughing" | "sigh" | "sighs" | "sniffs" | "typing"
            | "keyboard clicking" | "clicking" | "pause" | "foreign" | "speaking foreign language" | "crosstalk"
            | "unintelligible" | "static" | "beep" | "ringing" | "phone ringing" | "no audio" | "audio"
            | "silence ." | "music ♪" | "video playback" | "singing"
    )
}

fn is_caps_tag(content: &str) -> bool {
    let c = content.trim();
    c.len() >= 4
        && c.contains('_')
        && c.chars().all(|ch| ch.is_ascii_uppercase() || ch.is_ascii_digit() || ch == '_')
}

/// Remove `[BLANK_AUDIO]`, `[Music]`, `(silence)`, `*applause*` and `♪ ... ♪` annotations.
pub fn strip_nonspeech(s: &str) -> Cow<'_, str> {
    if !s.contains(['[', '(', '*', '♪', '♫']) {
        return Cow::Borrowed(s);
    }
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    let mut changed = false;
    while i < chars.len() {
        let c = chars[i];
        match c {
            '♪' | '♫' => {
                // A pair of note symbols wraps lyrics/music; a lone one is just noise.
                let close = chars[i + 1..].iter().position(|&d| d == '♪' || d == '♫');
                i = match close {
                    Some(k) => i + 1 + k + 1,
                    None => i + 1,
                };
                changed = true;
                out.push(' ');
                continue;
            }
            '[' | '(' | '*' => {
                let closer = match c {
                    '[' => ']',
                    '(' => ')',
                    _ => '*',
                };
                let end = chars[i + 1..].iter().take(41).position(|&d| d == closer);
                if let Some(k) = end {
                    let inner: String = chars[i + 1..i + 1 + k].iter().collect();
                    if is_nonspeech_label(&inner) || (c == '[' && is_caps_tag(&inner)) {
                        i += k + 2;
                        changed = true;
                        out.push(' ');
                        continue;
                    }
                }
            }
            _ => {}
        }
        out.push(c);
        i += 1;
    }
    if changed { Cow::Owned(out) } else { Cow::Borrowed(s) }
}

/// Phrases STT models invent for silence or music (case/punctuation-insensitive).
const HALLUCINATIONS: &[&str] = &[
    "thank you for watching",
    "thank you for watching this video",
    "thank you so much for watching",
    "thanks for watching",
    "thanks for watching and see you in the next video",
    "please subscribe",
    "please subscribe to my channel",
    "subscribe to my channel",
    "don't forget to subscribe",
    "like and subscribe",
    "please like and subscribe",
    "see you in the next video",
    "see you in the next one",
    "see you next time",
    "thank you for listening",
    "thanks for listening",
    "subtitles by the amara.org community",
    "amara.org",
    "transcribed by",
    "продолжение следует",
    "субтитры сделал dimatorzok",
    "субтитры создавал dimatorzok",
    "спасибо за просмотр",
    "спасибо за внимание",
    "подписывайтесь на канал",
    "редактор субтитров а.семкин корректор а.егорова",
    "sous-titres réalisés par la communauté d'amara.org",
    "sous-titrage st' 501",
    "merci d'avoir regardé cette vidéo",
    "merci d'avoir regardé",
    "abonnez-vous",
    "untertitel der amara.org-community",
    "untertitel im auftrag des zdf für funk, 2017",
    "untertitelung des zdf, 2020",
    "vielen dank fürs zuschauen",
    "danke fürs zuschauen",
    "vielen dank für ihre aufmerksamkeit",
    "gracias por ver el video",
    "gracias por ver",
    "suscríbete",
    "suscríbete al canal",
    "subtítulos realizados por la comunidad de amara.org",
    "obrigado por assistir",
    "legendas pela comunidade amara.org",
    "inscreva-se no canal",
    "grazie per la visione",
    "sottotitoli creati dalla comunità amara.org",
    "iscriviti al canale",
    "ご視聴ありがとうございました",
    "ご視聴ありがとうございます",
    "最後までご視聴いただきありがとうございました",
    "チャンネル登録お願いします",
    "チャンネル登録よろしくお願いします",
    "谢谢观看",
    "謝謝觀看",
    "感谢观看",
    "字幕由amara.org社群提供",
    "请不吝点赞 订阅 转发 打赏支持明镜与点点栏目",
];

fn words_only(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars().flat_map(char::to_lowercase) {
        if c.is_alphanumeric() {
            out.push(c);
        } else if !out.is_empty() && !out.ends_with(' ') {
            out.push(' ');
        }
    }
    if out.ends_with(' ') {
        out.pop();
    }
    out
}

/// True when the whole text is made of known hallucinated phrases. Only meaningful for
/// near-silent recordings: the caller checks that the VAD heard under 0.6 s of speech.
pub fn is_hallucination(text: &str) -> bool {
    let norm = words_only(text);
    if norm.is_empty() {
        return false;
    }
    let mut phrases: Vec<String> = HALLUCINATIONS.iter().map(|p| words_only(p)).collect();
    phrases.sort_by(|a, b| b.len().cmp(&a.len()));
    let mut rest = norm.as_str();
    while !rest.is_empty() {
        let hit = phrases.iter().find(|p| {
            rest.starts_with(p.as_str()) && rest[p.len()..].chars().next().is_none_or(|c| c == ' ')
        });
        match hit {
            Some(p) => rest = rest[p.len()..].trim_start(),
            None => return false,
        }
    }
    true
}

fn is_digit_word(lc: &str) -> bool {
    lc.chars().all(|c| c.is_ascii_digit())
        || matches!(lc, "zero" | "one" | "two" | "three" | "four" | "five" | "six" | "seven" | "eight" | "nine" | "oh")
}

/// If an n-gram (n = 1..=6) repeats four or more times in a row, keep one copy. Spoken digit
/// strings ("five five five five") are exempt.
pub fn collapse_loops(doc: &mut Doc<'_>) {
    let words: Vec<usize> = (0..doc.toks.len()).filter(|&i| doc.toks[i].kind == Kind::Word).collect();
    let m = words.len();
    if m < 4 {
        return;
    }
    let lc = |k: usize| &*doc.toks[words[k]].lc;
    let mut del = vec![false; doc.toks.len()];
    let mut any = false;
    let mut i = 0;
    while i < m {
        let mut best: Option<(usize, usize)> = None; // (n, reps)
        for n in 1..=6 {
            if i + n * 4 > m {
                break;
            }
            let mut reps = 1;
            while i + (reps + 1) * n <= m && (0..n).all(|k| lc(i + k) == lc(i + reps * n + k)) {
                reps += 1;
            }
            if reps >= 4 && !(0..n).all(|k| is_digit_word(lc(i + k))) && best.is_none_or(|(bn, br)| reps * n > bn * br) {
                best = Some((n, reps));
            }
        }
        match best {
            Some((n, reps)) => {
                for t in words[i + n - 1] + 1..=words[i + reps * n - 1] {
                    del[t] = true;
                }
                any = true;
                i += reps * n;
            }
            None => i += 1,
        }
    }
    if any {
        doc.remove(&del);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::cleanup::tokens::tokenize;

    #[test]
    fn strips_tags() {
        assert_eq!(strip_nonspeech("[BLANK_AUDIO]").trim(), "");
        assert_eq!(strip_nonspeech(" [blank_audio] ").trim(), "");
        assert_eq!(strip_nonspeech("hello [Music] world").split_whitespace().collect::<Vec<_>>(), ["hello", "world"]);
        assert_eq!(strip_nonspeech("(silence)").trim(), "");
        assert_eq!(strip_nonspeech("♪ la la la ♪ okay").trim(), "okay");
        assert_eq!(strip_nonspeech("a [ __ ] b").split_whitespace().collect::<Vec<_>>(), ["a", "b"]);
        assert_eq!(strip_nonspeech("*applause* thanks").trim(), "thanks");
        assert_eq!(strip_nonspeech("[NO_SPEECH]").trim(), "");
    }

    #[test]
    fn keeps_real_brackets() {
        for s in ["see (appendix B) for details", "arr[0] = 1", "a * b * c", "call me (maybe)", "[TODO] fix it", "(Music Man) is a film"] {
            assert_eq!(strip_nonspeech(s), s, "{s}");
        }
    }

    #[test]
    fn hallucination_phrases() {
        assert!(is_hallucination("Thank you for watching!"));
        assert!(is_hallucination("Thank you for watching. Please subscribe."));
        assert!(is_hallucination("Продолжение следует..."));
        assert!(is_hallucination("ご視聴ありがとうございました。"));
        assert!(!is_hallucination("Thank you."));
        assert!(!is_hallucination("Thank you for watching my kid play"));
        assert!(!is_hallucination(""));
        assert!(!is_hallucination("..."));
    }

    fn loops(s: &str) -> String {
        let mut d = tokenize(s);
        collapse_loops(&mut d);
        d.render()
    }

    #[test]
    fn collapses_loops_of_four_or_more() {
        assert_eq!(loops("the the the the cat"), "the cat");
        assert_eq!(loops("Thank you. Thank you. Thank you. Thank you."), "Thank you.");
        assert_eq!(loops("go left go left go left go left now"), "go left now");
        assert_eq!(loops("a b c d e f a b c d e f a b c d e f a b c d e f"), "a b c d e f");
        assert_eq!(loops("no no no no no no no no"), "no");
    }

    #[test]
    fn keeps_short_repeats_and_digit_runs() {
        for s in ["the the the cat", "very very very good", "five five five five five five", "0 0 0 0", "ha ha ha"] {
            assert_eq!(loops(s), s);
        }
    }
}
