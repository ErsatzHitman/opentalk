//! Page layout. Pure: [`layout`] places the page's [`Item`]s top to bottom into rects (content
//! space: x runs over the whole 380 px card, y from the top of the scrolling body) and reports the
//! content height. Text measuring is injected ([`Measure`]) so the tests need no fonts.
//!
//! Heights (logical px): toggle 44 (53.5 with one description line, +15.5 per further line),
//! Choice 44, Segmented 62 (with its label), Field 67 (Multiline: rows * 17 + 50), Radio card 52+,
//! Button 44, Row = `Row::height`. The same module owns the focus order (interactive items, then
//! the virtual rows, then the tail items), hit testing and the virtual-list index math.

use super::{BtnStyle, CARD_W_PAGE, FieldKind, Item, ItemId, Row, Trailing};
use crate::daemon::tray::popup::R;

/// Horizontal padding of the page content inside the card.
pub const PAD_X: f32 = 14.0;
/// Width available to items.
pub const CONTENT_W: f32 = CARD_W_PAGE - 2.0 * PAD_X;

pub const FS_BODY: f32 = 13.0;
pub const FS_SMALL: f32 = 12.0;
pub const FS_GROUP: f32 = 11.5;
pub const FS_CHIP: f32 = 10.5;
pub const LINE_BODY: f32 = 17.0;
pub const LINE_SMALL: f32 = 15.5;

pub const H_GROUP: f32 = 32.0;
pub const H_CHOICE: f32 = 44.0;
pub const H_RADIO: f32 = 36.0;
pub const H_TOGGLE_MIN: f32 = 44.0;
pub const H_SEGMENTED: f32 = 62.0;
pub const H_SEGMENTED_BARE: f32 = 40.0;
pub const SEG_H: f32 = 28.0;
pub const H_BUTTON: f32 = 44.0;
pub const BTN_H: f32 = 32.0;
pub const BTN_MIN_W: f32 = 88.0;
pub const H_DIVIDER: f32 = 13.0;
pub const FIELD_BOX_H: f32 = 34.0;
pub const FIELD_HOTKEY_H: f32 = 36.0;
pub const SWITCH_W: f32 = 36.0;
pub const SWITCH_H: f32 = 20.0;
/// Size of a row's trailing "x" / copy button.
pub const ROW_BTN: f32 = 26.0;
/// Icon buttons of an [`Item::Tools`] line: size and gap.
pub const TOOL_W: f32 = 30.0;
pub const TOOL_GAP: f32 = 4.0;
/// A tools line without / with its muted second line.
pub const H_TOOLS: f32 = 40.0;
pub const H_TOOLS_SUB: f32 = 46.0;
/// Room for the magnifier at the left of a search box (the text starts after it).
pub const SEARCH_INSET: f32 = 24.0;
/// Width a row's leading icon takes, including the gap after it.
pub const LEAD_W: f32 = 30.0;
/// Gap under a card-like item (radio, banner, result).
pub const CARD_GAP: f32 = 6.0;
/// Space above the first item and below the last.
pub const PAD_TOP: f32 = 12.0;
pub const PAD_BOTTOM: f32 = 12.0;

/// Text measure: `(text, logical px, weight) -> logical width`.
pub type Measure<'a> = &'a dyn Fn(&str, f32, i32) -> f32;

/// Keyboard focus / hover target: a static item by id, or a virtual row by index.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Foc {
    Item(ItemId),
    Row(usize),
}

/// Which part of an item a point is over.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Part {
    Body,
    /// The row's trailing button (delete / copy), or a toggle's switch.
    Trailing,
    /// A segment of a segmented control.
    Seg(usize),
}

/// One laid-out item.
#[derive(Clone, Debug, PartialEq)]
pub struct Placed {
    /// Index into the item list given to [`layout`].
    pub idx: usize,
    /// The slot, content space.
    pub rect: R,
    /// The control: switch, segmented bar, input box, button, trailing button. `R::ZERO` if none.
    pub ctl: R,
    /// Wrapped text (description / note / banner text / error / result text).
    pub lines: Vec<String>,
}

/// The virtualised region.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Virt {
    pub top: f32,
    pub count: usize,
    pub row_h: f32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Layout {
    pub width: f32,
    pub placed: Vec<Placed>,
    /// Number of `placed` entries before the virtual region (the rest are tail items).
    pub head_n: usize,
    pub virt: Option<Virt>,
    pub content_h: f32,
    /// Focusable ids in order, split around the virtual region.
    head_ids: Vec<ItemId>,
    tail_ids: Vec<ItemId>,
}

/// An item that takes keyboard focus.
pub fn focusable(item: &Item) -> bool {
    match item {
        Item::Toggle { .. } | Item::Segmented { .. } | Item::Choice { .. } | Item::Radio { .. } | Item::Field { .. } | Item::Row(_) => true,
        Item::Button { enabled, .. } => *enabled,
        Item::Tools { tools, .. } => tools.iter().any(|t| t.enabled),
        _ => false,
    }
}

/// Greedy word wrap at `max_w`. Explicit `\n` start a new line; an over-long word is split by char.
pub fn wrap_lines(text: &str, px: f32, weight: i32, max_w: f32, measure: Measure) -> Vec<String> {
    let mut out = Vec::new();
    for para in text.split('\n') {
        if para.trim().is_empty() {
            out.push(String::new());
            continue;
        }
        let mut line = String::new();
        for word in para.split(' ').filter(|w| !w.is_empty()) {
            let cand = if line.is_empty() { word.to_string() } else { format!("{line} {word}") };
            if measure(&cand, px, weight) <= max_w {
                line = cand;
                continue;
            }
            if !line.is_empty() {
                out.push(std::mem::take(&mut line));
            }
            if measure(word, px, weight) <= max_w {
                line = word.to_string();
            } else {
                // Break the long word by char.
                for ch in word.chars() {
                    let mut next = line.clone();
                    next.push(ch);
                    if !line.is_empty() && measure(&next, px, weight) > max_w {
                        out.push(std::mem::take(&mut line));
                    }
                    line.push(ch);
                }
            }
        }
        out.push(line);
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
}

/// Height of a multiline field's box.
pub fn multiline_box_h(rows: u8) -> f32 {
    rows.max(1) as f32 * LINE_BODY + 16.0
}

/// Slot height, control rect (slot-local, y from the slot top) and wrapped lines of an item.
fn slot(item: &Item, width: f32, measure: Measure) -> (f32, R, Vec<String>) {
    let (l, r) = (PAD_X, PAD_X + width);
    let z = R::ZERO;
    match item {
        Item::Group(_) => (H_GROUP, z, Vec::new()),
        Item::Note(t) => {
            let lines = wrap_lines(t, FS_SMALL, 400, width, measure);
            (lines.len() as f32 * LINE_SMALL + 10.0, z, lines)
        }
        Item::Text(t) => {
            let lines = wrap_lines(t, FS_BODY, 400, width, measure);
            (lines.len() as f32 * LINE_BODY + 10.0, z, lines)
        }
        Item::Banner { text, .. } => {
            let lines = wrap_lines(text, FS_SMALL, 400, width - 28.0, measure);
            (lines.len() as f32 * LINE_BODY + 18.0 + CARD_GAP, z, lines)
        }
        Item::Toggle { desc, .. } => {
            let lines = if desc.is_empty() { Vec::new() } else { wrap_lines(desc, FS_SMALL, 400, width - SWITCH_W - 16.0, measure) };
            let h = if lines.is_empty() { H_TOGGLE_MIN } else { (20.0 + LINE_BODY + 1.0 + lines.len() as f32 * LINE_SMALL).max(H_TOGGLE_MIN) };
            (h, R::xywh(r - SWITCH_W, (h - SWITCH_H) * 0.5, SWITCH_W, SWITCH_H), lines)
        }
        Item::Segmented { label, .. } => {
            if label.is_empty() {
                (H_SEGMENTED_BARE, R::new(l, 4.0, r, 4.0 + SEG_H), Vec::new())
            } else {
                (H_SEGMENTED, R::new(l, 28.0, r, 28.0 + SEG_H), Vec::new())
            }
        }
        Item::Choice { .. } => (H_CHOICE, z, Vec::new()),
        // one compact line: dot, label, the description muted after it (cut to fit)
        Item::Radio { .. } => (H_RADIO, z, Vec::new()),
        Item::Field { label, kind, error, .. } => {
            let top = 2.0 + if label.is_empty() { 0.0 } else { LINE_BODY + 4.0 };
            let box_h = match kind {
                FieldKind::Multiline { rows } => multiline_box_h(*rows),
                FieldKind::Hotkey => FIELD_HOTKEY_H,
                _ => FIELD_BOX_H,
            };
            let lines = error.as_deref().map(|e| wrap_lines(e, FS_SMALL, 400, width, measure)).unwrap_or_default();
            let err_h = if lines.is_empty() { 0.0 } else { 3.0 + lines.len() as f32 * LINE_SMALL };
            // a search box's control starts after its magnifier, so the text does too
            let cl = if *kind == FieldKind::Search { l + SEARCH_INSET } else { l };
            (top + box_h + err_h + 10.0, R::new(cl, top, r, top + box_h), lines)
        }
        Item::Button { label, style, .. } => {
            let link = matches!(style, BtnStyle::Link);
            let tw = measure(label, FS_BODY, 600);
            if link {
                (36.0, R::new(l, 4.0, l + tw, 32.0), Vec::new())
            } else {
                (H_BUTTON, R::new(l, 6.0, l + (tw + 28.0).max(BTN_MIN_W), 6.0 + BTN_H), Vec::new())
            }
        }
        Item::Tools { sub, tools, .. } => {
            let h = if sub.is_empty() { H_TOOLS } else { H_TOOLS_SUB };
            let n = tools.len() as f32;
            let w = n * TOOL_W + (n - 1.0).max(0.0) * TOOL_GAP;
            (h, R::new(r - w, (h - TOOL_W) * 0.5, r, (h + TOOL_W) * 0.5), Vec::new())
        }
        Item::Result { text, .. } => {
            let lines = wrap_lines(text, FS_SMALL, 400, width - 24.0, measure);
            (16.0 + LINE_BODY + lines.len() as f32 * LINE_SMALL + CARD_GAP, z, lines)
        }
        Item::Row(row) => (row.height, row_btn(row, R::new(l, 0.0, r, row.height)), Vec::new()),
        Item::Divider => (H_DIVIDER, z, Vec::new()),
        Item::Space(h) => (h.max(0.0), z, Vec::new()),
    }
}

/// Icon button `i` of a tools line whose buttons occupy `ctl`.
pub fn tool_rect(ctl: R, i: usize) -> R {
    let l = ctl.l + i as f32 * (TOOL_W + TOOL_GAP);
    R::new(l, ctl.t, l + TOOL_W, ctl.b)
}

/// The box a field draws: its control, widened back over a search box's magnifier.
pub fn field_frame(ctl: R, kind: FieldKind) -> R {
    if kind == FieldKind::Search { R::new(ctl.l - SEARCH_INSET, ctl.t, ctl.r, ctl.b) } else { ctl }
}

/// The trailing delete / copy button of a row in its slot (`R::ZERO` for other trailings).
pub fn row_btn(row: &Row, rect: R) -> R {
    match row.trailing {
        Trailing::Delete | Trailing::Copy => R::xywh(rect.r - 4.0 - ROW_BTN, rect.cy() - ROW_BTN * 0.5, ROW_BTN, ROW_BTN),
        _ => R::ZERO,
    }
}

/// Width taken by a row's trailing content, including the gap before it.
pub fn row_trailing_w(row: &Row, measure: Measure) -> f32 {
    match &row.trailing {
        Trailing::None => 0.0,
        Trailing::Chevron => 26.0,
        Trailing::Value(v) => measure(v, FS_SMALL, 400).min(150.0) + 36.0,
        Trailing::Badge { text, .. } => measure(text, FS_CHIP, 600) + 24.0,
        Trailing::Delete | Trailing::Copy => ROW_BTN + 8.0,
    }
}

/// Segment `i` of a segmented control `ctl` with `n` options.
pub fn seg_rect(ctl: R, n: usize, i: usize) -> R {
    let w = ctl.w() / n.max(1) as f32;
    R::new(ctl.l + w * i as f32, ctl.t, ctl.l + w * (i as f32 + 1.0), ctl.b)
}

/// Lay `items` out; `items[tail_start..]` go after the virtual region `virt = (count, row_h)`.
pub fn layout(items: &[Item], tail_start: usize, virt: Option<(usize, f32)>, measure: Measure, width: f32) -> Layout {
    let tail_start = tail_start.min(items.len());
    let mut y = PAD_TOP;
    let mut placed = Vec::with_capacity(items.len());
    let mut head_ids = Vec::new();
    let mut tail_ids = Vec::new();
    let mut vr = None;
    let put_virt = |y: &mut f32, vr: &mut Option<Virt>| {
        if let (Some((count, row_h)), None) = (virt, &*vr) {
            *vr = Some(Virt { top: *y, count, row_h });
            *y += count as f32 * row_h;
        }
    };
    for (idx, item) in items.iter().enumerate() {
        if idx == tail_start {
            put_virt(&mut y, &mut vr);
        }
        let (h, ctl, lines) = slot(item, width, measure);
        let rect = R::new(PAD_X, y, PAD_X + width, y + h);
        let ctl = if ctl == R::ZERO { ctl } else { ctl.offset(0.0, y) };
        placed.push(Placed { idx, rect, ctl, lines });
        if let (true, Some(id)) = (focusable(item), item.id()) {
            if idx < tail_start { head_ids.push(id) } else { tail_ids.push(id) }
        }
        y += h;
    }
    put_virt(&mut y, &mut vr);
    Layout { width, placed, head_n: tail_start, virt: vr, content_h: y + PAD_BOTTOM, head_ids, tail_ids }
}

impl Layout {
    /// Number of focus stops.
    pub fn order_len(&self) -> usize {
        self.head_ids.len() + self.virt.map_or(0, |v| v.count) + self.tail_ids.len()
    }

    /// The focus target at position `pos` of the order.
    pub fn foc_at(&self, pos: usize) -> Option<Foc> {
        let h = self.head_ids.len();
        let v = self.virt.map_or(0, |v| v.count);
        if pos < h {
            Some(Foc::Item(self.head_ids[pos]))
        } else if pos < h + v {
            Some(Foc::Row(pos - h))
        } else {
            self.tail_ids.get(pos - h - v).map(|&id| Foc::Item(id))
        }
    }

    pub fn pos_of(&self, f: Foc) -> Option<usize> {
        let h = self.head_ids.len();
        let v = self.virt.map_or(0, |v| v.count);
        match f {
            Foc::Item(id) => self.head_ids.iter().position(|&i| i == id).or_else(|| self.tail_ids.iter().position(|&i| i == id).map(|p| p + h + v)),
            Foc::Row(i) => (i < v).then_some(h + i),
        }
    }

    /// Next / previous focus stop (wraps). `from == None` starts at the first / last.
    pub fn step(&self, from: Option<Foc>, forward: bool) -> Option<Foc> {
        let n = self.order_len();
        if n == 0 {
            return None;
        }
        let pos = from.and_then(|f| self.pos_of(f));
        let next = match (pos, forward) {
            (None, true) => 0,
            (None, false) => n - 1,
            (Some(p), true) => (p + 1) % n,
            (Some(p), false) => (p + n - 1) % n,
        };
        self.foc_at(next)
    }

    pub fn first(&self) -> Option<Foc> {
        self.foc_at(0)
    }

    pub fn last(&self) -> Option<Foc> {
        self.order_len().checked_sub(1).and_then(|p| self.foc_at(p))
    }

    /// The placed entry of a static item id.
    pub fn placed_of(&self, items: &[Item], id: ItemId) -> Option<&Placed> {
        self.placed.iter().find(|p| items.get(p.idx).and_then(Item::id) == Some(id))
    }

    /// The slot of a virtual row.
    pub fn virt_rect(&self, i: usize) -> Option<R> {
        let v = self.virt?;
        (i < v.count).then(|| R::new(PAD_X, v.top + i as f32 * v.row_h, PAD_X + self.width, v.top + (i + 1) as f32 * v.row_h))
    }

    pub fn rect_of(&self, items: &[Item], f: Foc) -> Option<R> {
        match f {
            Foc::Item(id) => self.placed_of(items, id).map(|p| p.rect),
            Foc::Row(i) => self.virt_rect(i),
        }
    }

    /// Indexes (into `placed`) of the entries intersecting `[y0, y1)`.
    pub fn visible(&self, y0: f32, y1: f32) -> std::ops::Range<usize> {
        let a = self.placed.partition_point(|p| p.rect.b <= y0);
        let b = self.placed.partition_point(|p| p.rect.t < y1);
        a..b.max(a)
    }

    /// Virtual row indexes intersecting `[y0, y1)`: pure division, O(1).
    pub fn virt_visible(&self, y0: f32, y1: f32) -> std::ops::Range<usize> {
        let Some(v) = self.virt else { return 0..0 };
        if v.count == 0 || v.row_h <= 0.0 {
            return 0..0;
        }
        let first = (((y0 - v.top) / v.row_h).floor().max(0.0) as usize).min(v.count);
        let last = (((y1 - v.top) / v.row_h).ceil().max(0.0) as usize).min(v.count);
        first..last.max(first)
    }

    /// What is at content-space point (x, y): the target and the part. `virt_row` supplies the row
    /// at a virtual index (for its trailing button); `items` are the static items.
    pub fn hit(&self, items: &[Item], x: f32, y: f32, virt_row: &dyn Fn(usize) -> Option<Row>) -> Option<(Foc, Part)> {
        if let Some(v) = self.virt.filter(|v| y >= v.top && y < v.top + v.count as f32 * v.row_h) {
            let i = ((y - v.top) / v.row_h) as usize;
            let rect = self.virt_rect(i)?;
            if x < rect.l || x >= rect.r {
                return None;
            }
            let part = match virt_row(i) {
                Some(row) if row_btn(&row, rect).contains(x, y) => Part::Trailing,
                _ => Part::Body,
            };
            return Some((Foc::Row(i), part));
        }
        let range = self.visible(y, y + 0.01);
        for p in &self.placed[range] {
            let item = &items[p.idx];
            if !focusable(item) || !p.rect.contains(x, y) {
                continue;
            }
            let id = item.id()?;
            let part = match item {
                Item::Segmented { options, .. } if p.ctl.contains(x, y) => (0..options.len()).find(|&i| seg_rect(p.ctl, options.len(), i).contains(x, y)).map_or(Part::Body, Part::Seg),
                Item::Segmented { .. } => return None,
                Item::Toggle { .. } if p.ctl.inflate(6.0).contains(x, y) => Part::Trailing,
                Item::Row(_) if p.ctl.contains(x, y) => Part::Trailing,
                Item::Tools { tools, .. } => (0..tools.len()).find(|&i| tool_rect(p.ctl, i).inflate(2.0).contains(x, y)).map_or(Part::Body, Part::Seg),
                Item::Field { kind, .. } if field_frame(p.ctl, *kind).contains(x, y) => Part::Body,
                Item::Field { .. } | Item::Button { .. } if !p.ctl.contains(x, y) => return None,
                _ => Part::Body,
            };
            return Some((Foc::Item(id), part));
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::tray::page::Tone;

    /// 6 px per char at any size/weight: easy to reason about.
    fn m6(s: &str, _px: f32, _w: i32) -> f32 {
        s.chars().count() as f32 * 6.0
    }

    fn toggle(id: ItemId, desc: &str) -> Item {
        Item::Toggle { id, label: "L".into(), desc: desc.into(), on: false }
    }

    fn lay(items: &[Item]) -> Layout {
        layout(items, items.len(), None, &m6, CONTENT_W)
    }

    const ROW_H: f32 = 32.0;

    #[test]
    fn wrap_breaks_at_words_newlines_and_long_words() {
        let w = |t: &str, max: f32| wrap_lines(t, 12.0, 400, max, &m6);
        assert_eq!(w("one two three", 40.0), ["one", "two", "three"]);
        assert_eq!(w("one two three", 77.0), ["one two", "three"]);
        assert_eq!(w("one two three", 78.0), ["one two three"]);
        assert_eq!(w("a\nb", 100.0), ["a", "b"]);
        assert_eq!(w("", 100.0), [""]);
        let long = w("abcdefghijkl", 30.0);
        assert!(long.len() >= 3 && long.concat() == "abcdefghijkl", "{long:?}");
        assert!(long.iter().all(|l| l.chars().count() <= 5));
        // Runs of spaces collapse; nothing is lost but spacing.
        assert_eq!(w("a    b", 100.0), ["a b"]);
    }

    #[test]
    fn item_heights_by_kind() {
        let h = |i: Item| lay(&[i]).placed[0].rect.h();
        assert_eq!(h(Item::Group("G".into())), H_GROUP);
        assert_eq!(h(toggle(1, "")), 44.0);
        // One description line makes the row taller than the bare one, each extra line adds 15.5.
        let one = h(toggle(1, "short"));
        let many = toggle(1, &"word ".repeat(40));
        let two = h(many.clone());
        assert!(one > 44.0 && two > one);
        let lines = lay(&[many]).placed[0].lines.len();
        assert!((two - one - (lines as f32 - 1.0) * LINE_SMALL).abs() < 0.01);
        assert_eq!(h(Item::Choice { id: 1, label: "c".into(), value: "v".into() }), 44.0);
        assert_eq!(h(Item::Segmented { id: 1, label: "s".into(), options: vec!["a".into(), "b".into()], sel: 0 }), H_SEGMENTED);
        assert_eq!(h(Item::Segmented { id: 1, label: String::new(), options: vec!["a".into(), "b".into()], sel: 0 }), H_SEGMENTED_BARE);
        assert_eq!(h(Item::Button { id: 1, label: "Go".into(), style: BtnStyle::Primary, enabled: true }), H_BUTTON);
        assert_eq!(h(Item::Divider), H_DIVIDER);
        assert_eq!(h(Item::Space(7.0)), 7.0);
        assert_eq!(h(Item::Row(Row::new(1, "r", 64.0))), 64.0);
        // radios are one line whatever the description
        assert_eq!(h(Item::Radio { id: 1, label: "r".into(), desc: String::new(), on: true }), H_RADIO);
        assert_eq!(h(Item::Radio { id: 1, label: "r".into(), desc: "d ".repeat(80), on: true }), H_RADIO);
        assert_eq!(h(Item::Tools { id: 1, label: "t".into(), sub: String::new(), tools: Vec::new() }), H_TOOLS);
        assert_eq!(h(Item::Tools { id: 1, label: "t".into(), sub: "s".into(), tools: Vec::new() }), H_TOOLS_SUB);
    }

    #[test]
    fn field_height_depends_on_kind_label_and_error() {
        let f = |kind, label: &str, err: Option<&str>| {
            lay(&[Item::Field { id: 1, label: label.into(), hint: String::new(), value: String::new(), kind, error: err.map(String::from), max_chars: 0 }]).placed[0].rect.h()
        };
        let base = f(FieldKind::Text, "Name", None);
        assert_eq!(base, 2.0 + 17.0 + 4.0 + 34.0 + 10.0);
        assert!(f(FieldKind::Text, "", None) < base);
        assert!(f(FieldKind::Text, "Name", Some("bad")) > base);
        assert_eq!(f(FieldKind::Multiline { rows: 5 }, "Name", None) - base, 5.0 * 17.0 + 16.0 - 34.0);
        assert!(f(FieldKind::Hotkey, "Name", None) > base);
        // The control rect is the input box, inside the slot.
        let l = lay(&[Item::Field { id: 1, label: "N".into(), hint: String::new(), value: String::new(), kind: FieldKind::Text, error: None, max_chars: 0 }]);
        let p = &l.placed[0];
        assert!(p.ctl.t >= p.rect.t && p.ctl.b <= p.rect.b && p.ctl.h() == FIELD_BOX_H);
    }

    #[test]
    fn items_stack_without_overlap_and_content_height_covers_them() {
        let items = vec![
            Item::Group("A".into()),
            toggle(1, "a description that is rather long so that it wraps to several lines in the narrow card"),
            Item::Banner { tone: Tone::Warn, text: "careful ".repeat(30) },
            Item::Note("note".into()),
            Item::Divider,
            Item::Row(Row::new(9, "row", 44.0)),
        ];
        let l = lay(&items);
        let mut y = 0.0;
        for p in &l.placed {
            assert!(p.rect.t >= y - 0.001, "overlap at {}", p.idx);
            assert!(p.rect.l == PAD_X && p.rect.r == PAD_X + CONTENT_W);
            y = p.rect.b;
        }
        assert_eq!(l.content_h, y + PAD_BOTTOM);
        assert_eq!(l.head_n, 6);
        assert_eq!(l.placed[0].rect.t, PAD_TOP);
    }

    #[test]
    fn controls_sit_inside_their_slots() {
        let items = vec![
            toggle(1, "d"),
            Item::Segmented { id: 2, label: "s".into(), options: vec!["a".into(), "b".into(), "c".into()], sel: 1 },
            Item::Button { id: 3, label: "Test".into(), style: BtnStyle::Outline, enabled: true },
            Item::Button { id: 4, label: "Reset".into(), style: BtnStyle::Link, enabled: true },
            Item::Row({
                let mut r = Row::new(5, "w", ROW_H);
                r.trailing = Trailing::Delete;
                r
            }),
        ];
        let l = lay(&items);
        for p in &l.placed {
            assert!(p.ctl.t >= p.rect.t - 0.001 && p.ctl.b <= p.rect.b + 0.001, "ctl of {} outside its slot: {:?} vs {:?}", p.idx, p.ctl, p.rect);
            assert!(p.ctl.l >= p.rect.l - 0.001 && p.ctl.r <= p.rect.r + 0.001);
        }
        // The switch is right-aligned 36x20; the segmented bar spans the content; a button is at least 88 wide.
        assert_eq!((l.placed[0].ctl.w(), l.placed[0].ctl.h()), (SWITCH_W, SWITCH_H));
        assert_eq!(l.placed[0].ctl.r, PAD_X + CONTENT_W);
        assert!((l.placed[0].ctl.cy() - l.placed[0].rect.cy()).abs() < 0.01, "the switch is centred in its row");
        assert_eq!(l.placed[1].ctl.w(), CONTENT_W);
        assert!(l.placed[2].ctl.w() >= BTN_MIN_W);
        assert_eq!(l.placed[2].ctl.h(), BTN_H);
        // A link is as wide as its text.
        assert_eq!(l.placed[3].ctl.w(), 5.0 * 6.0);
        // The delete button is at the right edge of the row.
        assert_eq!(l.placed[4].ctl.w(), ROW_BTN);
        assert!(l.placed[4].ctl.r < l.placed[4].rect.r);
    }

    #[test]
    fn segments_split_the_bar_evenly() {
        let ctl = R::new(10.0, 0.0, 130.0, 28.0);
        assert_eq!(seg_rect(ctl, 3, 0), R::new(10.0, 0.0, 50.0, 28.0));
        assert_eq!(seg_rect(ctl, 3, 2), R::new(90.0, 0.0, 130.0, 28.0));
    }

    #[test]
    fn focus_order_skips_inert_items_and_disabled_buttons_and_wraps() {
        let items = vec![
            Item::Group("g".into()),
            toggle(10, ""),
            Item::Note("n".into()),
            Item::Button { id: 11, label: "off".into(), style: BtnStyle::Outline, enabled: false },
            Item::Divider,
            Item::Choice { id: 12, label: "c".into(), value: String::new() },
            Item::Result { ok: true, badge: String::new(), text: String::new() },
        ];
        let l = lay(&items);
        assert_eq!(l.order_len(), 2);
        assert_eq!(l.first(), Some(Foc::Item(10)));
        assert_eq!(l.step(Some(Foc::Item(10)), true), Some(Foc::Item(12)));
        assert_eq!(l.step(Some(Foc::Item(12)), true), Some(Foc::Item(10)), "wraps");
        assert_eq!(l.step(Some(Foc::Item(10)), false), Some(Foc::Item(12)));
        assert_eq!(l.step(None, true), Some(Foc::Item(10)));
        assert_eq!(l.step(None, false), Some(Foc::Item(12)));
        assert_eq!(l.last(), Some(Foc::Item(12)));
        assert_eq!(l.step(Some(Foc::Item(99)), true), Some(Foc::Item(10)), "unknown focus starts over");
        assert_eq!(lay(&[Item::Note("x".into())]).step(None, true), None);
    }

    fn with_virt(count: usize) -> (Vec<Item>, Layout) {
        let items = vec![Item::Group("Head".into()), toggle(1, ""), Item::Button { id: 2, label: "More".into(), style: BtnStyle::Outline, enabled: true }];
        // The button is the tail (index 2); rows go between the toggle and it.
        let l = layout(&items, 2, Some((count, 64.0)), &m6, CONTENT_W);
        (items, l)
    }

    #[test]
    fn virtual_region_sits_between_head_and_tail() {
        let (_items, l) = with_virt(100);
        let v = l.virt.unwrap();
        assert_eq!(v.count, 100);
        assert_eq!(v.top, l.placed[1].rect.b, "right after the last head item");
        assert_eq!(l.placed[2].rect.t, v.top + 100.0 * 64.0, "tail after the rows");
        assert_eq!(l.head_n, 2);
        assert!(l.content_h > v.top + 6400.0);
        // Focus runs head, rows, tail.
        assert_eq!(l.order_len(), 1 + 100 + 1);
        assert_eq!(l.foc_at(0), Some(Foc::Item(1)));
        assert_eq!(l.foc_at(1), Some(Foc::Row(0)));
        assert_eq!(l.foc_at(100), Some(Foc::Row(99)));
        assert_eq!(l.foc_at(101), Some(Foc::Item(2)));
        assert_eq!(l.foc_at(102), None);
        assert_eq!(l.pos_of(Foc::Row(5)), Some(6));
        assert_eq!(l.pos_of(Foc::Item(2)), Some(101));
        assert_eq!(l.pos_of(Foc::Row(100)), None);
        assert_eq!(l.step(Some(Foc::Row(99)), true), Some(Foc::Item(2)));
        assert_eq!(l.step(Some(Foc::Item(1)), true), Some(Foc::Row(0)));
    }

    #[test]
    fn a_virtual_list_without_tail_items_ends_the_content() {
        let items = vec![toggle(1, "")];
        let l = layout(&items, 1, Some((10, 32.0)), &m6, CONTENT_W);
        let v = l.virt.unwrap();
        assert_eq!(v.top, l.placed[0].rect.b);
        assert_eq!(l.content_h, v.top + 320.0 + PAD_BOTTOM);
        // No items at all: the list starts at the top.
        let l = layout(&[], 0, Some((3, 10.0)), &m6, CONTENT_W);
        assert_eq!(l.virt.unwrap().top, PAD_TOP);
    }

    #[test]
    fn virtual_rows_index_by_division() {
        let (_i, l) = with_virt(1000);
        let top = l.virt.unwrap().top;
        assert_eq!(l.virt_visible(0.0, top), 0..0, "head only");
        assert_eq!(l.virt_visible(top, top + 64.0), 0..1);
        assert_eq!(l.virt_visible(top + 63.9, top + 64.1), 0..2);
        assert_eq!(l.virt_visible(top + 640.0, top + 640.0 + 300.0), 10..15);
        assert_eq!(l.virt_visible(top + 1.0e6, top + 1.1e6), 1000..1000, "past the end");
        assert_eq!(l.virt_rect(3).unwrap().t, top + 192.0);
        assert!(l.virt_rect(1000).is_none());
        let none = lay(&[Item::Divider]);
        assert_eq!(none.virt_visible(0.0, 100.0), 0..0);
    }

    #[test]
    fn visible_range_is_a_binary_search_over_slots() {
        let items: Vec<Item> = (0..50).map(|i| Item::Row(Row::new(i, "r", 44.0))).collect();
        let l = lay(&items);
        let r = l.visible(0.0, 100.0);
        assert_eq!(r.start, 0);
        assert!(l.placed[r.clone()].iter().all(|p| p.rect.t < 100.0));
        assert!(l.placed[r.end].rect.t >= 100.0);
        let mid = l.visible(1000.0, 1100.0);
        assert!(l.placed[mid.start].rect.b > 1000.0 && (mid.start == 0 || l.placed[mid.start - 1].rect.b <= 1000.0));
        assert!(l.visible(1.0e6, 2.0e6).is_empty());
    }

    #[test]
    fn hit_testing_resolves_item_and_part() {
        let items = vec![
            toggle(1, ""),
            Item::Segmented { id: 2, label: "s".into(), options: vec!["a".into(), "b".into()], sel: 0 },
            Item::Button { id: 3, label: "Run".into(), style: BtnStyle::Primary, enabled: true },
            Item::Field { id: 4, label: "F".into(), hint: String::new(), value: String::new(), kind: FieldKind::Text, error: None, max_chars: 0 },
            Item::Row({
                let mut r = Row::new(5, "w", 32.0);
                r.trailing = Trailing::Delete;
                r
            }),
        ];
        let l = lay(&items);
        let no_virt = |_: usize| None;
        let at = |x: f32, y: f32| l.hit(&items, x, y, &no_virt);
        let (t, s, b, f, r) = (&l.placed[0], &l.placed[1], &l.placed[2], &l.placed[3], &l.placed[4]);
        assert_eq!(at(t.rect.l + 5.0, t.rect.cy()), Some((Foc::Item(1), Part::Body)));
        assert_eq!(at(t.ctl.cx(), t.ctl.cy()), Some((Foc::Item(1), Part::Trailing)));
        assert_eq!(at(s.ctl.l + 2.0, s.ctl.cy()), Some((Foc::Item(2), Part::Seg(0))));
        assert_eq!(at(s.ctl.r - 2.0, s.ctl.cy()), Some((Foc::Item(2), Part::Seg(1))));
        assert_eq!(at(s.rect.l + 2.0, s.rect.t + 5.0), None, "the label of a segmented control is inert");
        assert_eq!(at(b.ctl.cx(), b.ctl.cy()), Some((Foc::Item(3), Part::Body)));
        assert_eq!(at(b.ctl.r + 10.0, b.ctl.cy()), None, "beside a button");
        assert_eq!(at(f.ctl.cx(), f.ctl.cy()), Some((Foc::Item(4), Part::Body)));
        assert_eq!(at(f.rect.l + 2.0, f.rect.t + 1.0), None, "above the input box");
        assert_eq!(at(r.ctl.cx(), r.ctl.cy()), Some((Foc::Item(5), Part::Trailing)));
        assert_eq!(at(r.rect.l + 4.0, r.rect.cy()), Some((Foc::Item(5), Part::Body)));
        assert_eq!(at(2.0, 20.0), None, "left of the content");
        assert_eq!(at(100.0, 1.0e5), None);
    }

    #[test]
    fn virtual_row_hit_finds_the_trailing_button() {
        let (items, l) = with_virt(10);
        let v = l.virt.unwrap();
        let row = |_i: usize| {
            let mut r = Row::new(0, "w", 64.0);
            r.trailing = Trailing::Copy;
            Some(r)
        };
        let y = v.top + 2.0 * 64.0 + 32.0;
        assert_eq!(l.hit(&items, 30.0, y, &row), Some((Foc::Row(2), Part::Body)));
        assert_eq!(l.hit(&items, PAD_X + CONTENT_W - 17.0, y, &row), Some((Foc::Row(2), Part::Trailing)));
        assert_eq!(l.hit(&items, 5.0, y, &row), None);
    }

    #[test]
    fn disabled_button_has_no_hit() {
        let items = vec![Item::Button { id: 1, label: "No".into(), style: BtnStyle::Outline, enabled: false }];
        let l = lay(&items);
        let p = &l.placed[0];
        assert_eq!(l.hit(&items, p.ctl.cx(), p.ctl.cy(), &|_| None), None);
    }

    #[test]
    fn row_trailing_widths() {
        let mut r = Row::new(1, "r", 44.0);
        assert_eq!(row_trailing_w(&r, &m6), 0.0);
        r.trailing = Trailing::Chevron;
        assert!(row_trailing_w(&r, &m6) > 0.0);
        r.trailing = Trailing::Value("abcd".into());
        assert_eq!(row_trailing_w(&r, &m6), 24.0 + 36.0);
        r.trailing = Trailing::Value("x".repeat(100));
        assert_eq!(row_trailing_w(&r, &m6), 150.0 + 36.0, "long values are capped");
        r.trailing = Trailing::Badge { text: "OK".into(), tone: Tone::Ok };
        assert_eq!(row_trailing_w(&r, &m6), 12.0 + 24.0);
    }
}
