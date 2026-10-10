//! The text editor behind the popup's fields. A native EDIT control cannot live in the layered
//! popup (child windows of an `UpdateLayeredWindow` window are not composited), so the editing
//! model is pure and the picture is drawn by `draw`. `Editor` keeps the text as chars (a caret can
//! never split a surrogate pair), a caret and an optional selection anchor, and supports single-line
//! (optionally secret) and soft-wrapped multi-line text:
//! insert / backspace / delete, caret and word moves with Shift selection, select all, clipboard
//! (copy and cut are refused for secret fields), double-click word selection and click-to-caret
//! from measured prefix widths. All text measuring goes through [`TextMetrics`].

use super::FieldKind;
use super::metrics::TextMetrics;

/// Line height of multi-line text and of the hint / value text in a box.
pub const LINE_H: f32 = 17.0;
/// Text size inside fields.
pub const FIELD_PX: f32 = 13.0;
pub const FIELD_WEIGHT: i32 = 400;
/// What a secret field shows per char.
pub const BULLET: char = '\u{2022}';

/// One visual line: chars `[start, end)` of the text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Line {
    pub start: usize,
    pub end: usize,
    /// Ended by wrapping (not by a newline or the end of text).
    pub soft: bool,
    /// Furthest caret position on this line (a soft line's hanging spaces are excluded).
    pub caret_end: usize,
}

/// Measured geometry of an editor's lines.
#[derive(Clone, Debug, PartialEq)]
pub struct Vis {
    pub lines: Vec<Line>,
    /// `pre[i][k]` = x of char `lines[i].start + k` relative to the line's left edge.
    pub pre: Vec<Vec<f32>>,
}

impl Vis {
    /// Visual line holding caret position `p`: the last line starting at or before it (so a caret
    /// at a soft wrap shows at the start of the next line).
    pub fn line_of(&self, p: usize) -> usize {
        self.lines.partition_point(|l| l.start <= p).saturating_sub(1)
    }

    /// X of caret position `p`, relative to its line's left edge.
    pub fn x_of(&self, p: usize) -> f32 {
        let i = self.line_of(p);
        let l = self.lines[i];
        let k = p.clamp(l.start, l.end) - l.start;
        self.pre[i].get(k).copied().unwrap_or(0.0)
    }

    /// Caret position on line `i` nearest to x (relative to the line's left edge).
    pub fn pos_at(&self, i: usize, x: f32) -> usize {
        let i = i.min(self.lines.len() - 1);
        let l = self.lines[i];
        let pre = &self.pre[i];
        let mut best = l.start;
        let mut best_d = f32::MAX;
        for p in l.start..=l.caret_end {
            let d = (pre.get(p - l.start).copied().unwrap_or(0.0) - x).abs();
            if d < best_d {
                best = p;
                best_d = d;
            }
        }
        best
    }
}

/// Caret moves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NavKey {
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
}

/// Editing keys the page maps from the keyboard (printable text comes through `insert`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EditKey {
    Nav(NavKey),
    Backspace,
    Delete,
    SelectAll,
    Copy,
    Cut,
    Paste,
    /// Enter: commits a single-line field, inserts a newline in a multi-line one.
    Enter,
}

/// What an editing key did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EditResult {
    /// The text changed (send `Edited`).
    pub changed: bool,
    /// The caret or selection changed (redraw).
    pub moved: bool,
    /// Text for the clipboard (copy / cut).
    pub clip: Option<String>,
    /// Enter in a single-line field: commit.
    pub commit: bool,
}

#[derive(Clone, Debug)]
pub struct Editor {
    chars: Vec<char>,
    /// Caret as a char index (`0..=len`).
    pub caret: usize,
    /// Selection anchor; the selection spans anchor..caret.
    pub anchor: Option<usize>,
    pub multiline: bool,
    pub secret: bool,
    /// 0 = unlimited.
    pub max_chars: usize,
    /// Wrap width / box inner width (logical px).
    pub inner_w: f32,
    pub px: f32,
    pub weight: i32,
    /// Horizontal scroll of a single-line field.
    pub scroll_x: f32,
    /// Vertical scroll of a multi-line field.
    pub scroll_y: f32,
    /// Sticky x for runs of Up / Down.
    goal_x: Option<f32>,
}

fn class(c: char) -> u8 {
    if c.is_whitespace() {
        0
    } else if c.is_alphanumeric() || c == '_' {
        1
    } else {
        2
    }
}

/// Start of the word before `i` (skips spaces, then one run of one class).
pub fn word_left(chars: &[char], mut i: usize) -> usize {
    while i > 0 && class(chars[i - 1]) == 0 {
        i -= 1;
    }
    if i > 0 {
        let c = class(chars[i - 1]);
        while i > 0 && class(chars[i - 1]) == c {
            i -= 1;
        }
    }
    i
}

/// Start of the next word after `i` (skips the current run, then spaces).
pub fn word_right(chars: &[char], mut i: usize) -> usize {
    if i < chars.len() {
        let c = class(chars[i]);
        while i < chars.len() && class(chars[i]) == c {
            i += 1;
        }
    }
    while i < chars.len() && class(chars[i]) == 0 {
        i += 1;
    }
    i
}

/// Split text into visual lines of at most `max_w` (greedy at spaces; a word wider than a line is
/// broken by char). `prefix` measures one paragraph.
pub fn wrap_chars(chars: &[char], max_w: f32, prefix: &dyn Fn(&str) -> Vec<f32>) -> (Vec<Line>, Vec<Vec<f32>>) {
    let mut lines = Vec::new();
    let mut pres = Vec::new();
    let mut base = 0usize;
    let n_all = chars.len();
    loop {
        let end = chars[base..].iter().position(|&c| c == '\n').map_or(n_all, |p| base + p);
        let para = &chars[base..end];
        let s: String = para.iter().collect();
        let pre = prefix(&s);
        let n = para.len();
        let mut start = 0usize;
        let mut last_break: Option<usize> = None;
        let mut i = 0usize;
        while i < n {
            let c = para[i];
            let w = pre[i + 1] - pre[start];
            if c != ' ' && w > max_w && i > start {
                let brk = match last_break {
                    Some(b) if b > start => b,
                    _ => i,
                };
                let hang = para[start..brk].iter().rev().take_while(|&&c| c == ' ').count();
                lines.push(Line { start: base + start, end: base + brk, soft: true, caret_end: base + (brk - hang).max(start) });
                pres.push(pre[start..=brk].iter().map(|x| x - pre[start]).collect());
                start = brk;
                last_break = None;
                i = start;
                continue;
            }
            if c == ' ' {
                last_break = Some(i + 1);
            }
            i += 1;
        }
        lines.push(Line { start: base + start, end: base + n, soft: false, caret_end: base + n });
        pres.push(pre[start..=n].iter().map(|x| x - pre[start]).collect());
        if end >= n_all {
            break;
        }
        base = end + 1;
    }
    (lines, pres)
}

impl Editor {
    pub fn new(text: &str, kind: FieldKind, max_chars: usize, inner_w: f32) -> Editor {
        let multiline = matches!(kind, FieldKind::Multiline { .. });
        let mut e = Editor {
            chars: Vec::new(),
            caret: 0,
            anchor: None,
            multiline,
            secret: matches!(kind, FieldKind::Secret),
            max_chars,
            inner_w,
            px: FIELD_PX,
            weight: FIELD_WEIGHT,
            scroll_x: 0.0,
            scroll_y: 0.0,
            goal_x: None,
        };
        e.set_text(text);
        e
    }

    pub fn text(&self) -> String {
        self.chars.iter().collect()
    }

    pub fn len(&self) -> usize {
        self.chars.len()
    }

    pub fn is_empty(&self) -> bool {
        self.chars.is_empty()
    }

    pub fn chars(&self) -> &[char] {
        &self.chars
    }

    /// Replace everything; the caret goes to the end.
    pub fn set_text(&mut self, text: &str) {
        let mut v = self.sanitize(text);
        if self.max_chars > 0 {
            v.truncate(self.max_chars);
        }
        self.chars = v;
        self.caret = self.chars.len();
        self.anchor = None;
        self.goal_x = None;
    }

    /// Clean text for this field: CR / CRLF become newlines; newlines and tabs become spaces in a
    /// single-line field; other control chars are dropped.
    fn sanitize(&self, s: &str) -> Vec<char> {
        let mut out = Vec::with_capacity(s.len());
        let mut it = s.chars().peekable();
        while let Some(c) = it.next() {
            let c = if c == '\r' {
                if it.peek() == Some(&'\n') {
                    it.next();
                }
                '\n'
            } else {
                c
            };
            match c {
                '\n' if self.multiline => out.push('\n'),
                '\n' | '\t' => out.push(' '),
                c if c.is_control() => {}
                c => out.push(c),
            }
        }
        out
    }

    pub fn selection(&self) -> Option<(usize, usize)> {
        let a = self.anchor?;
        (a != self.caret).then(|| (a.min(self.caret), a.max(self.caret)))
    }

    pub fn selected_text(&self) -> String {
        self.selection().map(|(a, b)| self.chars[a..b].iter().collect()).unwrap_or_default()
    }

    pub fn select_all(&mut self) -> bool {
        let moved = self.anchor != Some(0) || self.caret != self.chars.len();
        self.anchor = Some(0);
        self.caret = self.chars.len();
        self.goal_x = None;
        moved && !self.chars.is_empty()
    }

    fn delete_selection(&mut self) -> bool {
        let Some((a, b)) = self.selection() else {
            self.anchor = None;
            return false;
        };
        self.chars.drain(a..b);
        self.caret = a;
        self.anchor = None;
        true
    }

    /// Insert text at the caret (replacing the selection). True when the text changed.
    pub fn insert(&mut self, s: &str) -> bool {
        let new = self.sanitize(s);
        if new.is_empty() {
            return false;
        }
        let sel_len = self.selection().map_or(0, |(a, b)| b - a);
        let room = if self.max_chars == 0 { usize::MAX } else { self.max_chars.saturating_sub(self.chars.len() - sel_len) };
        if room == 0 {
            return false;
        }
        let take = new.len().min(room);
        self.delete_selection();
        self.chars.splice(self.caret..self.caret, new[..take].iter().copied());
        self.caret += take;
        self.goal_x = None;
        true
    }

    pub fn backspace(&mut self, ctrl: bool) -> bool {
        if self.delete_selection() {
            return true;
        }
        if self.caret == 0 {
            return false;
        }
        let from = if ctrl { word_left(&self.chars, self.caret) } else { self.caret - 1 };
        self.chars.drain(from..self.caret);
        self.caret = from;
        self.goal_x = None;
        true
    }

    pub fn delete(&mut self, ctrl: bool) -> bool {
        if self.delete_selection() {
            return true;
        }
        if self.caret >= self.chars.len() {
            return false;
        }
        let to = if ctrl { word_right(&self.chars, self.caret) } else { self.caret + 1 };
        self.chars.drain(self.caret..to);
        self.goal_x = None;
        true
    }

    /// Measured lines of the current text.
    pub fn vis(&self, m: &dyn TextMetrics) -> Vis {
        let secret = self.secret;
        let (px, weight) = (self.px, self.weight);
        let prefix = |s: &str| {
            if secret {
                let b: String = s.chars().map(|_| BULLET).collect();
                m.prefix(&b, px, weight)
            } else {
                m.prefix(s, px, weight)
            }
        };
        if !self.multiline {
            let s: String = self.chars.iter().collect();
            let pre = prefix(&s);
            let n = self.chars.len();
            return Vis { lines: vec![Line { start: 0, end: n, soft: false, caret_end: n }], pre: vec![pre] };
        }
        let (lines, pre) = wrap_chars(&self.chars, self.inner_w, &prefix);
        Vis { lines, pre }
    }

    /// Move the caret to `p`; with `extend` the anchor stays (or is set), else the selection clears.
    pub fn move_to(&mut self, p: usize, extend: bool) -> bool {
        let p = p.min(self.chars.len());
        let before = (self.caret, self.anchor);
        if extend {
            if self.anchor.is_none() {
                self.anchor = Some(self.caret);
            }
        } else {
            self.anchor = None;
        }
        self.caret = p;
        if self.anchor == Some(self.caret) {
            self.anchor = None;
        }
        (self.caret, self.anchor) != before
    }

    /// Caret key. Without Shift a non-empty selection collapses to its edge first (Left / Right).
    pub fn nav(&mut self, k: NavKey, shift: bool, ctrl: bool, m: &dyn TextMetrics) -> bool {
        let sticky = matches!(k, NavKey::Up | NavKey::Down);
        let goal = if sticky { self.goal_x } else { None };
        let moved = match k {
            NavKey::Left | NavKey::Right => {
                let left = k == NavKey::Left;
                if let (Some((a, b)), false) = (self.selection(), shift) {
                    self.move_to(if left { a } else { b }, false)
                } else {
                    let to = match (left, ctrl) {
                        (true, true) => word_left(&self.chars, self.caret),
                        (true, false) => self.caret.saturating_sub(1),
                        (false, true) => word_right(&self.chars, self.caret),
                        (false, false) => (self.caret + 1).min(self.chars.len()),
                    };
                    self.move_to(to, shift)
                }
            }
            NavKey::Home | NavKey::End => {
                let end = k == NavKey::End;
                let to = if ctrl || !self.multiline {
                    if end { self.chars.len() } else { 0 }
                } else {
                    let v = self.vis(m);
                    let l = v.lines[v.line_of(self.caret)];
                    if end { l.caret_end } else { l.start }
                };
                self.move_to(to, shift)
            }
            NavKey::Up | NavKey::Down => {
                if !self.multiline {
                    // A single line: Up / Down jump to the ends, like a one-line text box.
                    let to = if k == NavKey::Up { 0 } else { self.chars.len() };
                    self.move_to(to, shift)
                } else {
                    let v = self.vis(m);
                    let li = v.line_of(self.caret);
                    let x = goal.unwrap_or_else(|| v.x_of(self.caret));
                    let target = if k == NavKey::Up { li.checked_sub(1) } else { (li + 1 < v.lines.len()).then_some(li + 1) };
                    let moved = match target {
                        Some(t) => self.move_to(v.pos_at(t, x), shift),
                        None => self.move_to(if k == NavKey::Up { 0 } else { self.chars.len() }, shift),
                    };
                    self.goal_x = Some(x);
                    moved
                }
            }
        };
        if !sticky {
            self.goal_x = None;
        }
        moved
    }

    /// Apply an editing key. `paste` is the clipboard text for `EditKey::Paste`.
    pub fn apply(&mut self, k: EditKey, shift: bool, ctrl: bool, m: &dyn TextMetrics, paste: Option<&str>) -> EditResult {
        let mut r = EditResult::default();
        match k {
            EditKey::Nav(n) => r.moved = self.nav(n, shift, ctrl, m),
            EditKey::Backspace => r.changed = self.backspace(ctrl),
            EditKey::Delete => r.changed = self.delete(ctrl),
            EditKey::SelectAll => r.moved = self.select_all(),
            EditKey::Copy => {
                if !self.secret && self.selection().is_some() {
                    r.clip = Some(self.selected_text());
                }
            }
            EditKey::Cut => {
                if !self.secret && self.selection().is_some() {
                    r.clip = Some(self.selected_text());
                    r.changed = self.delete_selection();
                }
            }
            EditKey::Paste => {
                if let Some(t) = paste {
                    r.changed = self.insert(t);
                }
            }
            EditKey::Enter => {
                if self.multiline {
                    r.changed = self.insert("\n");
                } else {
                    r.commit = true;
                }
            }
        }
        r.moved |= r.changed;
        r
    }

    /// A click at `x` (relative to the text origin, scroll already removed) on visual line `line`.
    pub fn click(&mut self, line: usize, x: f32, extend: bool, m: &dyn TextMetrics) -> bool {
        let v = self.vis(m);
        let p = v.pos_at(line, x);
        self.goal_x = None;
        self.move_to(p, extend)
    }

    /// Double-click: select the word (or the run of spaces) around char `p`.
    pub fn select_word_at(&mut self, p: usize) {
        if self.chars.is_empty() {
            return;
        }
        let i = p.min(self.chars.len() - 1);
        let c = class(self.chars[i]);
        let mut a = i;
        while a > 0 && class(self.chars[a - 1]) == c {
            a -= 1;
        }
        let mut b = i + 1;
        while b < self.chars.len() && class(self.chars[b]) == c {
            b += 1;
        }
        self.anchor = Some(a);
        self.caret = b;
        self.goal_x = None;
    }

    /// Keep the caret inside a box of `view_w` x `view_h` (inner size) by adjusting the scroll.
    pub fn follow_caret(&mut self, m: &dyn TextMetrics, view_w: f32, view_h: f32) {
        let v = self.vis(m);
        if self.multiline {
            let li = v.line_of(self.caret);
            let (top, bottom) = (li as f32 * LINE_H, (li + 1) as f32 * LINE_H);
            if top < self.scroll_y {
                self.scroll_y = top;
            } else if bottom > self.scroll_y + view_h {
                self.scroll_y = bottom - view_h;
            }
            let total = v.lines.len() as f32 * LINE_H;
            self.scroll_y = self.scroll_y.min((total - view_h).max(0.0)).max(0.0);
            self.scroll_x = 0.0;
        } else {
            let x = v.x_of(self.caret);
            let total = v.pre[0].last().copied().unwrap_or(0.0);
            // Leave room for the caret bar at the right edge.
            let w = (view_w - 2.0).max(1.0);
            if x < self.scroll_x {
                self.scroll_x = x;
            } else if x > self.scroll_x + w {
                self.scroll_x = x - w;
            }
            // Never scroll past the end when text was deleted.
            self.scroll_x = self.scroll_x.min((total + 2.0 - view_w).max(0.0)).max(0.0);
            self.scroll_y = 0.0;
        }
    }
}

/// Combines UTF-16 units from `WM_CHAR` into chars (a surrogate pair arrives as two messages).
#[derive(Clone, Copy, Debug, Default)]
pub struct Utf16Acc {
    high: Option<u16>,
}

impl Utf16Acc {
    /// Feed one unit; returns the completed char. A stray low surrogate or an unpaired high one is
    /// dropped (U+FFFD would pollute the field).
    pub fn push(&mut self, unit: u16) -> Option<char> {
        match unit {
            0xD800..=0xDBFF => {
                self.high = Some(unit);
                None
            }
            0xDC00..=0xDFFF => {
                let high = self.high.take()?;
                char::decode_utf16([high, unit]).next().and_then(Result::ok)
            }
            _ => {
                self.high = None;
                char::from_u32(unit as u32)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::tray::page::metrics::FixedMetrics;

    const M: FixedMetrics = FixedMetrics;
    /// Width of one char at the field size.
    const CW: f32 = FIELD_PX * FixedMetrics::EM;

    fn single(t: &str) -> Editor {
        Editor::new(t, FieldKind::Text, 0, 300.0)
    }

    fn multi(t: &str, w: f32) -> Editor {
        Editor::new(t, FieldKind::Multiline { rows: 5 }, 0, w)
    }

    fn at(e: &mut Editor, caret: usize) {
        e.caret = caret;
        e.anchor = None;
    }

    #[test]
    fn insert_backspace_delete_at_the_caret() {
        let mut e = single("hello");
        assert_eq!(e.caret, 5, "a new editor puts the caret at the end");
        assert!(e.insert("!"));
        assert_eq!(e.text(), "hello!");
        at(&mut e, 0);
        assert!(!e.backspace(false), "nothing before the start");
        assert!(e.delete(false));
        assert_eq!(e.text(), "ello!");
        at(&mut e, 2);
        assert!(e.insert("XY"));
        assert_eq!((e.text().as_str(), e.caret), ("elXYlo!", 4));
        assert!(e.backspace(false));
        assert_eq!((e.text().as_str(), e.caret), ("elXlo!", 3));
        at(&mut e, 6);
        assert!(!e.delete(false), "nothing after the end");
        assert!(!e.insert(""));
    }

    #[test]
    fn selection_is_replaced_and_deleted() {
        let mut e = single("hello world");
        e.anchor = Some(6);
        e.caret = 11;
        assert_eq!(e.selected_text(), "world");
        assert!(e.insert("there"));
        assert_eq!(e.text(), "hello there");
        assert_eq!((e.caret, e.anchor), (11, None));
        e.anchor = Some(0);
        e.caret = 6;
        assert!(e.backspace(false));
        assert_eq!((e.text().as_str(), e.caret), ("there", 0));
        e.select_all();
        assert!(e.delete(false));
        assert_eq!(e.text(), "");
        assert!(!e.select_all(), "nothing to select in an empty field");
    }

    #[test]
    fn home_end_and_arrows() {
        let mut e = single("abc def");
        at(&mut e, 3);
        assert!(e.nav(NavKey::Home, false, false, &M));
        assert_eq!(e.caret, 0);
        assert!(!e.nav(NavKey::Left, false, false, &M), "at the start already");
        assert!(e.nav(NavKey::End, false, false, &M));
        assert_eq!(e.caret, 7);
        assert!(!e.nav(NavKey::Right, false, false, &M));
        assert!(e.nav(NavKey::Left, false, false, &M));
        assert_eq!(e.caret, 6);
        // Up / Down in one line jump to the ends.
        e.nav(NavKey::Up, false, false, &M);
        assert_eq!(e.caret, 0);
        e.nav(NavKey::Down, false, false, &M);
        assert_eq!(e.caret, 7);
    }

    #[test]
    fn word_moves_follow_windows_conventions() {
        let t: Vec<char> = "foo  bar.baz qux".chars().collect();
        // Ctrl+Right lands on the start of the next word.
        assert_eq!(word_right(&t, 0), 5);
        assert_eq!(word_right(&t, 5), 8, "punctuation is its own word");
        assert_eq!(word_right(&t, 8), 9);
        assert_eq!(word_right(&t, 9), 13);
        assert_eq!(word_right(&t, 13), 16);
        assert_eq!(word_right(&t, 16), 16);
        // Ctrl+Left lands on the start of the word before.
        assert_eq!(word_left(&t, 16), 13);
        assert_eq!(word_left(&t, 13), 9);
        assert_eq!(word_left(&t, 9), 8);
        assert_eq!(word_left(&t, 8), 5);
        assert_eq!(word_left(&t, 5), 0);
        assert_eq!(word_left(&t, 7), 5, "from the middle of a word");
        assert_eq!(word_left(&t, 0), 0);
        let mut e = single("foo bar");
        e.nav(NavKey::Left, false, true, &M);
        assert_eq!(e.caret, 4);
        e.backspace(true);
        assert_eq!(e.text(), "bar", "Ctrl+Backspace deletes the word before");
        at(&mut e, 0);
        e.delete(true);
        assert_eq!(e.text(), "", "Ctrl+Delete deletes to the next word");
    }

    #[test]
    fn shift_extends_the_selection_and_plain_moves_collapse_it() {
        let mut e = single("hello world");
        at(&mut e, 0);
        e.nav(NavKey::Right, true, false, &M);
        e.nav(NavKey::Right, true, false, &M);
        assert_eq!(e.selection(), Some((0, 2)));
        e.nav(NavKey::Right, true, true, &M);
        assert_eq!(e.selection(), Some((0, 6)), "Ctrl+Shift+Right selects by word");
        e.nav(NavKey::Left, true, false, &M);
        assert_eq!(e.selection(), Some((0, 5)));
        // Plain Left collapses to the selection's left edge, plain Right to its right edge.
        e.nav(NavKey::Left, false, false, &M);
        assert_eq!((e.caret, e.selection()), (0, None));
        e.anchor = Some(2);
        e.caret = 7;
        e.nav(NavKey::Right, false, false, &M);
        assert_eq!((e.caret, e.selection()), (7, None));
        // Shift+Home / Shift+End select to the ends.
        at(&mut e, 4);
        e.nav(NavKey::Home, true, false, &M);
        assert_eq!(e.selection(), Some((0, 4)));
        at(&mut e, 4);
        e.nav(NavKey::End, true, false, &M);
        assert_eq!(e.selection(), Some((4, 11)));
        // Moving back onto the anchor drops the selection.
        at(&mut e, 4);
        e.nav(NavKey::Right, true, false, &M);
        e.nav(NavKey::Left, true, false, &M);
        assert_eq!(e.selection(), None);
    }

    #[test]
    fn select_all_and_clipboard_ops() {
        let mut e = single("secret text");
        let m = &M;
        assert!(e.apply(EditKey::SelectAll, false, true, m, None).moved);
        let r = e.apply(EditKey::Copy, false, true, m, None);
        assert_eq!(r.clip.as_deref(), Some("secret text"));
        assert!(!r.changed);
        let r = e.apply(EditKey::Cut, false, true, m, None);
        assert_eq!(r.clip.as_deref(), Some("secret text"));
        assert!(r.changed && e.text().is_empty());
        let r = e.apply(EditKey::Paste, false, true, m, Some("pasted"));
        assert!(r.changed && r.moved);
        assert_eq!(e.text(), "pasted");
        assert!(!e.apply(EditKey::Paste, false, true, m, None).changed, "an empty clipboard pastes nothing");
        // Nothing selected: copy and cut do nothing.
        let r = e.apply(EditKey::Copy, false, true, m, None);
        assert_eq!(r.clip, None);
        assert!(!e.apply(EditKey::Cut, false, true, m, None).changed);
    }

    #[test]
    fn secret_fields_refuse_copy_and_cut_but_edit_normally() {
        let mut e = Editor::new("sk-12345", FieldKind::Secret, 0, 300.0);
        e.select_all();
        assert_eq!(e.apply(EditKey::Copy, false, true, &M, None).clip, None);
        let r = e.apply(EditKey::Cut, false, true, &M, None);
        assert_eq!(r.clip, None);
        assert!(!r.changed);
        assert_eq!(e.text(), "sk-12345", "a refused cut leaves the text alone");
        assert!(e.insert("x"), "typing replaces the selection");
        assert_eq!(e.text(), "x");
        // Its geometry is measured on bullets: as wide as the text has chars.
        let e = Editor::new("abc", FieldKind::Secret, 0, 300.0);
        let v = e.vis(&M);
        assert_eq!(v.pre[0].len(), 4);
        assert!((v.pre[0][3] - 3.0 * CW).abs() < 1e-3);
    }

    #[test]
    fn pasted_text_is_cleaned_for_the_field() {
        let mut e = single("");
        e.insert("line one\r\nline two\nline\tthree\u{7}\u{0}end");
        assert_eq!(e.text(), "line one line two line threeend");
        let mut m = multi("", 300.0);
        m.insert("a\r\nb\rc\nd\te\u{7}");
        assert_eq!(m.text(), "a\nb\nc\nd e", "newlines survive in a text area, CR and CRLF become one newline");
        // Typed characters are filtered the same way.
        let mut e = single("");
        assert!(!e.insert("\u{1}"), "a control char is not text");
        assert!(e.insert("\n"), "a typed newline becomes a space in one line");
        assert_eq!(e.text(), " ");
    }

    #[test]
    fn surrogate_pairs_are_one_char() {
        let mut e = single("a\u{1F600}b");
        assert_eq!(e.len(), 3);
        at(&mut e, 2);
        assert!(e.backspace(false));
        assert_eq!(e.text(), "ab", "one backspace removes the whole emoji");
        e.insert("\u{1F600}");
        e.nav(NavKey::Left, false, false, &M);
        assert_eq!(e.caret, 1, "the caret steps over an emoji in one move");
        e.nav(NavKey::Right, false, false, &M);
        assert_eq!(e.caret, 2);
        e.delete(false);
        assert_eq!(e.text(), "a\u{1F600}");
    }

    #[test]
    fn utf16_accumulator_joins_pairs_and_drops_strays() {
        let mut a = Utf16Acc::default();
        assert_eq!(a.push('x' as u16), Some('x'));
        let units: Vec<u16> = "\u{1F600}".encode_utf16().collect();
        assert_eq!(a.push(units[0]), None);
        assert_eq!(a.push(units[1]), Some('\u{1F600}'));
        assert_eq!(a.push(units[1]), None, "a low surrogate without its high half is dropped");
        a.push(units[0]);
        assert_eq!(a.push('y' as u16), Some('y'), "an unpaired high surrogate is dropped by the next char");
        assert_eq!(a.push(0x00e9), Some('\u{e9}'));
    }

    #[test]
    fn max_chars_limits_typing_and_pasting() {
        let mut e = Editor::new("abc", FieldKind::Text, 5, 300.0);
        assert!(e.insert("de"));
        assert!(!e.insert("f"), "full");
        assert_eq!(e.text(), "abcde");
        e.anchor = Some(0);
        e.caret = 3;
        assert!(e.insert("XYZW"), "a selection frees room");
        assert_eq!(e.text(), "XYZde");
        let mut p = Editor::new("", FieldKind::Text, 4, 300.0);
        assert!(p.insert("abcdefgh"));
        assert_eq!(p.text(), "abcd", "a long paste is cut to the limit");
        assert_eq!(Editor::new("abcdefgh", FieldKind::Text, 3, 300.0).text(), "abc", "so is the initial value");
    }

    #[test]
    fn click_places_the_caret_at_the_nearest_boundary() {
        let mut e = single("abcd");
        let v = e.vis(&M);
        assert_eq!(v.pos_at(0, -10.0), 0);
        assert_eq!(v.pos_at(0, 0.4 * CW), 0);
        assert_eq!(v.pos_at(0, 0.6 * CW), 1);
        assert_eq!(v.pos_at(0, 2.49 * CW), 2);
        assert_eq!(v.pos_at(0, 2.51 * CW), 3);
        assert_eq!(v.pos_at(0, 999.0), 4);
        assert!(e.click(0, 1.6 * CW, false, &M));
        assert_eq!(e.caret, 2);
        assert!(e.click(0, 4.0 * CW, true, &M));
        assert_eq!(e.selection(), Some((2, 4)), "shift-click / drag extends");
        assert!(!e.click(0, 4.0 * CW, true, &M));
    }

    #[test]
    fn double_click_selects_a_word_or_a_gap() {
        let mut e = single("foo  bar.baz");
        e.select_word_at(1);
        assert_eq!(e.selected_text(), "foo");
        e.select_word_at(3);
        assert_eq!(e.selected_text(), "  ");
        e.select_word_at(7);
        assert_eq!(e.selected_text(), "bar");
        e.select_word_at(8);
        assert_eq!(e.selected_text(), ".");
        e.select_word_at(99);
        assert_eq!(e.selected_text(), "baz", "past the end selects the last word");
        let mut empty = single("");
        empty.select_word_at(0);
        assert_eq!(empty.selection(), None);
    }

    #[test]
    fn the_caret_always_sits_on_a_char_boundary() {
        let mut e = single("a\u{1F600}b\u{e9}");
        for _ in 0..10 {
            e.nav(NavKey::Left, false, false, &M);
            assert!(e.caret <= e.len());
        }
        for _ in 0..10 {
            e.nav(NavKey::Right, false, false, &M);
            assert!(e.caret <= e.len());
        }
        assert_eq!(e.caret, e.len());
        e.move_to(999, false);
        assert_eq!(e.caret, 4);
        // The text built from the chars is always valid and char-aligned with the caret.
        let s = e.text();
        assert_eq!(s.chars().count(), e.len());
    }

    #[test]
    fn enter_commits_single_line_and_breaks_multiline() {
        let mut e = single("x");
        let r = e.apply(EditKey::Enter, false, false, &M, None);
        assert!(r.commit && !r.changed);
        let mut m = multi("x", 300.0);
        let r = m.apply(EditKey::Enter, false, false, &M, None);
        assert!(!r.commit && r.changed);
        assert_eq!(m.text(), "x\n");
    }

    #[test]
    fn multiline_wraps_at_spaces_and_breaks_long_words() {
        let w = 10.5 * CW;
        let e = multi("aaaa bbbb cccc", w);
        let v = e.vis(&M);
        let texts: Vec<String> = v.lines.iter().map(|l| e.chars()[l.start..l.end].iter().collect()).collect();
        assert_eq!(texts, ["aaaa bbbb ", "cccc"]);
        assert!(v.lines[0].soft && !v.lines[1].soft);
        assert_eq!(v.lines[0].caret_end, 9, "the hanging space is not a caret stop");
        // A long word is broken by char.
        let e = multi("abcdefghijklmnopqrstuvwxyz", w);
        let v = e.vis(&M);
        assert_eq!(v.lines.len(), 3);
        assert!(v.lines.iter().all(|l| l.end - l.start <= 10));
        assert_eq!(v.lines.last().unwrap().end, 26);
        // Newlines start lines, including empty ones.
        let e = multi("a\n\nb", w);
        let v = e.vis(&M);
        assert_eq!(v.lines.iter().map(|l| (l.start, l.end)).collect::<Vec<_>>(), [(0, 1), (2, 2), (3, 4)]);
        // Trailing newline gives an empty last line.
        let e = multi("a\n", w);
        assert_eq!(e.vis(&M).lines.len(), 2);
        // Empty text is one empty line.
        let e = multi("", w);
        let v = e.vis(&M);
        assert_eq!(v.lines.len(), 1);
        assert_eq!(v.pre, vec![vec![0.0]]);
    }

    #[test]
    fn multiline_up_down_home_end_keep_the_column() {
        let w = 10.5 * CW;
        let mut e = multi("hello\nab\nworld wide", w);
        at(&mut e, 4); // "hell|o"
        e.nav(NavKey::Down, false, false, &M);
        assert_eq!(e.caret, 8, "clamped to the end of the shorter line 'ab'");
        e.nav(NavKey::Down, false, false, &M);
        assert_eq!(e.caret, 13, "the column is remembered: 4 into 'world '");
        e.nav(NavKey::Up, false, false, &M);
        e.nav(NavKey::Up, false, false, &M);
        assert_eq!(e.caret, 4);
        e.nav(NavKey::Up, false, false, &M);
        assert_eq!(e.caret, 0, "Up on the first line goes to the start");
        e.nav(NavKey::End, false, false, &M);
        assert_eq!(e.caret, 5, "End is the end of the visual line");
        e.nav(NavKey::Home, false, false, &M);
        assert_eq!(e.caret, 0);
        at(&mut e, 12);
        e.nav(NavKey::Home, false, false, &M);
        assert_eq!(e.caret, 9);
        e.nav(NavKey::End, false, true, &M);
        assert_eq!(e.caret, e.len(), "Ctrl+End goes to the end of the text");
        e.nav(NavKey::Home, false, true, &M);
        assert_eq!(e.caret, 0);
        // Shift+Down selects across lines.
        e.nav(NavKey::Down, true, false, &M);
        assert_eq!(e.selection(), Some((0, 6)));
    }

    #[test]
    fn a_caret_at_a_soft_wrap_shows_on_the_next_line() {
        let w = 6.5 * CW;
        let e = multi("aaaa bbbb", w);
        let v = e.vis(&M);
        assert_eq!(v.lines.len(), 2);
        assert_eq!(v.line_of(4), 0, "before the space");
        assert_eq!(v.line_of(5), 1, "at the wrap: start of the next line");
        assert_eq!(v.x_of(5), 0.0);
        assert_eq!(v.line_of(9), 1);
        // Clicking far right on the first line stops before the hanging space.
        assert_eq!(v.pos_at(0, 100.0), 4);
    }

    #[test]
    fn multiline_click_picks_line_and_column() {
        let w = 20.0 * CW;
        let mut e = multi("abc\ndef", w);
        e.click(1, 2.1 * CW, false, &M);
        assert_eq!(e.caret, 6);
        e.click(0, 0.1, false, &M);
        assert_eq!(e.caret, 0);
        e.click(7, 1.0, false, &M);
        assert_eq!(e.caret, 4, "a line past the end clamps to the last line");
    }

    #[test]
    fn single_line_scroll_follows_the_caret() {
        let mut e = single(&"x".repeat(100));
        let view_w = 20.0 * CW;
        e.follow_caret(&M, view_w, LINE_H);
        assert!(e.scroll_x > 0.0, "the end of long text is scrolled into view");
        let end_x = e.vis(&M).x_of(100);
        assert!(end_x - e.scroll_x <= view_w);
        at(&mut e, 0);
        e.follow_caret(&M, view_w, LINE_H);
        assert_eq!(e.scroll_x, 0.0);
        // Deleting text never leaves a scroll past the end.
        e.set_text(&"y".repeat(100));
        e.follow_caret(&M, view_w, LINE_H);
        e.set_text("yy");
        e.follow_caret(&M, view_w, LINE_H);
        assert_eq!(e.scroll_x, 0.0);
    }

    #[test]
    fn multiline_scroll_follows_the_caret_line() {
        let mut e = multi(&"line\n".repeat(20), 300.0);
        let view_h = 5.0 * LINE_H;
        e.follow_caret(&M, 300.0, view_h);
        assert_eq!(e.scroll_y, 21.0 * LINE_H - view_h, "the caret is on the empty 21st line");
        at(&mut e, 0);
        e.follow_caret(&M, 300.0, view_h);
        assert_eq!(e.scroll_y, 0.0);
        at(&mut e, 5 * 7);
        e.follow_caret(&M, 300.0, view_h);
        assert_eq!(e.scroll_y, 8.0 * LINE_H - view_h);
    }
}
