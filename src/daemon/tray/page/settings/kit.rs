//! Small builders shared by the Settings pages, and (tests only) the text dump the pages are
//! reviewed through: every page's `items()` is rendered to plain text and compared with a file in
//! `snapshots/`, so wording, ordering and consistency can be read without a screenshot. Set
//! `OPENTALK_UPDATE_SNAPSHOTS=1` to rewrite the files after an intended change.
use crate::core::config::Config;
use crate::daemon::tray::page::{BtnStyle, Effect, Event, FieldKind, Item, ItemId};

pub fn group(s: &str) -> Item {
    Item::Group(s.into())
}

pub fn note(s: &str) -> Item {
    Item::Note(s.into())
}

pub fn toggle(id: ItemId, label: &str, desc: &str, on: bool) -> Item {
    Item::Toggle { id, label: label.into(), desc: desc.into(), on }
}

pub fn segmented(id: ItemId, label: &str, options: &[&str], sel: usize) -> Item {
    Item::Segmented { id, label: label.into(), options: options.iter().map(|s| s.to_string()).collect(), sel }
}

pub fn choice(id: ItemId, label: &str, value: &str) -> Item {
    Item::Choice { id, label: label.into(), value: value.into() }
}

pub fn radio(id: ItemId, label: &str, desc: &str, on: bool) -> Item {
    Item::Radio { id, label: label.into(), desc: desc.into(), on }
}

pub fn button(id: ItemId, label: &str, style: BtnStyle, enabled: bool) -> Item {
    Item::Button { id, label: label.into(), style, enabled }
}

/// A plain single-line text field.
pub fn text_field(id: ItemId, label: &str, hint: &str, value: &str) -> Item {
    Item::Field { id, label: label.into(), hint: hint.into(), value: value.into(), kind: FieldKind::Text, error: None, max_chars: 0 }
}

/// A boolean setting bound to a toggle: one table drives both `items()` and `event()`.
pub struct ToggleDef {
    pub id: ItemId,
    pub label: &'static str,
    pub desc: &'static str,
    pub get: fn(&Config) -> bool,
    pub set: fn(&mut Config, bool),
}

impl ToggleDef {
    pub fn item(&self, cfg: &Config) -> Item {
        toggle(self.id, self.label, self.desc, (self.get)(cfg))
    }
}

/// Apply a `Toggled` event to the setting of the matching entry of `defs`. True when it did.
pub fn apply_toggle(defs: &[ToggleDef], id: ItemId, ev: &Event, cfg: &mut Config) -> bool {
    match (defs.iter().find(|d| d.id == id), ev) {
        (Some(d), Event::Toggled(v)) => {
            (d.set)(cfg, *v);
            true
        }
        _ => false,
    }
}

/// The effects of an event that changed `cx.cfg`.
pub fn saved() -> Vec<Effect> {
    vec![Effect::Saved]
}

/// Plain-text rendering of a page, one line per item (tests only).
#[cfg(test)]
pub fn dump(title: &str, items: &[Item]) -> String {
    use crate::daemon::tray::page::{Trailing, Tone};
    let tone = |t: Tone| match t {
        Tone::Info => "info",
        Tone::Warn => "warn",
        Tone::Ok => "ok",
        Tone::Err => "error",
    };
    let mut out = format!("# {title}\n");
    for it in items {
        let line = match it {
            Item::Group(s) => format!("\n## {s}"),
            Item::Note(s) => format!("  note: {s}"),
            Item::Text(s) => format!("  text: {s}"),
            Item::Banner { tone: t, text } => format!("  banner[{}]: {text}", tone(*t)),
            Item::Toggle { id, label, desc, on } => format!("  [{}] #{id} {label}{}", if *on { "x" } else { " " }, tail(desc)),
            Item::Segmented { id, label, options, sel } => {
                let o: Vec<String> = options.iter().enumerate().map(|(i, s)| if i == *sel { format!("({s})") } else { s.clone() }).collect();
                format!("  seg #{id} {label}: {}", o.join(" | "))
            }
            Item::Choice { id, label, value } => format!("  choice #{id} {label} > {value}"),
            Item::Radio { id, label, desc, on } => format!("  ({}) #{id} {label}{}", if *on { "*" } else { " " }, tail(desc)),
            Item::Field { id, label, hint, value, kind, error, max_chars } => {
                let k = match kind {
                    FieldKind::Text => "text".to_string(),
                    FieldKind::Secret => "secret".to_string(),
                    FieldKind::Multiline { rows } => format!("multiline x{rows}"),
                    FieldKind::Hotkey => "hotkey".to_string(),
                };
                let shown = if matches!(kind, FieldKind::Secret) && !value.is_empty() { "*".repeat(value.chars().count().min(12)) } else { value.clone() };
                let mut s = format!("  field #{id} {label} [{k}] = \"{shown}\" (hint \"{hint}\"");
                if *max_chars > 0 {
                    s.push_str(&format!(", max {max_chars}"));
                }
                s.push(')');
                if let Some(e) = error {
                    s.push_str(&format!("\n    error: {e}"));
                }
                s
            }
            Item::Button { id, label, style, enabled } => format!("  button #{id} {label} <{style:?}>{}", if *enabled { "" } else { " (disabled)" }),
            Item::Result { ok, badge, text } => format!("  result[{}] {badge}: {text}", if *ok { "ok" } else { "error" }),
            Item::Row(r) => {
                let trailing = match &r.trailing {
                    Trailing::None => String::new(),
                    Trailing::Chevron => " >".to_string(),
                    Trailing::Value(v) => format!(" {v} >"),
                    Trailing::Badge { text, tone: t } => format!(" [{text}:{}]", tone(*t)),
                    Trailing::Delete => " (x)".to_string(),
                    Trailing::Copy => " (copy)".to_string(),
                };
                let sec = if r.secondary.is_empty() { String::new() } else { format!(" -- {}", r.secondary) };
                format!("  row #{} {}{}{}", r.id, r.primary, trailing, sec)
            }
            Item::Divider => "  ---".to_string(),
            Item::Space(_) => continue,
        };
        out.push_str(&line);
        out.push('\n');
    }
    out
}

#[cfg(test)]
fn tail(desc: &str) -> String {
    if desc.is_empty() { String::new() } else { format!(" -- {desc}") }
}

/// Compare `actual` with the committed snapshot (tests only).
#[cfg(test)]
pub fn check_snapshot(name: &str, actual: &str, expected: &str) {
    if std::env::var("OPENTALK_UPDATE_SNAPSHOTS").is_ok_and(|v| v == "1") {
        let path = format!("{}/src/daemon/tray/page/settings/snapshots/{name}.txt", env!("CARGO_MANIFEST_DIR"));
        std::fs::write(&path, actual).unwrap_or_else(|e| panic!("{path}: {e}"));
        return;
    }
    assert_eq!(actual.trim_end(), expected.trim_end(), "snapshot {name} differs (OPENTALK_UPDATE_SNAPSHOTS=1 rewrites it)");
}

/// `check_snapshot` against `snapshots/<name>.txt` embedded at compile time.
#[cfg(test)]
macro_rules! snapshot {
    ($name:expr, $actual:expr) => {
        $crate::daemon::tray::page::settings::kit::check_snapshot($name, &$actual, include_str!(concat!("snapshots/", $name, ".txt")))
    };
}
#[cfg(test)]
pub(crate) use snapshot;
