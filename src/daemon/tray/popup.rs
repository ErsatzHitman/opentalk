//! Tray popup, pure model: what the rows are, where they sit, which one a point or a key picks, and
//! where the popup goes relative to the tray. No Win32 and no drawing here (see `popup_draw` and
//! `popup_win`), so all of it is unit-tested.
//!
//! Layout is in logical pixels with the origin at the top-left of the card (the visible popover);
//! the window around it adds [`MARGIN`] on every side for the soft shadow. Placement works in device
//! pixels on the monitor the tray icon is on.

use super::TrayCmd;

pub const CARD_W: f32 = 288.0;
/// Transparent border of the window around the card (room for the shadow).
pub const MARGIN: f32 = 16.0;
/// Inner padding between the card edge and the rows.
pub const PAD: f32 = 6.0;
pub const RADIUS: f32 = 10.0;
pub const ROW_RADIUS: f32 = 6.0;
pub const HEADER_H: f32 = 44.0;
pub const PRIMARY_H: f32 = 40.0;
pub const ROW_H: f32 = 34.0;
pub const RECENT_H: f32 = 32.0;
pub const SECTION_H: f32 = 26.0;
pub const SEP_H: f32 = 9.0;
/// Distance between the popup and the edge of the work area, like Windows' own flyouts.
pub const EDGE_GAP: f32 = 12.0;
/// How many recent dictations the popup shows.
pub const MAX_RECENT: usize = 2;
/// A tray click this soon after the popup closed itself (the click that deactivated it) toggles the
/// popup shut instead of opening it again.
pub const REOPEN_GUARD_MS: u64 = 300;

/// What the popup shows. Built by the daemon from its state when the tray icon is activated.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Content {
    pub phase: Phase,
    pub floating_pill_visible: bool,
    pub can_retry: bool,
    /// The dictation hotkey as the pill's tooltip shows it, e.g. "Ctrl + Win".
    pub hotkey: String,
    /// Newest first, at most [`MAX_RECENT`], already single-line.
    pub recent: Vec<String>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Phase {
    #[default]
    Ready,
    Recording,
    Transcribing,
}

impl Phase {
    pub fn label(self) -> &'static str {
        match self {
            Phase::Ready => "Ready",
            Phase::Recording => "Recording",
            Phase::Transcribing => "Transcribing",
        }
    }
}

/// What a row does when chosen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    Dictation,
    History,
    Dictionary,
    Settings,
    /// Copy the n-th recent dictation to the clipboard.
    Recent(usize),
    TogglePill,
    Retry,
    Quit,
}

impl Target {
    /// The tray command this row maps to (`None` for the clipboard row, handled by the popup).
    pub fn cmd(self) -> Option<TrayCmd> {
        Some(match self {
            Target::Dictation => TrayCmd::ToggleListening,
            Target::History => TrayCmd::OpenHistory,
            Target::Dictionary => TrayCmd::OpenDictionary,
            Target::Settings => TrayCmd::OpenSettings,
            Target::TogglePill => TrayCmd::ToggleFloatingIcon,
            Target::Retry => TrayCmd::RetryLast,
            Target::Quit => TrayCmd::Quit,
            Target::Recent(_) => return None,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Header,
    /// The big Start / Stop dictation row.
    Primary,
    Item,
    Section,
    Separator,
    Recent,
}

/// An axis-aligned rectangle in logical px (card-local).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct R {
    pub l: f32,
    pub t: f32,
    pub r: f32,
    pub b: f32,
}

impl R {
    #[cfg(test)]
    pub fn h(&self) -> f32 {
        self.b - self.t
    }
    pub fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.l && x < self.r && y >= self.t && y < self.b
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    pub kind: Kind,
    pub rect: R,
    pub target: Option<Target>,
    pub label: String,
    /// A disabled row is drawn dimmed and can be neither hovered nor chosen.
    pub enabled: bool,
}

impl Row {
    pub fn selectable(&self) -> bool {
        self.target.is_some() && self.enabled
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Layout {
    pub card_w: f32,
    pub card_h: f32,
    pub rows: Vec<Row>,
}

impl Layout {
    /// Window size in logical px (the card plus the shadow margin on every side).
    pub fn window_size(&self) -> (f32, f32) {
        (self.card_w + 2.0 * MARGIN, self.card_h + 2.0 * MARGIN)
    }

    /// Index of the selectable row under the card-local point.
    pub fn hit(&self, x: f32, y: f32) -> Option<usize> {
        self.rows.iter().position(|r| r.selectable() && r.rect.contains(x, y))
    }

    pub fn selectable(&self) -> Vec<usize> {
        (0..self.rows.len()).filter(|&i| self.rows[i].selectable()).collect()
    }

    pub fn target_of(&self, row: usize) -> Option<Target> {
        self.rows.get(row).filter(|r| r.selectable()).and_then(|r| r.target)
    }
}

/// The dictation row's label for a phase.
pub fn dictation_label(phase: Phase) -> &'static str {
    match phase {
        Phase::Ready => "Start dictation",
        Phase::Recording => "Stop dictation",
        Phase::Transcribing => "Transcribing...",
    }
}

/// Stacks rows top to bottom.
struct Stack {
    rows: Vec<Row>,
    y: f32,
}

impl Stack {
    fn row(&mut self, kind: Kind, h: f32, target: Option<Target>, label: &str) -> &mut Row {
        // Header and separators run edge to edge; everything else sits inside the padding.
        let (l, r) = if matches!(kind, Kind::Header | Kind::Separator) { (0.0, CARD_W) } else { (PAD, CARD_W - PAD) };
        self.rows.push(Row { kind, rect: R { l, t: self.y, r, b: self.y + h }, target, label: label.to_string(), enabled: true });
        self.y += h;
        self.rows.last_mut().expect("just pushed")
    }

    fn item(&mut self, target: Target, label: &str) {
        self.row(Kind::Item, ROW_H, Some(target), label);
    }
}

pub fn layout(c: &Content) -> Layout {
    let mut s = Stack { rows: Vec::new(), y: 0.0 };
    s.row(Kind::Header, HEADER_H, None, "OpenTalk");
    s.y += PAD;
    s.row(Kind::Primary, PRIMARY_H, Some(Target::Dictation), dictation_label(c.phase)).enabled = c.phase != Phase::Transcribing;
    s.y += 4.0;
    s.item(Target::History, "History");
    s.item(Target::Dictionary, "Dictionary");
    s.item(Target::Settings, "Settings");
    if !c.recent.is_empty() {
        s.row(Kind::Separator, SEP_H, None, "");
        s.row(Kind::Section, SECTION_H, None, "Recent");
        for (i, text) in c.recent.iter().take(MAX_RECENT).enumerate() {
            s.row(Kind::Recent, RECENT_H, Some(Target::Recent(i)), text);
        }
    }
    s.row(Kind::Separator, SEP_H, None, "");
    s.item(Target::TogglePill, if c.floating_pill_visible { "Hide floating pill" } else { "Show floating pill" });
    if c.can_retry {
        s.item(Target::Retry, "Retry last dictation");
    }
    s.item(Target::Quit, "Quit OpenTalk");
    s.y += PAD;
    Layout { card_w: CARD_W, card_h: s.y, rows: s.rows }
}

// ---------------------------------------------------------------------------------------------
// Pointer and keyboard state
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    Up,
    Down,
    Home,
    End,
    Tab,
    ShiftTab,
    Enter,
    Space,
    Escape,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyOutcome {
    None,
    /// The highlight moved (or was set): redraw.
    Moved,
    Close,
    Choose(Target),
}

/// The highlighted row plus how it got highlighted. One highlight serves the pointer and the
/// keyboard, as in a menu: moving the pointer onto a row highlights it, arrow keys move it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Nav {
    pub active: Option<usize>,
    /// The highlight came from the keyboard: draw the focus ring.
    pub keyboard: bool,
    /// The primary button is down on the highlighted row.
    pub pressed: bool,
}

impl Nav {
    /// Opened from the keyboard (NIN_KEYSELECT): focus the first row at once.
    pub fn for_keyboard(layout: &Layout) -> Nav {
        Nav { active: layout.selectable().first().copied(), keyboard: true, pressed: false }
    }

    /// The pointer is at `p` (card-local) or gone. True when the highlight changed.
    pub fn pointer(&mut self, layout: &Layout, p: Option<(f32, f32)>) -> bool {
        let before = *self;
        match p {
            Some((x, y)) => {
                // Over a separator or the header: keep what the keyboard picked, else nothing.
                if let Some(i) = layout.hit(x, y) {
                    self.active = Some(i);
                    self.keyboard = false;
                } else if !self.keyboard {
                    self.active = None;
                }
            }
            None => {
                if !self.keyboard {
                    self.active = None;
                }
            }
        }
        if self.active != before.active {
            self.pressed = false;
        }
        *self != before
    }

    /// Primary button down at `p`: press the row under it.
    pub fn press(&mut self, layout: &Layout, p: (f32, f32)) -> bool {
        let before = *self;
        self.pointer(layout, Some(p));
        self.pressed = layout.hit(p.0, p.1).is_some();
        *self != before
    }

    /// Primary button up at `p`: the target to run when the press began and ended on the same row.
    pub fn release(&mut self, layout: &Layout, p: (f32, f32)) -> Option<Target> {
        let was = std::mem::replace(&mut self.pressed, false);
        let row = layout.hit(p.0, p.1)?;
        (was && self.active == Some(row)).then(|| layout.target_of(row)).flatten()
    }

    pub fn key(&mut self, layout: &Layout, key: Key) -> KeyOutcome {
        let sel = layout.selectable();
        if sel.is_empty() {
            return if key == Key::Escape { KeyOutcome::Close } else { KeyOutcome::None };
        }
        let pos = self.active.and_then(|a| sel.iter().position(|&i| i == a));
        let move_to = |this: &mut Nav, idx: usize| {
            this.active = Some(sel[idx]);
            this.keyboard = true;
            this.pressed = false;
            KeyOutcome::Moved
        };
        let n = sel.len();
        match key {
            Key::Escape => KeyOutcome::Close,
            Key::Down | Key::Tab => move_to(self, pos.map_or(0, |p| (p + 1) % n)),
            Key::Up | Key::ShiftTab => move_to(self, pos.map_or(n - 1, |p| (p + n - 1) % n)),
            Key::Home => move_to(self, 0),
            Key::End => move_to(self, n - 1),
            Key::Enter | Key::Space => match self.active.and_then(|a| layout.target_of(a)) {
                Some(t) => KeyOutcome::Choose(t),
                // Enter with nothing highlighted picks the primary action.
                None => move_to(self, 0),
            },
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Placement
// ---------------------------------------------------------------------------------------------

/// Integer rectangle in device px (screen coordinates).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IRect {
    pub l: i32,
    pub t: i32,
    pub r: i32,
    pub b: i32,
}

impl IRect {
    pub fn new(l: i32, t: i32, r: i32, b: i32) -> IRect {
        IRect { l, t, r, b }
    }
    pub fn w(&self) -> i32 {
        self.r - self.l
    }
    pub fn h(&self) -> i32 {
        self.b - self.t
    }
    pub fn cx(&self) -> i32 {
        self.l + self.w() / 2
    }
    pub fn cy(&self) -> i32 {
        self.t + self.h() / 2
    }
    pub fn is_empty(&self) -> bool {
        self.r <= self.l || self.b <= self.t
    }
    pub fn intersects(&self, o: &IRect) -> bool {
        self.l < o.r && o.l < self.r && self.t < o.b && o.t < self.b
    }
    #[cfg(test)]
    pub fn inflate(&self, d: i32) -> IRect {
        IRect::new(self.l - d, self.t - d, self.r + d, self.b + d)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Edge {
    Left,
    Top,
    Right,
    Bottom,
}

/// The screen edge the taskbar sits on, from the monitor and its work area: the side with the
/// biggest inset. An auto-hidden taskbar leaves the whole monitor as work area; then the shell's
/// own answer (`ABM_GETTASKBARPOS`) is used, and failing that the bottom.
pub fn taskbar_edge(monitor: IRect, work: IRect, reported: Option<Edge>) -> Edge {
    let insets = [(Edge::Left, work.l - monitor.l), (Edge::Top, work.t - monitor.t), (Edge::Right, monitor.r - work.r), (Edge::Bottom, monitor.b - work.b)];
    let (edge, inset) = insets.iter().copied().fold((Edge::Bottom, 0), |best, e| if e.1 > best.1 { e } else { best });
    if inset > 0 { edge } else { reported.unwrap_or(Edge::Bottom) }
}

/// Top-left of the card, in screen px, for a card of `w` x `h` px.
///
/// `anchor` is the tray icon's rectangle when the shell knows it; the popup is centred on it along the
/// taskbar and clamped into the work area. Without it (the icon is in the hidden overflow, or the
/// shell would not say) the popup goes to the corner of the work area next to the notification area:
/// bottom-right for a bottom or right taskbar, top-right for a top one, bottom-left for a left one.
pub fn place(w: i32, h: i32, anchor: Option<IRect>, work: IRect, edge: Edge, gap: i32) -> (i32, i32) {
    let corner_x = work.r - gap - w;
    let corner_y = work.b - gap - h;
    let (x, y) = match edge {
        Edge::Bottom => (anchor.map_or(corner_x, |a| a.cx() - w / 2), work.b - gap - h),
        Edge::Top => (anchor.map_or(corner_x, |a| a.cx() - w / 2), work.t + gap),
        Edge::Right => (work.r - gap - w, anchor.map_or(corner_y, |a| a.cy() - h / 2)),
        Edge::Left => (work.l + gap, anchor.map_or(corner_y, |a| a.cy() - h / 2)),
    };
    (clamp_axis(x, w, work.l, work.r, gap), clamp_axis(y, h, work.t, work.b, gap))
}

/// Keep `[v, v + len)` inside `[lo + gap, hi - gap)`; the near edge wins when it cannot fit.
fn clamp_axis(v: i32, len: i32, lo: i32, hi: i32, gap: i32) -> i32 {
    v.min(hi - gap - len).max(lo + gap)
}

/// Whether a tray click at `now_ms` should only close (not reopen) the popup that closed itself at
/// `closed_at_ms`: the click that dismissed it by activating the taskbar arrives right after.
pub fn is_reopen_click(closed_at_ms: Option<u64>, now_ms: u64) -> bool {
    closed_at_ms.is_some_and(|c| now_ms.saturating_sub(c) < REOPEN_GUARD_MS)
}

/// A second tray event this soon after the popup opened is the same click reported twice
/// (`WM_LBUTTONUP` + `NIN_SELECT`, or `NIN_SELECT` + `NIN_KEYSELECT` for Enter).
pub const DUPLICATE_GUARD_MS: u64 = 250;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    Open,
    Close,
    Ignore,
}

/// What a tray activation does: close an open popup, open a closed one, and swallow the echoes of the
/// click that just opened or just dismissed it.
pub fn decide(opened_at: Option<u64>, closed_at: Option<u64>, now_ms: u64) -> Decision {
    match opened_at {
        Some(t) if now_ms.saturating_sub(t) < DUPLICATE_GUARD_MS => Decision::Ignore,
        Some(_) => Decision::Close,
        None if is_reopen_click(closed_at, now_ms) => Decision::Ignore,
        None => Decision::Open,
    }
}

/// Whether the shell's rectangle for the tray icon can anchor the popup: non-empty, on this monitor
/// and not inside the work area (the icon lives on the taskbar, outside it). A rectangle inside the
/// work area is the overflow flyout's, and the popup goes to the corner instead.
pub fn anchor_usable(rect: IRect, monitor: IRect, work: IRect) -> bool {
    !rect.is_empty() && rect.intersects(&monitor) && !(rect.l >= work.l && rect.t >= work.t && rect.r <= work.r && rect.b <= work.b)
}

/// The work area of a monitor whose taskbar is hidden (it reports the whole monitor): shrink it by
/// the taskbar's thickness on `edge` so the popup still clears the bar when it slides in.
pub fn work_without_hidden_bar(monitor: IRect, work: IRect, edge: Edge, bar: IRect) -> IRect {
    if work != monitor {
        return work;
    }
    match edge {
        Edge::Bottom => IRect { b: work.b - bar.h().max(0), ..work },
        Edge::Top => IRect { t: work.t + bar.h().max(0), ..work },
        Edge::Left => IRect { l: work.l + bar.w().max(0), ..work },
        Edge::Right => IRect { r: work.r - bar.w().max(0), ..work },
    }
}

/// The popup key for a virtual-key code (`shift`: Shift is held), or `None` for keys it ignores.
pub fn key_from_vk(vk: u32, shift: bool) -> Option<Key> {
    Some(match vk {
        0x26 => Key::Up,
        0x28 => Key::Down,
        0x24 => Key::Home,
        0x23 => Key::End,
        0x09 if shift => Key::ShiftTab,
        0x09 => Key::Tab,
        0x0D => Key::Enter,
        0x20 => Key::Space,
        0x1B => Key::Escape,
        _ => return None,
    })
}

/// Single-line preview of a dictation for the Recent rows: whitespace collapsed, trimmed.
pub fn preview_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn full() -> Content {
        Content { phase: Phase::Ready, floating_pill_visible: true, can_retry: true, hotkey: "Ctrl + Win".into(), recent: vec!["One".into(), "Two".into()] }
    }

    fn labels(l: &Layout) -> Vec<&str> {
        l.rows.iter().filter(|r| r.target.is_some()).map(|r| r.label.as_str()).collect()
    }

    #[test]
    fn rows_follow_the_content() {
        let minimal = layout(&Content::default());
        assert_eq!(labels(&minimal), ["Start dictation", "History", "Dictionary", "Settings", "Show floating pill", "Quit OpenTalk"]);
        let all = layout(&full());
        assert_eq!(
            labels(&all),
            ["Start dictation", "History", "Dictionary", "Settings", "One", "Two", "Hide floating pill", "Retry last dictation", "Quit OpenTalk"]
        );
        let rec = layout(&Content { phase: Phase::Recording, ..Content::default() });
        assert_eq!(rec.rows[1].label, "Stop dictation");
    }

    #[test]
    fn rows_are_stacked_inside_the_card_without_overlap() {
        for c in [Content::default(), full()] {
            let l = layout(&c);
            let mut y = 0.0;
            for r in &l.rows {
                assert!(r.rect.t >= y - 0.001, "row {:?} overlaps the previous one", r.label);
                assert!(r.rect.b <= l.card_h + 0.001);
                assert!(r.rect.l >= 0.0 && r.rect.r <= l.card_w);
                y = r.rect.b;
            }
            let (w, h) = l.window_size();
            assert_eq!((w, h), (l.card_w + 32.0, l.card_h + 32.0));
        }
    }

    #[test]
    fn row_heights_are_compact_and_touch_friendly() {
        let l = layout(&full());
        for r in &l.rows {
            match r.kind {
                Kind::Item | Kind::Recent => assert!((32.0..=36.0).contains(&r.rect.h()), "{}: {}", r.label, r.rect.h()),
                Kind::Primary => assert!(r.rect.h() >= 36.0 && r.rect.h() <= 44.0),
                _ => {}
            }
        }
        assert!(l.card_h < 440.0, "the popup stays small: {}", l.card_h);
        assert!(layout(&Content::default()).card_h < 320.0);
    }

    #[test]
    fn a_phase_change_keeps_the_card_size_so_an_open_popup_refreshes_in_place() {
        for base in [Content::default(), full()] {
            let sizes: Vec<_> = [Phase::Ready, Phase::Recording, Phase::Transcribing]
                .into_iter()
                .map(|phase| {
                    let l = layout(&Content { phase, ..base.clone() });
                    (l.card_h, l.rows.len())
                })
                .collect();
            assert!(sizes.windows(2).all(|w| w[0] == w[1]), "{sizes:?}");
        }
    }

    #[test]
    fn transcribing_disables_the_dictation_row() {
        let l = layout(&Content { phase: Phase::Transcribing, ..Content::default() });
        let row = &l.rows[1];
        assert_eq!(row.label, "Transcribing...");
        assert!(!row.enabled && !row.selectable());
        assert_eq!(l.hit(row.rect.l + 4.0, row.rect.t + 4.0), None);
        assert_eq!(l.selectable().first().map(|&i| l.rows[i].label.as_str()), Some("History"));
    }

    #[test]
    fn hit_testing_finds_rows_and_ignores_chrome() {
        let l = layout(&full());
        for (i, r) in l.rows.iter().enumerate() {
            let (cx, cy) = ((r.rect.l + r.rect.r) / 2.0, (r.rect.t + r.rect.b) / 2.0);
            assert_eq!(l.hit(cx, cy), r.selectable().then_some(i), "{}", r.label);
        }
        // The header, the card padding and the outside never hit.
        assert_eq!(l.hit(100.0, 10.0), None);
        assert_eq!(l.hit(2.0, 60.0), None, "left padding");
        assert_eq!(l.hit(-5.0, 60.0), None);
        assert_eq!(l.hit(100.0, l.card_h + 3.0), None);
        // Edges are half-open: the first pixel of a row hits, the one past its end belongs to the next.
        let h = l.rows.iter().position(|r| r.label == "History").unwrap();
        let r = l.rows[h].rect;
        assert_eq!(l.hit(r.l, r.t), Some(h));
        assert_ne!(l.hit(r.l + 1.0, r.b), Some(h));
    }

    #[test]
    fn every_row_maps_to_a_tray_command() {
        let l = layout(&full());
        let cmds: Vec<_> = l.rows.iter().filter_map(|r| r.target).map(Target::cmd).collect();
        assert_eq!(
            cmds,
            [
                Some(TrayCmd::ToggleListening),
                Some(TrayCmd::OpenHistory),
                Some(TrayCmd::OpenDictionary),
                Some(TrayCmd::OpenSettings),
                None,
                None,
                Some(TrayCmd::ToggleFloatingIcon),
                Some(TrayCmd::RetryLast),
                Some(TrayCmd::Quit),
            ]
        );
    }

    #[test]
    fn arrow_keys_wrap_and_skip_chrome() {
        let l = layout(&Content::default());
        let sel = l.selectable();
        let mut nav = Nav::default();
        assert_eq!(nav.key(&l, Key::Down), KeyOutcome::Moved);
        assert_eq!(nav.active, Some(sel[0]));
        assert!(nav.keyboard);
        for want in &sel[1..] {
            nav.key(&l, Key::Down);
            assert_eq!(nav.active, Some(*want));
        }
        nav.key(&l, Key::Down);
        assert_eq!(nav.active, Some(sel[0]), "wraps to the top");
        nav.key(&l, Key::Up);
        assert_eq!(nav.active, Some(*sel.last().unwrap()), "wraps to the bottom");
        nav.key(&l, Key::Home);
        assert_eq!(nav.active, Some(sel[0]));
        nav.key(&l, Key::End);
        assert_eq!(nav.active, Some(*sel.last().unwrap()));
        let mut nav = Nav::default();
        nav.key(&l, Key::Up);
        assert_eq!(nav.active, Some(*sel.last().unwrap()), "Up from nothing starts at the bottom");
        nav.key(&l, Key::ShiftTab);
        assert_eq!(nav.active, Some(sel[sel.len() - 2]));
        nav.key(&l, Key::Tab);
        assert_eq!(nav.active, Some(*sel.last().unwrap()));
    }

    #[test]
    fn enter_and_space_choose_escape_closes() {
        let l = layout(&Content::default());
        let mut nav = Nav::for_keyboard(&l);
        assert_eq!(nav.key(&l, Key::Enter), KeyOutcome::Choose(Target::Dictation));
        nav.key(&l, Key::Down);
        assert_eq!(nav.key(&l, Key::Space), KeyOutcome::Choose(Target::History));
        assert_eq!(nav.key(&l, Key::Escape), KeyOutcome::Close);
        // Nothing highlighted: Enter highlights the first row instead of firing something.
        let mut fresh = Nav::default();
        assert_eq!(fresh.key(&l, Key::Enter), KeyOutcome::Moved);
        assert_eq!(fresh.active, Some(l.selectable()[0]));
    }

    #[test]
    fn disabled_rows_are_skipped_by_the_keyboard() {
        let l = layout(&Content { phase: Phase::Transcribing, ..Content::default() });
        let mut nav = Nav::for_keyboard(&l);
        assert_eq!(l.rows[nav.active.unwrap()].label, "History");
        assert_eq!(nav.key(&l, Key::Enter), KeyOutcome::Choose(Target::History));
    }

    #[test]
    fn pointer_highlights_presses_and_releases() {
        let l = layout(&full());
        let h = l.rows.iter().position(|r| r.label == "History").unwrap();
        let q = l.rows.iter().position(|r| r.label == "Quit OpenTalk").unwrap();
        let mid = |i: usize| ((l.rows[i].rect.l + l.rows[i].rect.r) / 2.0, (l.rows[i].rect.t + l.rows[i].rect.b) / 2.0);
        let mut nav = Nav::default();
        assert!(nav.pointer(&l, Some(mid(h))));
        assert_eq!(nav.active, Some(h));
        assert!(!nav.keyboard);
        assert!(!nav.pointer(&l, Some(mid(h))), "no change, no redraw");
        // Press and release on the same row chooses it.
        assert!(nav.press(&l, mid(h)));
        assert!(nav.pressed);
        assert_eq!(nav.release(&l, mid(h)), Some(Target::History));
        assert!(!nav.pressed);
        // Press on one row, release on another: nothing.
        nav.press(&l, mid(h));
        nav.pointer(&l, Some(mid(q)));
        assert_eq!(nav.release(&l, mid(q)), None);
        // Release without a press: nothing.
        assert_eq!(nav.release(&l, mid(q)), None);
        // Leaving clears the highlight; over the header it clears too.
        assert!(nav.pointer(&l, None));
        assert_eq!(nav.active, None);
        nav.pointer(&l, Some(mid(h)));
        nav.pointer(&l, Some((100.0, 10.0)));
        assert_eq!(nav.active, None);
    }

    #[test]
    fn keyboard_highlight_survives_the_pointer_leaving() {
        let l = layout(&Content::default());
        let mut nav = Nav::for_keyboard(&l);
        let first = nav.active;
        assert!(!nav.pointer(&l, None));
        assert_eq!(nav.active, first);
        nav.pointer(&l, Some((100.0, 10.0)));
        assert_eq!(nav.active, first, "over the header keeps the key focus");
        // Moving onto a row hands the highlight to the pointer.
        let h = l.selectable()[1];
        let (x, y) = ((l.rows[h].rect.l + l.rows[h].rect.r) / 2.0, (l.rows[h].rect.t + l.rows[h].rect.b) / 2.0);
        nav.pointer(&l, Some((x, y)));
        assert_eq!((nav.active, nav.keyboard), (Some(h), false));
    }

    // ---- placement -----------------------------------------------------------------------

    const MON: IRect = IRect { l: 0, t: 0, r: 1920, b: 1080 };

    #[test]
    fn edge_is_read_from_the_work_area() {
        assert_eq!(taskbar_edge(MON, IRect::new(0, 0, 1920, 1040), None), Edge::Bottom);
        assert_eq!(taskbar_edge(MON, IRect::new(0, 40, 1920, 1080), None), Edge::Top);
        assert_eq!(taskbar_edge(MON, IRect::new(60, 0, 1920, 1080), None), Edge::Left);
        assert_eq!(taskbar_edge(MON, IRect::new(0, 0, 1860, 1080), None), Edge::Right);
        // Auto-hide: no inset anywhere; the shell's answer, else the bottom.
        assert_eq!(taskbar_edge(MON, MON, Some(Edge::Left)), Edge::Left);
        assert_eq!(taskbar_edge(MON, MON, None), Edge::Bottom);
        // A secondary monitor with an offset origin.
        let m2 = IRect::new(-1920, 200, 0, 1280);
        assert_eq!(taskbar_edge(m2, IRect::new(-1920, 200, 0, 1240), Some(Edge::Top)), Edge::Bottom, "the work area wins over a report for another monitor");
    }

    #[test]
    fn bottom_taskbar_corner_and_icon_placement() {
        let work = IRect::new(0, 0, 1920, 1040);
        let (w, h) = (320, 340);
        // No icon rect: bottom-right corner, 12 px from both edges.
        assert_eq!(place(w, h, None, work, Edge::Bottom, 12), (1920 - 12 - 320, 1040 - 12 - 340));
        // Icon near the right: centred on it.
        let icon = IRect::new(1700, 1048, 1724, 1072);
        assert_eq!(place(w, h, Some(icon), work, Edge::Bottom, 12), (1712 - 160, 1040 - 12 - 340));
        // Icon at the far right: clamped to the corner.
        let far = IRect::new(1896, 1048, 1920, 1072);
        assert_eq!(place(w, h, Some(far), work, Edge::Bottom, 12).0, 1920 - 12 - 320);
        // Icon at the far left: clamped to the left gap.
        let left = IRect::new(0, 1048, 24, 1072);
        assert_eq!(place(w, h, Some(left), work, Edge::Bottom, 12).0, 12);
    }

    #[test]
    fn top_left_and_right_taskbars() {
        let (w, h) = (320, 340);
        let top = IRect::new(0, 40, 1920, 1080);
        assert_eq!(place(w, h, None, top, Edge::Top, 12), (1920 - 12 - 320, 40 + 12));
        let left = IRect::new(60, 0, 1920, 1080);
        assert_eq!(place(w, h, None, left, Edge::Left, 12), (60 + 12, 1080 - 12 - 340), "tray sits at the bottom of a vertical bar");
        let right = IRect::new(0, 0, 1860, 1080);
        assert_eq!(place(w, h, None, right, Edge::Right, 12), (1860 - 12 - 320, 1080 - 12 - 340));
        // With an icon on a vertical bar the popup centres on it vertically.
        let icon = IRect::new(1870, 900, 1890, 920);
        assert_eq!(place(w, h, Some(icon), right, Edge::Right, 12), (1860 - 12 - 320, 1080 - 12 - 340), "clamped to the bottom gap");
        let mid = IRect::new(1870, 500, 1890, 520);
        assert_eq!(place(w, h, Some(mid), right, Edge::Right, 12).1, 510 - 170);
        let icon_l = IRect::new(20, 100, 40, 120);
        assert_eq!(place(w, h, Some(icon_l), left, Edge::Left, 12), (72, 12), "clamped to the top gap");
    }

    #[test]
    fn placement_on_a_secondary_monitor_with_negative_origin() {
        let work = IRect::new(-2560, -200, 0, 1240);
        let (w, h) = (320, 340);
        assert_eq!(place(w, h, None, work, Edge::Bottom, 12), (-12 - 320, 1240 - 12 - 340));
        let icon = IRect::new(-400, 1250, -376, 1274);
        assert_eq!(place(w, h, Some(icon), work, Edge::Bottom, 12), (-388 - 160, 1240 - 12 - 340));
        // An icon near the right edge of that monitor is clamped to its corner, not pushed onto the next screen.
        let near = IRect::new(-100, 1250, -76, 1274);
        assert_eq!(place(w, h, Some(near), work, Edge::Bottom, 12).0, -12 - 320);
    }

    #[test]
    fn a_popup_larger_than_the_work_area_pins_to_the_top_left() {
        let work = IRect::new(0, 0, 300, 200);
        assert_eq!(place(320, 340, None, work, Edge::Bottom, 12), (12, 12));
    }

    #[test]
    fn placement_scales_with_dpi() {
        // 150 %: the gap and the popup are bigger in device px, the geometry is the same.
        let work = IRect::new(0, 0, 2880, 1560);
        let s = 1.5f32;
        let (w, h) = ((CARD_W * s) as i32, (300.0 * s) as i32);
        let gap = (EDGE_GAP * s).round() as i32;
        let (x, y) = place(w, h, None, work, Edge::Bottom, gap);
        assert_eq!((x, y), (2880 - gap - w, 1560 - gap - h));
        assert_eq!(gap, 18);
    }

    #[test]
    fn rect_helpers() {
        let r = IRect::new(10, 20, 50, 40);
        assert_eq!((r.w(), r.h(), r.cx(), r.cy()), (40, 20, 30, 30));
        assert!(!r.is_empty() && IRect::new(5, 5, 5, 9).is_empty());
        assert!(r.intersects(&IRect::new(49, 39, 60, 60)));
        assert!(!r.intersects(&IRect::new(50, 20, 60, 40)));
        assert_eq!(r.inflate(2), IRect::new(8, 18, 52, 42));
    }

    #[test]
    fn a_click_right_after_the_popup_closed_does_not_reopen_it() {
        assert!(is_reopen_click(Some(1000), 1100));
        assert!(!is_reopen_click(Some(1000), 1000 + REOPEN_GUARD_MS));
        assert!(!is_reopen_click(None, 5));
        assert!(is_reopen_click(Some(1000), 900), "a clock step back counts as simultaneous");
    }

    #[test]
    fn activation_decisions() {
        // Closed, nothing recent: open.
        assert_eq!(decide(None, None, 5000), Decision::Open);
        assert_eq!(decide(None, Some(1000), 5000), Decision::Open);
        // Just dismissed by the click on the tray: that click must not reopen it.
        assert_eq!(decide(None, Some(4900), 5000), Decision::Ignore);
        // Open: a later click closes, an echo of the opening click is ignored.
        assert_eq!(decide(Some(4900), None, 5000), Decision::Ignore);
        assert_eq!(decide(Some(1000), None, 5000), Decision::Close);
    }

    #[test]
    fn only_a_taskbar_rect_anchors_the_popup() {
        let work = IRect::new(0, 0, 1920, 1040);
        // On the taskbar: fine. Fully inside the work area (overflow flyout): rejected.
        assert!(anchor_usable(IRect::new(1700, 1048, 1724, 1072), MON, work));
        assert!(!anchor_usable(IRect::new(1700, 900, 1724, 924), MON, work));
        // Straddling the edge still counts. Empty or on another monitor: rejected.
        assert!(anchor_usable(IRect::new(1700, 1030, 1724, 1054), MON, work));
        assert!(!anchor_usable(IRect::new(0, 0, 0, 0), MON, work));
        assert!(!anchor_usable(IRect::new(2000, 1048, 2024, 1072), MON, work));
    }

    #[test]
    fn a_hidden_taskbar_still_reserves_its_thickness() {
        let bar = IRect::new(0, 1032, 1920, 1080);
        assert_eq!(work_without_hidden_bar(MON, MON, Edge::Bottom, bar), IRect::new(0, 0, 1920, 1032));
        assert_eq!(work_without_hidden_bar(MON, MON, Edge::Top, IRect::new(0, 0, 1920, 48)), IRect::new(0, 48, 1920, 1080));
        assert_eq!(work_without_hidden_bar(MON, MON, Edge::Left, IRect::new(0, 0, 60, 1080)), IRect::new(60, 0, 1920, 1080));
        assert_eq!(work_without_hidden_bar(MON, MON, Edge::Right, IRect::new(1860, 0, 1920, 1080)), IRect::new(0, 0, 1860, 1080));
        // A visible taskbar already shows in the work area: untouched.
        let work = IRect::new(0, 0, 1920, 1040);
        assert_eq!(work_without_hidden_bar(MON, work, Edge::Bottom, bar), work);
    }

    #[test]
    fn virtual_keys_map_to_popup_keys() {
        assert_eq!(key_from_vk(0x28, false), Some(Key::Down));
        assert_eq!(key_from_vk(0x26, false), Some(Key::Up));
        assert_eq!(key_from_vk(0x09, false), Some(Key::Tab));
        assert_eq!(key_from_vk(0x09, true), Some(Key::ShiftTab));
        assert_eq!(key_from_vk(0x0D, false), Some(Key::Enter));
        assert_eq!(key_from_vk(0x20, false), Some(Key::Space));
        assert_eq!(key_from_vk(0x1B, false), Some(Key::Escape));
        assert_eq!(key_from_vk(0x24, false), Some(Key::Home));
        assert_eq!(key_from_vk(0x23, false), Some(Key::End));
        assert_eq!(key_from_vk(0x41, false), None);
    }

    #[test]
    fn previews_are_one_line() {
        assert_eq!(preview_line("  Hello\n\n  world \t again "), "Hello world again");
        assert_eq!(preview_line(""), "");
    }
}
