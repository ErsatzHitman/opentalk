//! API-key field: a pure state machine over the DPAPI secret store, shown as page items.
//!
//! States: *stored* (masked value + Replace/Remove), *unset* or *replacing* (secret input +
//! Save, and Cancel when a key already exists), and *from environment* (`OPENTALK_SECRET_*` wins
//! over the stored value, so the page cannot change it and says where the key comes from).
//! Saving and removing are `PopupAction::SetSecret` (the daemon applies them at once, no `Config`
//! change); the field updates its masked view optimistically and re-reads the store on `refresh`.
use std::path::PathBuf;

use super::kit::{button, note};
use crate::daemon::tray::page::{BtnStyle, ConfirmId, Effect, Event, FieldKind, Item, ItemId, PopupAction, Row, Tone, Trailing};

/// Where the field reads secrets from (the real store, or a fake in tests).
pub trait SecretStore {
    /// Masked value of the stored secret (`None` when absent or blank).
    fn masked(&self, name: &str) -> Option<String>;
    /// Name of the environment variable that currently supplies the secret, if it is set.
    fn env_override(&self, name: &str) -> Option<String>;
}

/// The DPAPI store behind `core::secrets`.
pub struct SysSecrets;

impl SecretStore for SysSecrets {
    fn masked(&self, name: &str) -> Option<String> {
        std::panic::catch_unwind(|| crate::core::secrets::get(name))
            .ok()
            .flatten()
            .filter(|v| !v.trim().is_empty())
            .map(|v| crate::core::secrets::mask(&v))
    }

    fn env_override(&self, name: &str) -> Option<String> {
        let var = crate::core::secrets::env_var_name(name);
        std::env::var(&var).ok().filter(|v| !v.trim().is_empty()).map(|_| var)
    }
}

// Item id offsets from a field's base id.
pub const OFF_FIELD: ItemId = 0;
pub const OFF_SAVE: ItemId = 1;
pub const OFF_CANCEL: ItemId = 2;
pub const OFF_GET: ItemId = 3;
pub const OFF_ROW: ItemId = 4;
pub const OFF_REPLACE: ItemId = 5;
pub const OFF_REMOVE: ItemId = 6;
/// Ids a field occupies from its base.
pub const SPAN: ItemId = 8;

pub struct KeyField {
    pub name: &'static str,
    label: &'static str,
    /// Where to get a key (empty = no link).
    key_url: &'static str,
    /// Placeholder of the input.
    hint: &'static str,
    /// What the buttons name ("Save key", "Save client ID").
    noun: &'static str,
    /// Masked stored value.
    stored: Option<String>,
    env: Option<String>,
    /// Replacing an existing key.
    editing: bool,
    buf: String,
}

/// What an event did to the field.
#[derive(Debug, Default, PartialEq)]
pub struct KeyOutcome {
    pub effects: Vec<Effect>,
    /// A key was saved or removed (callers drop test results that used the old key).
    pub changed: bool,
}

impl KeyField {
    pub fn new(store: &dyn SecretStore, name: &'static str, label: &'static str, key_url: &'static str) -> KeyField {
        let mut f = KeyField { name, label, key_url, hint: "Paste your API key", noun: "key", stored: None, env: None, editing: false, buf: String::new() };
        f.refresh(store);
        f
    }

    pub fn with_hint(mut self, hint: &'static str) -> KeyField {
        self.hint = hint;
        self
    }

    /// Name the secret in button labels when a page has several ("client ID", "client secret").
    pub fn with_noun(mut self, noun: &'static str) -> KeyField {
        self.noun = noun;
        self
    }

    /// Re-read the store (page entered, or another page may have changed the key).
    pub fn refresh(&mut self, store: &dyn SecretStore) {
        self.stored = store.masked(self.name);
        self.env = store.env_override(self.name);
        if self.stored.is_none() {
            self.editing = false;
        }
    }

    /// A key is available (stored or from the environment).
    pub fn has_key(&self) -> bool {
        self.stored.is_some() || self.env.is_some()
    }

    /// Does `id` belong to this field (base `base`)?
    pub fn owns(base: ItemId, id: ItemId) -> bool {
        id >= base && id < base + SPAN
    }

    fn input_visible(&self) -> bool {
        self.env.is_none() && (self.stored.is_none() || self.editing)
    }

    pub fn items(&self, base: ItemId) -> Vec<Item> {
        let mut v = Vec::new();
        if let Some(var) = &self.env {
            v.push(Item::Row(Row {
                trailing: Trailing::Badge { text: "From environment".into(), tone: Tone::Info },
                secondary: self.stored.clone().unwrap_or_default(),
                ..Row::new(base + OFF_ROW, self.label, 44.0)
            }));
            v.push(note(&format!("Set by {var}. Remove the variable to manage this key here.")));
        } else if self.input_visible() {
            v.push(Item::Field {
                id: base + OFF_FIELD,
                label: self.label.into(),
                hint: self.hint.into(),
                value: self.buf.clone(),
                kind: FieldKind::Secret,
                error: None,
                max_chars: 0,
            });
            v.push(button(base + OFF_SAVE, &format!("Save {}", self.noun), BtnStyle::Primary, !self.buf.trim().is_empty()));
            if self.stored.is_some() {
                v.push(button(base + OFF_CANCEL, "Cancel", BtnStyle::Outline, true));
            }
            if !self.key_url.is_empty() {
                v.push(button(base + OFF_GET, "Get a key", BtnStyle::Link, true));
            }
        } else if let Some(m) = &self.stored {
            v.push(Item::Row(Row {
                trailing: Trailing::Badge { text: "Saved".into(), tone: Tone::Ok },
                secondary: m.clone(),
                ..Row::new(base + OFF_ROW, self.label, 44.0)
            }));
            v.push(button(base + OFF_REPLACE, &format!("Replace {}", self.noun), BtnStyle::Outline, true));
            v.push(button(base + OFF_REMOVE, &format!("Remove {}", self.noun), BtnStyle::Destructive, true));
        }
        v
    }

    /// The noun for toasts: "API key" for the plain case, else the page's own ("Client ID").
    fn title_noun(&self) -> String {
        if self.noun == "key" {
            return "API key".into();
        }
        let mut c = self.noun.chars();
        c.next().map(|f| f.to_uppercase().chain(c).collect()).unwrap_or_default()
    }

    /// Handle an event on one of this field's items.
    pub fn event(&mut self, base: ItemId, id: ItemId, ev: Event) -> KeyOutcome {
        let mut out = KeyOutcome::default();
        let Some(off) = id.checked_sub(base) else { return out };
        match (off, ev) {
            (OFF_FIELD, Event::Edited(t) | Event::Committed(t)) => self.buf = t,
            (OFF_SAVE, Event::Pressed) => {
                let value = self.buf.trim().to_string();
                if value.is_empty() {
                    return out;
                }
                self.stored = Some(crate::core::secrets::mask(&value));
                self.editing = false;
                self.buf.clear();
                out.effects = vec![Effect::Do(PopupAction::SetSecret { name: self.name.into(), value: Some(value) }), Effect::Toast(format!("{} saved", self.title_noun()))];
                out.changed = true;
            }
            (OFF_CANCEL, Event::Pressed) => {
                self.editing = false;
                self.buf.clear();
            }
            (OFF_GET, Event::Pressed) => out.effects = vec![Effect::OpenPath(PathBuf::from(self.key_url))],
            (OFF_ROW | OFF_REPLACE, Event::Activated | Event::Pressed) if self.env.is_none() => {
                self.editing = true;
                self.buf.clear();
                out.effects = vec![Effect::Focus(base + OFF_FIELD)];
            }
            (OFF_REMOVE, Event::Pressed) => {
                out.effects = vec![Effect::Confirm {
                    title: format!("Remove the {}?", self.noun),
                    body: format!("The saved {} is deleted from this PC. Features that use it stop working until you add it again.", self.noun),
                    action: "Remove".into(),
                    destructive: true,
                    on_confirm: ConfirmId::RemoveKey(base),
                }];
            }
            _ => {}
        }
        out
    }

    /// The user confirmed removal.
    pub fn remove(&mut self) -> KeyOutcome {
        self.stored = None;
        self.editing = false;
        self.buf.clear();
        KeyOutcome {
            effects: vec![Effect::Do(PopupAction::SetSecret { name: self.name.into(), value: None }), Effect::Toast(format!("{} removed", self.title_noun()))],
            changed: true,
        }
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::BTreeMap;

    /// In-memory store: `values` are masked already, `env` maps secret name -> variable.
    #[derive(Default)]
    pub struct Fake {
        pub values: RefCell<BTreeMap<String, String>>,
        pub env: RefCell<BTreeMap<String, String>>,
    }

    impl Fake {
        pub fn with(name: &str, masked: &str) -> Fake {
            let f = Fake::default();
            f.values.borrow_mut().insert(name.into(), masked.into());
            f
        }
    }

    impl SecretStore for Fake {
        fn masked(&self, name: &str) -> Option<String> {
            self.values.borrow().get(name).cloned()
        }
        fn env_override(&self, name: &str) -> Option<String> {
            self.env.borrow().get(name).cloned()
        }
    }

    fn ids(items: &[Item]) -> Vec<ItemId> {
        items.iter().filter_map(Item::id).collect()
    }

    #[test]
    fn unset_shows_input_save_disabled_and_link() {
        let f = KeyField::new(&Fake::default(), "openai", "API key", "https://example.com/keys");
        assert!(!f.has_key());
        let it = f.items(100);
        assert_eq!(ids(&it), vec![100, 101, 103], "field, save, get-key (no cancel without a stored key)");
        assert!(matches!(&it[0], Item::Field { kind: FieldKind::Secret, value, .. } if value.is_empty()));
        assert!(matches!(&it[1], Item::Button { enabled: false, style: BtnStyle::Primary, .. }));
    }

    #[test]
    fn typing_enables_save_and_save_stores_trimmed_key() {
        let mut f = KeyField::new(&Fake::default(), "openai", "API key", "");
        assert!(f.event(100, 100, Event::Edited("  sk-test-1234567890  ".into())).effects.is_empty());
        assert!(matches!(&f.items(100)[1], Item::Button { enabled: true, .. }));
        let out = f.event(100, 101, Event::Pressed);
        assert!(out.changed);
        assert_eq!(out.effects[0], Effect::Do(PopupAction::SetSecret { name: "openai".into(), value: Some("sk-test-1234567890".into()) }));
        assert!(matches!(out.effects[1], Effect::Toast(_)));
        assert!(f.has_key());
        let it = f.items(100);
        assert!(matches!(&it[0], Item::Row(r) if r.trailing == Trailing::Badge { text: "Saved".into(), tone: Tone::Ok } && !r.secondary.contains("1234567890")));
        assert_eq!(ids(&it), vec![104, 105, 106]);
    }

    #[test]
    fn blank_input_cannot_be_saved() {
        let mut f = KeyField::new(&Fake::default(), "openai", "API key", "");
        f.event(100, 100, Event::Edited("   ".into()));
        assert_eq!(f.event(100, 101, Event::Pressed), KeyOutcome::default());
        assert!(!f.has_key());
    }

    #[test]
    fn stored_key_replace_then_cancel_restores_the_masked_view() {
        let mut f = KeyField::new(&Fake::with("groq", "gsk_...abcd"), "groq", "API key", "https://x");
        assert!(f.has_key());
        assert_eq!(ids(&f.items(10)), vec![14, 15, 16]);
        let out = f.event(10, 15, Event::Pressed);
        assert_eq!(out.effects, vec![Effect::Focus(10)]);
        let it = f.items(10);
        assert_eq!(ids(&it), vec![10, 11, 12, 13], "input, save, cancel, get key");
        f.event(10, 10, Event::Edited("half typed".into()));
        let out = f.event(10, 12, Event::Pressed);
        assert!(!out.changed && out.effects.is_empty());
        assert_eq!(ids(&f.items(10)), vec![14, 15, 16]);
        // The half-typed text is gone: replacing again starts empty.
        f.event(10, 14, Event::Activated);
        assert!(matches!(&f.items(10)[0], Item::Field { value, .. } if value.is_empty()));
    }

    #[test]
    fn remove_asks_first_then_deletes() {
        let mut f = KeyField::new(&Fake::with("groq", "gsk_...abcd"), "groq", "API key", "");
        let out = f.event(10, 16, Event::Pressed);
        assert!(matches!(&out.effects[0], Effect::Confirm { destructive: true, on_confirm: ConfirmId::RemoveKey(10), .. }));
        assert!(f.has_key(), "nothing is removed before the confirmation");
        let out = f.remove();
        assert!(out.changed);
        assert_eq!(out.effects[0], Effect::Do(PopupAction::SetSecret { name: "groq".into(), value: None }));
        assert!(!f.has_key());
        assert_eq!(ids(&f.items(10)), vec![10, 11], "back to the empty input");
    }

    #[test]
    fn environment_override_is_read_only() {
        let store = Fake::with("openai", "sk-...zzzz");
        store.env.borrow_mut().insert("openai".into(), "OPENTALK_SECRET_OPENAI".into());
        let mut f = KeyField::new(&store, "openai", "API key", "https://x");
        assert!(f.has_key());
        let it = f.items(0);
        assert_eq!(ids(&it), vec![4], "just the status row");
        assert!(it.iter().any(|i| matches!(i, Item::Note(n) if n.contains("OPENTALK_SECRET_OPENAI"))));
        assert!(f.event(0, 4, Event::Activated).effects.is_empty(), "cannot start replacing");
        assert!(f.event(0, 5, Event::Pressed).effects.is_empty());
    }

    #[test]
    fn refresh_follows_the_store_and_drops_a_stale_edit() {
        let store = Fake::with("groq", "gsk_...abcd");
        let mut f = KeyField::new(&store, "groq", "API key", "");
        f.event(0, 5, Event::Pressed);
        store.values.borrow_mut().clear(); // removed elsewhere
        f.refresh(&store);
        assert!(!f.has_key());
        assert_eq!(ids(&f.items(0)), vec![0, 1]);
    }

    #[test]
    fn get_key_opens_the_provider_page() {
        let mut f = KeyField::new(&Fake::default(), "openai", "API key", "https://platform.openai.com/api-keys");
        let out = f.event(0, 3, Event::Pressed);
        assert_eq!(out.effects, vec![Effect::OpenPath(PathBuf::from("https://platform.openai.com/api-keys"))]);
    }

    #[test]
    fn owns_covers_exactly_the_span() {
        assert!(KeyField::owns(100, 100) && KeyField::owns(100, 107));
        assert!(!KeyField::owns(100, 99) && !KeyField::owns(100, 108));
    }

    #[test]
    fn toasts_name_the_secret() {
        let toast = |f: &mut KeyField| {
            f.event(100, 100, Event::Edited("abcdefghijkl".into()));
            match f.event(100, 101, Event::Pressed).effects.pop() {
                Some(Effect::Toast(t)) => t,
                other => panic!("{other:?}"),
            }
        };
        assert_eq!(toast(&mut KeyField::new(&Fake::default(), "openai", "API key", "")), "API key saved");
        assert_eq!(toast(&mut KeyField::new(&Fake::default(), "corti_client_id", "Client ID", "").with_noun("client ID")), "Client ID saved");
        assert_eq!(KeyField::new(&Fake::default(), "x", "Client secret", "").with_noun("client secret").remove().effects.pop(), Some(Effect::Toast("Client secret removed".into())));
    }
}
