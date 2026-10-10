//! One open page: its model, the items it last produced, the layout, scroll position, keyboard
//! focus, hover / press state and the text editor of the focused field. Everything here is pure
//! (no Win32): the window feeds pointer and key input in and gets [`Resp`] back, the pager runs
//! the effects. Positions are body-local logical px: x over the 380 px card, y from the top of the
//! scrolling body.

use super::edit::{EditKey, Editor, LINE_H, NavKey};
use super::layout::{self, CONTENT_W, Foc, Layout, Part};
use super::metrics::TextMetrics;
use super::scroll::{BAR_HIT_W, Scroll};
use super::{CARD_W_PAGE, Cx, Effect, Event, FieldKind, Item, ItemId, PageModel, Route, Row, ToolTone, Trailing};
use crate::daemon::tray::popup::R;

/// The system clipboard as the session sees it.
pub trait Clip {
    fn get(&mut self) -> Option<String>;
    fn set(&mut self, text: &str) -> bool;
}

/// A clipboard that holds nothing (tests that do not care).
#[derive(Default)]
pub struct MemClip(pub Option<String>);

impl Clip for MemClip {
    fn get(&mut self) -> Option<String> {
        self.0.clone()
    }
    fn set(&mut self, text: &str) -> bool {
        self.0 = Some(text.to_string());
        true
    }
}

/// Everything an input handler needs besides the session.
pub struct Env<'a, 'c> {
    pub cx: &'a mut Cx<'c>,
    pub m: &'a dyn TextMetrics,
    pub clip: &'a mut dyn Clip,
}

/// Keys the page handles (printable text arrives separately through `ch`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum K {
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PgUp,
    PgDn,
    Tab,
    Enter,
    Space,
    Esc,
    Backspace,
    Delete,
    /// A letter pressed with Ctrl (A, C, X, V).
    Letter(char),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeyEv {
    pub k: K,
    pub shift: bool,
    pub ctrl: bool,
    pub alt: bool,
}

impl KeyEv {
    #[cfg(test)]
    pub fn plain(k: K) -> KeyEv {
        KeyEv { k, shift: false, ctrl: false, alt: false }
    }
}

/// The page key for a virtual-key code.
pub fn key_from_vk(vk: u32, ctrl: bool) -> Option<K> {
    Some(match vk {
        0x26 => K::Up,
        0x28 => K::Down,
        0x25 => K::Left,
        0x27 => K::Right,
        0x24 => K::Home,
        0x23 => K::End,
        0x21 => K::PgUp,
        0x22 => K::PgDn,
        0x09 => K::Tab,
        0x0D => K::Enter,
        0x20 => K::Space,
        0x1B => K::Esc,
        0x08 => K::Backspace,
        0x2E => K::Delete,
        0x41 | 0x43 | 0x58 | 0x56 if ctrl => K::Letter(char::from_u32(vk).unwrap_or('A').to_ascii_lowercase()),
        _ => return None,
    })
}

/// What an input did.
#[derive(Default)]
pub struct Resp {
    pub redraw: bool,
    pub effects: Vec<Effect>,
    /// The user asked to leave the page (Esc, Backspace, Alt+Left).
    pub back: bool,
    /// The key was consumed (the window does not pass it on).
    pub handled: bool,
}

impl Resp {
    fn redraw() -> Resp {
        Resp { redraw: true, ..Resp::default() }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Press {
    pub foc: Foc,
    pub part: Part,
}

/// The text field being edited.
#[derive(Clone, Debug)]
pub struct FieldEdit {
    pub id: ItemId,
    pub ed: Editor,
    /// The last committed value (what Esc restores).
    pub base: String,
    /// The primary button is down in the field: moves extend the selection.
    pub dragging: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CursorKind {
    Arrow,
    IBeam,
    Hand,
}

/// Text origin inside an input box (content space): left edge and the top of the first line.
pub fn text_origin(ctl: R, multiline: bool) -> (f32, f32) {
    (ctl.l + 10.0, if multiline { ctl.t + 8.0 } else { ctl.cy() - LINE_H * 0.5 })
}

/// Inner size of the text area of an input box.
pub fn text_area(ctl: R, multiline: bool) -> (f32, f32) {
    (ctl.w() - 20.0, if multiline { ctl.h() - 16.0 } else { LINE_H })
}

pub struct Session {
    pub route: Route,
    pub model: Box<dyn PageModel>,
    pub items: Vec<Item>,
    pub layout: Layout,
    pub scroll: Scroll,
    pub focus: Option<Foc>,
    /// Draw the keyboard focus ring (focus came from the keyboard).
    pub ring: bool,
    pub hover: Option<(Foc, Part)>,
    pub press: Option<Press>,
    pub edit: Option<FieldEdit>,
    /// Hotkey capture in progress: the field and the live preview.
    pub capture: Option<(ItemId, String)>,
    /// The pointer is over the scrollbar track.
    pub bar_hover: bool,
    /// Last pointer position (body-local), to refresh hover after the content moves.
    pub ptr: Option<(f32, f32)>,
}

impl Session {
    pub fn new(route: Route, model: Box<dyn PageModel>, view_h: f32) -> Session {
        Session {
            route,
            model,
            items: Vec::new(),
            layout: layout::layout(&[], 0, None, &|_, _, _| 0.0, CONTENT_W),
            scroll: Scroll::new(view_h),
            focus: None,
            ring: false,
            hover: None,
            press: None,
            edit: None,
            capture: None,
            bar_hover: false,
            ptr: None,
        }
    }

    /// Ask the model for its items and lay them out. Focus, scroll and a field being edited stay.
    pub fn rebuild(&mut self, cx: &Cx, m: &dyn TextMetrics) {
        let mut items = self.model.items(cx);
        let tail_start = items.len();
        items.extend(self.model.tail_items(cx));
        let virt = self.model.virtual_rows().map(|v| (v.count, v.row_h));
        let measure = |s: &str, px: f32, w: i32| m.width(s, px, w);
        self.layout = layout::layout(&items, tail_start, virt, &measure, CONTENT_W);
        self.items = items;
        self.scroll.resize(self.scroll.view_h, self.layout.content_h);
        if self.focus.is_some_and(|f| self.layout.pos_of(f).is_none()) {
            self.focus = None;
        }
        if let Some(fe) = &mut self.edit {
            let ctl = self.layout.placed_of(&self.items, fe.id).filter(|p| matches!(self.items[p.idx], Item::Field { .. })).map(|p| p.ctl);
            match ctl {
                Some(ctl) => fe.ed.inner_w = text_area(ctl, fe.ed.multiline).0,
                None => self.edit = None,
            }
        }
        self.refresh_hover();
    }

    /// A pointer drag is in progress (scrollbar thumb or text selection): the pager keeps feeding
    /// it positions clamped to the body while the button stays down.
    pub fn dragging(&self) -> bool {
        self.scroll.dragging() || self.edit.as_ref().is_some_and(|fe| fe.dragging)
    }

    /// The item of a focus target (a virtual row is built on demand).
    pub fn item_of(&self, f: Foc) -> Option<Item> {
        match f {
            Foc::Item(id) => self.items.iter().find(|i| i.id() == Some(id)).cloned(),
            Foc::Row(i) => self.model.virtual_rows().filter(|v| i < v.count).map(|v| (v.row)(i)),
        }
    }

    fn virt_row(&self, i: usize) -> Option<Row> {
        match self.item_of(Foc::Row(i)) {
            Some(Item::Row(r)) => Some(r),
            _ => None,
        }
    }

    fn fire(&mut self, f: Foc, ev: Event, cx: &mut Cx) -> Vec<Effect> {
        match f {
            Foc::Item(id) => self.model.event(id, ev, cx),
            Foc::Row(i) => self.model.row_event(i, ev, cx),
        }
    }

    fn hit(&self, x: f32, y: f32) -> Option<(Foc, Part)> {
        self.layout.hit(&self.items, x, y + self.scroll.offset, &|i| self.virt_row(i))
    }

    fn refresh_hover(&mut self) {
        self.hover = match self.ptr {
            Some((x, y)) if y >= 0.0 && y < self.scroll.view_h => self.hit(x, y),
            _ => None,
        };
    }

    pub fn cursor(&self) -> CursorKind {
        match self.hover {
            Some((Foc::Item(id), Part::Body)) => match self.items.iter().find(|i| i.id() == Some(id)) {
                Some(Item::Field { kind: FieldKind::Hotkey, .. }) => CursorKind::Hand,
                Some(Item::Field { .. }) => CursorKind::IBeam,
                _ => CursorKind::Hand,
            },
            Some(_) => CursorKind::Hand,
            None if self.bar_hover => CursorKind::Arrow,
            None => CursorKind::Arrow,
        }
    }

    // -----------------------------------------------------------------------------------------
    // Editing
    // -----------------------------------------------------------------------------------------

    fn begin_edit(&mut self, id: ItemId, select_all: bool) {
        let Some(p) = self.layout.placed_of(&self.items, id) else { return };
        let Item::Field { value, kind, max_chars, .. } = &self.items[p.idx] else { return };
        if matches!(kind, FieldKind::Hotkey) {
            return;
        }
        let multi = matches!(kind, FieldKind::Multiline { .. });
        let mut ed = Editor::new(value, *kind, *max_chars, text_area(p.ctl, multi).0);
        if select_all && !multi {
            ed.select_all();
        }
        self.edit = Some(FieldEdit { id, ed, base: value.clone(), dragging: false });
    }

    /// Commit the edited field if its text differs from the last committed value.
    pub fn commit_edit(&mut self, cx: &mut Cx) -> Vec<Effect> {
        let Some(fe) = &mut self.edit else { return Vec::new() };
        let text = fe.ed.text();
        if text == fe.base {
            return Vec::new();
        }
        fe.base = text.clone();
        let id = fe.id;
        self.model.event(id, Event::Committed(text), cx)
    }

    fn end_edit(&mut self, cx: &mut Cx) -> Vec<Effect> {
        let fx = self.commit_edit(cx);
        self.edit = None;
        fx
    }

    /// Commit a dirty field (page leave, popup close) without leaving the page.
    pub fn flush(&mut self, cx: &mut Cx, m: &dyn TextMetrics) -> Vec<Effect> {
        let fx = self.commit_edit(cx);
        if !fx.is_empty() {
            self.rebuild(cx, m);
        }
        fx
    }

    fn set_focus(&mut self, f: Option<Foc>, ring: bool, cx: &mut Cx) -> Vec<Effect> {
        let mut fx = Vec::new();
        let same_field = matches!((f, &self.edit), (Some(Foc::Item(id)), Some(fe)) if fe.id == id);
        if !same_field {
            fx = self.end_edit(cx);
        }
        self.focus = f;
        self.ring = ring;
        if let (Some(Foc::Item(id)), false) = (f, same_field) {
            self.begin_edit(id, ring);
        }
        if let Some(r) = f.and_then(|f| self.layout.rect_of(&self.items, f)) {
            self.scroll.reveal(r.t, r.b);
        }
        fx
    }

    pub fn focus_item(&mut self, id: ItemId, cx: &mut Cx, m: &dyn TextMetrics) -> Vec<Effect> {
        if self.layout.pos_of(Foc::Item(id)).is_none() {
            return Vec::new();
        }
        let fx = self.set_focus(Some(Foc::Item(id)), true, cx);
        if let Some(fe) = &mut self.edit {
            let (w, h) = self.layout.placed_of(&self.items, id).map_or((CONTENT_W, LINE_H), |p| text_area(p.ctl, fe.ed.multiline));
            fe.ed.follow_caret(m, w, h);
        }
        fx
    }

    // -----------------------------------------------------------------------------------------
    // Activation
    // -----------------------------------------------------------------------------------------

    fn activation(&self, f: Foc, part: Part) -> Option<Event> {
        Some(match self.item_of(f)? {
            Item::Toggle { on, .. } => Event::Toggled(!on),
            Item::Segmented { options, sel, .. } => match part {
                Part::Seg(i) if i != sel => Event::Picked(i),
                Part::Seg(_) => return None,
                _ => Event::Picked((sel + 1) % options.len().max(1)),
            },
            Item::Choice { .. } => Event::Activated,
            Item::Radio { on, .. } => {
                if on {
                    return None;
                }
                Event::Toggled(true)
            }
            Item::Button { enabled, .. } => {
                if !enabled {
                    return None;
                }
                Event::Pressed
            }
            Item::Tools { tools, .. } => match part {
                Part::Seg(i) => {
                    if !tools.get(i).is_some_and(|t| t.enabled) {
                        return None;
                    }
                    Event::Picked(i)
                }
                // Enter, or a tap on the label: the primary button, else the first safe one
                _ => {
                    let pick = |f: &dyn Fn(&super::Tool) -> bool| tools.iter().position(|t| t.enabled && f(t));
                    Event::Picked(pick(&|t| t.tone == ToolTone::Primary).or_else(|| pick(&|t| t.tone != ToolTone::Danger)).or_else(|| pick(&|_| true))?)
                }
            },
            Item::Row(r) => match (part, r.trailing) {
                (Part::Trailing, Trailing::Delete) => Event::Deleted,
                (Part::Trailing, Trailing::Copy) => Event::Copy,
                _ => Event::Activated,
            },
            Item::Field { kind: FieldKind::Hotkey, .. } => Event::Activated,
            _ => return None,
        })
    }

    fn activate(&mut self, f: Foc, part: Part, env: &mut Env) -> Vec<Effect> {
        match self.activation(f, part) {
            Some(ev) => {
                let fx = self.fire(f, ev, env.cx);
                self.rebuild(env.cx, env.m);
                fx
            }
            None => Vec::new(),
        }
    }

    /// Where the caret of the edited field is, body-local: x and the bottom of its line (the IME
    /// puts its composition and candidate windows there).
    pub fn caret_pos(&self, m: &dyn TextMetrics) -> Option<(f32, f32)> {
        let fe = self.edit.as_ref()?;
        let p = self.layout.placed_of(&self.items, fe.id)?;
        let (ox, oy) = text_origin(p.ctl, fe.ed.multiline);
        let v = fe.ed.vis(m);
        let li = v.line_of(fe.ed.caret);
        Some((ox - fe.ed.scroll_x + v.x_of(fe.ed.caret), oy + (li + 1) as f32 * LINE_H - fe.ed.scroll_y - self.scroll.offset))
    }

    // -----------------------------------------------------------------------------------------
    // Pointer
    // -----------------------------------------------------------------------------------------

    fn on_bar(&self, x: f32, y: f32) -> bool {
        self.scroll.scrollable() && x >= CARD_W_PAGE - BAR_HIT_W && y >= 0.0 && y < self.scroll.view_h
    }

    /// Caret position in the edited field for a body-local pointer position.
    fn caret_at(&self, x: f32, y: f32, m: &dyn TextMetrics) -> Option<(usize, f32)> {
        let fe = self.edit.as_ref()?;
        let p = self.layout.placed_of(&self.items, fe.id)?;
        let (ox, oy) = text_origin(p.ctl, fe.ed.multiline);
        let line = if fe.ed.multiline { (((y + self.scroll.offset - oy + fe.ed.scroll_y) / LINE_H).floor().max(0.0)) as usize } else { 0 };
        let xr = x - ox + fe.ed.scroll_x;
        let v = fe.ed.vis(m);
        let li = line.min(v.lines.len() - 1);
        Some((li, xr))
    }

    /// The pointer moved to (x, y) (body-local), or left the window (`None`).
    pub fn pointer(&mut self, p: Option<(f32, f32)>, buttons: bool, env: &mut Env) -> Resp {
        let Some((x, y)) = p else {
            let had = self.hover.is_some() || self.bar_hover;
            self.ptr = None;
            self.hover = None;
            self.bar_hover = false;
            return Resp { redraw: had, ..Resp::default() };
        };
        self.ptr = Some((x, y));
        if self.scroll.dragging() {
            return Resp { redraw: self.scroll.bar_drag(y), ..Resp::default() };
        }
        if buttons && self.edit.as_ref().is_some_and(|fe| fe.dragging) && let Some((line, xr)) = self.caret_at(x, y, env.m) {
            let fe = self.edit.as_mut().expect("checked");
            let moved = fe.ed.click(line, xr, true, env.m);
            if let Some(p) = self.layout.placed_of(&self.items, fe.id) {
                let (w, h) = text_area(p.ctl, fe.ed.multiline);
                fe.ed.follow_caret(env.m, w, h);
            }
            return Resp { redraw: moved, ..Resp::default() };
        }
        let bar = self.on_bar(x, y);
        let hover = if bar { None } else if y >= 0.0 && y < self.scroll.view_h { self.hit(x, y) } else { None };
        let changed = hover != self.hover || bar != self.bar_hover;
        self.hover = hover;
        self.bar_hover = bar;
        Resp { redraw: changed, ..Resp::default() }
    }

    /// Primary button down at (x, y) (body-local). `dbl` is the second click of a double click.
    pub fn press(&mut self, x: f32, y: f32, dbl: bool, env: &mut Env) -> Resp {
        self.ptr = Some((x, y));
        if self.on_bar(x, y) {
            return Resp { redraw: self.scroll.bar_press(y), ..Resp::default() };
        }
        let target = self.hit(x, y).filter(|_| y >= 0.0 && y < self.scroll.view_h);
        // A click anywhere but the hotkey box being captured ends the capture: the hook swallows
        // the whole keyboard while it runs, so it must not outlive the user's attention.
        let end_capture = self.capture.as_ref().is_some_and(|(cid, _)| target.map(|t| t.0) != Some(Foc::Item(*cid)));
        let Some((foc, part)) = target else {
            // A click on nothing: leave the field.
            let mut fx = self.set_focus(None, false, env.cx);
            if end_capture {
                fx.push(Effect::EndHotkeyCapture);
            }
            self.press = None;
            self.rebuild(env.cx, env.m);
            return Resp { redraw: true, effects: fx, ..Resp::default() };
        };
        let field = matches!(self.item_of(foc), Some(Item::Field { kind, .. }) if !matches!(kind, FieldKind::Hotkey));
        let mut fx = self.set_focus(Some(foc), false, env.cx);
        if field {
            // set_focus created the editor (without select-all: this was a click).
            if let Some((line, xr)) = self.caret_at(x, y, env.m) {
                let fe = self.edit.as_mut().expect("caret_at found the editor");
                fe.ed.click(line, xr, false, env.m);
                if dbl {
                    let c = fe.ed.caret;
                    fe.ed.select_word_at(c);
                }
                fe.dragging = true;
            }
            self.press = None;
        } else {
            self.press = Some(Press { foc, part });
        }
        if end_capture {
            fx.push(Effect::EndHotkeyCapture);
        }
        if !fx.is_empty() {
            self.rebuild(env.cx, env.m);
        }
        Resp { redraw: true, effects: std::mem::take(&mut fx), ..Resp::default() }
    }

    /// Primary button up at (x, y): activates the pressed item when released over it.
    pub fn release(&mut self, x: f32, y: f32, env: &mut Env) -> Resp {
        self.ptr = Some((x, y));
        self.scroll.bar_release();
        if let Some(fe) = &mut self.edit {
            fe.dragging = false;
        }
        let Some(pr) = self.press.take() else { return Resp::redraw() };
        let over = if y >= 0.0 && y < self.scroll.view_h { self.hit(x, y) } else { None };
        let mut r = Resp::redraw();
        if over == Some((pr.foc, pr.part)) {
            r.effects = self.activate(pr.foc, pr.part, env);
        }
        r
    }

    pub fn wheel(&mut self, delta: i32) -> Resp {
        let moved = self.scroll.wheel(delta);
        if moved {
            self.refresh_hover();
        }
        Resp { redraw: moved, ..Resp::default() }
    }

    // -----------------------------------------------------------------------------------------
    // Keyboard
    // -----------------------------------------------------------------------------------------

    /// Printable text (`WM_CHAR`, IME results, paste from a test).
    pub fn text(&mut self, s: &str, env: &mut Env) -> Resp {
        let Some(fe) = &mut self.edit else { return Resp::default() };
        if !fe.ed.insert(s) {
            return Resp { handled: true, ..Resp::default() };
        }
        self.after_edit(true, env)
    }

    fn after_edit(&mut self, changed: bool, env: &mut Env) -> Resp {
        let Some(fe) = &mut self.edit else { return Resp::default() };
        let id = fe.id;
        let text = fe.ed.text();
        if let Some(p) = self.layout.placed_of(&self.items, id) {
            let (w, h) = text_area(p.ctl, fe.ed.multiline);
            fe.ed.follow_caret(env.m, w, h);
        }
        let mut fx = Vec::new();
        if changed {
            fx = self.model.event(id, Event::Edited(text), env.cx);
            self.rebuild(env.cx, env.m);
        }
        Resp { redraw: true, effects: fx, handled: true, ..Resp::default() }
    }

    pub fn key(&mut self, ev: KeyEv, env: &mut Env) -> Resp {
        if ev.alt {
            return Resp { back: ev.k == K::Left, handled: true, ..Resp::default() };
        }
        if self.capture.is_some() {
            // The low-level hook owns the keyboard while a combination is captured.
            return Resp { handled: true, ..Resp::default() };
        }
        if self.edit.is_some() {
            return self.key_editing(ev, env);
        }
        self.key_nav(ev, env)
    }

    fn step_focus(&mut self, forward: bool, env: &mut Env) -> Resp {
        let next = self.layout.step(self.focus, forward);
        let fx = self.set_focus(next, true, env.cx);
        if let (Some(fe), Some(Foc::Item(id))) = (&mut self.edit, next)
            && let Some(p) = self.layout.placed_of(&self.items, id)
        {
            let (w, h) = text_area(p.ctl, fe.ed.multiline);
            fe.ed.follow_caret(env.m, w, h);
        }
        if !fx.is_empty() {
            self.rebuild(env.cx, env.m);
        }
        Resp { redraw: true, effects: fx, handled: true, ..Resp::default() }
    }

    fn key_nav(&mut self, ev: KeyEv, env: &mut Env) -> Resp {
        let handled = Resp { handled: true, ..Resp::default() };
        match ev.k {
            K::Tab => self.step_focus(!ev.shift, env),
            K::Down => self.step_focus(true, env),
            K::Up => self.step_focus(false, env),
            K::Home if ev.ctrl => Resp { redraw: self.scroll.home(), ..handled },
            K::End if ev.ctrl => Resp { redraw: self.scroll.end(), ..handled },
            K::Home | K::End => {
                let to = if ev.k == K::Home { self.layout.first() } else { self.layout.last() };
                let fx = self.set_focus(to, true, env.cx);
                Resp { redraw: true, effects: fx, ..handled }
            }
            K::PgUp | K::PgDn => {
                let moved = self.scroll.page(ev.k == K::PgDn);
                self.refresh_hover();
                Resp { redraw: moved, ..handled }
            }
            K::Left | K::Right => {
                if let Some(Item::Segmented { options, sel, .. }) = self.focus.and_then(|f| self.item_of(f)) {
                    let to = if ev.k == K::Left { sel.checked_sub(1) } else { (sel + 1 < options.len()).then_some(sel + 1) };
                    if let (Some(to), Some(f)) = (to, self.focus) {
                        let fx = self.fire(f, Event::Picked(to), env.cx);
                        self.rebuild(env.cx, env.m);
                        return Resp { redraw: true, effects: fx, ..handled };
                    }
                }
                handled
            }
            K::Enter | K::Space => {
                let Some(f) = self.focus else { return handled };
                // Enter on a text field that was left with Esc starts editing it again (Space is
                // left alone: its WM_CHAR would replace the selected text).
                if let (K::Enter, Foc::Item(id), Some(Item::Field { kind, .. })) = (ev.k, f, self.item_of(f))
                    && !matches!(kind, FieldKind::Hotkey)
                {
                    self.begin_edit(id, true);
                    return Resp { redraw: true, ..handled };
                }
                let fx = self.activate(f, Part::Body, env);
                Resp { redraw: true, effects: fx, ..handled }
            }
            K::Delete => {
                if let Some(f) = self.focus
                    && matches!(self.item_of(f), Some(Item::Row(Row { trailing: Trailing::Delete, .. })))
                {
                    let fx = self.activate(f, Part::Trailing, env);
                    return Resp { redraw: true, effects: fx, ..handled };
                }
                handled
            }
            K::Esc | K::Backspace => Resp { back: true, ..handled },
            K::Letter(_) => handled,
        }
    }

    fn key_editing(&mut self, ev: KeyEv, env: &mut Env) -> Resp {
        let multiline = self.edit.as_ref().is_some_and(|fe| fe.ed.multiline);
        let edit_key = match ev.k {
            K::Left => Some(EditKey::Nav(NavKey::Left)),
            K::Right => Some(EditKey::Nav(NavKey::Right)),
            K::Home => Some(EditKey::Nav(NavKey::Home)),
            K::End => Some(EditKey::Nav(NavKey::End)),
            K::Up if multiline => Some(EditKey::Nav(NavKey::Up)),
            K::Down if multiline => Some(EditKey::Nav(NavKey::Down)),
            K::Backspace => Some(EditKey::Backspace),
            K::Delete => Some(EditKey::Delete),
            K::Enter => Some(EditKey::Enter),
            K::Letter('a') => Some(EditKey::SelectAll),
            K::Letter('c') => Some(EditKey::Copy),
            K::Letter('x') => Some(EditKey::Cut),
            K::Letter('v') => Some(EditKey::Paste),
            _ => None,
        };
        if let Some(k) = edit_key {
            let paste = if k == EditKey::Paste { env.clip.get() } else { None };
            let fe = self.edit.as_mut().expect("editing");
            // Up on the first line / Down on the last leave the text area.
            if multiline && matches!(k, EditKey::Nav(NavKey::Up | NavKey::Down)) {
                let v = fe.ed.vis(env.m);
                let li = v.line_of(fe.ed.caret);
                let edge = if k == EditKey::Nav(NavKey::Up) { li == 0 } else { li + 1 == v.lines.len() };
                if edge && !ev.shift {
                    return self.step_focus(k == EditKey::Nav(NavKey::Down), env);
                }
            }
            let r = fe.ed.apply(k, ev.shift, ev.ctrl, env.m, paste.as_deref());
            if let Some(c) = &r.clip {
                env.clip.set(c);
            }
            let mut resp = self.after_edit(r.changed, env);
            if r.commit {
                resp.effects.extend(self.commit_edit(env.cx));
                self.rebuild(env.cx, env.m);
            }
            resp.redraw |= r.moved;
            return resp;
        }
        match ev.k {
            K::Tab => self.step_focus(!ev.shift, env),
            K::Up => self.step_focus(false, env),
            K::Down => self.step_focus(true, env),
            K::PgUp | K::PgDn => {
                let moved = self.scroll.page(ev.k == K::PgDn);
                Resp { redraw: moved, handled: true, ..Resp::default() }
            }
            K::Esc => {
                let fe = self.edit.as_mut().expect("editing");
                let text = fe.ed.text();
                if text != fe.base {
                    // Revert the buffer to the last committed value.
                    let base = fe.base.clone();
                    let id = fe.id;
                    fe.ed.set_text(&base);
                    let fx = self.model.event(id, Event::Edited(base), env.cx);
                    self.rebuild(env.cx, env.m);
                    Resp { redraw: true, effects: fx, handled: true, ..Resp::default() }
                } else {
                    // Clean: leave the field (focus stays on it).
                    self.edit = None;
                    Resp { redraw: true, handled: true, ..Resp::default() }
                }
            }
            _ => Resp { handled: true, ..Resp::default() },
        }
    }

    // -----------------------------------------------------------------------------------------
    // Model callbacks
    // -----------------------------------------------------------------------------------------

    pub fn capture_update(&mut self, text: String, env: &mut Env) -> Vec<Effect> {
        let Some((id, _)) = self.capture.clone() else { return Vec::new() };
        self.capture = Some((id, text.clone()));
        let fx = self.model.event(id, Event::CaptureUpdate(text), env.cx);
        self.rebuild(env.cx, env.m);
        fx
    }

    pub fn capture_done(&mut self, accel: String, env: &mut Env) -> Vec<Effect> {
        let Some((id, _)) = self.capture.take() else { return Vec::new() };
        if accel.is_empty() {
            // The capture was cancelled (Esc alone): nothing to validate or save.
            return Vec::new();
        }
        let fx = self.model.event(id, Event::CaptureDone(accel), env.cx);
        self.rebuild(env.cx, env.m);
        fx
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::config::Config;
    use crate::daemon::tray::page::layout::seg_rect;
    use crate::daemon::tray::page::metrics::FixedMetrics;
    use crate::daemon::tray::page::{BtnStyle, ConfirmId};

    /// A scriptable page: records every event, builds items from `items`.
    struct Rec {
        items: Vec<Item>,
        log: std::rc::Rc<std::cell::RefCell<Vec<(ItemId, Event)>>>,
        rows: usize,
        row_log: std::rc::Rc<std::cell::RefCell<Vec<(usize, Event)>>>,
        reply: Vec<Effect>,
    }

    type Log = std::rc::Rc<std::cell::RefCell<Vec<(ItemId, Event)>>>;
    type RowLog = std::rc::Rc<std::cell::RefCell<Vec<(usize, Event)>>>;

    impl PageModel for Rec {
        fn title(&self) -> String {
            "Rec".into()
        }
        fn items(&self, _cx: &Cx) -> Vec<Item> {
            self.items.clone()
        }
        fn event(&mut self, id: ItemId, ev: Event, _cx: &mut Cx) -> Vec<Effect> {
            // A field model stores the committed value, like a real page.
            if let Event::Committed(t) = &ev {
                for it in &mut self.items {
                    if let Item::Field { id: i, value, .. } = it
                        && *i == id
                    {
                        *value = t.clone();
                    }
                }
            }
            if let Event::Toggled(v) = &ev {
                for it in &mut self.items {
                    if let Item::Toggle { id: i, on, .. } = it
                        && *i == id
                    {
                        *on = *v;
                    }
                }
            }
            self.log.borrow_mut().push((id, ev));
            self.reply.clone()
        }
        fn virtual_rows(&self) -> Option<crate::daemon::tray::page::VirtualRows<'_>> {
            (self.rows > 0).then(|| crate::daemon::tray::page::VirtualRows {
                count: self.rows,
                row_h: 44.0,
                row: Box::new(|i| {
                    let mut r = Row::new(i as u16, format!("row {i}"), 44.0);
                    r.trailing = Trailing::Delete;
                    Item::Row(r)
                }),
            })
        }
        fn row_event(&mut self, index: usize, ev: Event, _cx: &mut Cx) -> Vec<Effect> {
            self.row_log.borrow_mut().push((index, ev));
            Vec::new()
        }
        fn confirmed(&mut self, _id: ConfirmId, _cx: &mut Cx) -> Vec<Effect> {
            Vec::new()
        }
    }

    struct T {
        s: Session,
        cfg: Config,
        clip: MemClip,
        log: Log,
        rows: RowLog,
    }

    const VIEW_H: f32 = 300.0;

    fn field(id: ItemId, value: &str, kind: FieldKind) -> Item {
        Item::Field { id, label: "F".into(), hint: "hint".into(), value: value.into(), kind, error: None, max_chars: 0 }
    }

    fn toggle(id: ItemId) -> Item {
        Item::Toggle { id, label: "T".into(), desc: String::new(), on: false }
    }

    fn setup(items: Vec<Item>, rows: usize) -> T {
        let log: Log = Default::default();
        let row_log: RowLog = Default::default();
        let model = Rec { items, log: log.clone(), rows, row_log: row_log.clone(), reply: Vec::new() };
        let mut s = Session::new(Route::SettingsIndex, Box::new(model), VIEW_H);
        let mut cfg = Config::default();
        s.rebuild(&Cx::new(&mut cfg, 0), &FixedMetrics);
        T { s, cfg, clip: MemClip::default(), log, rows: row_log }
    }

    impl T {
        fn env<R>(&mut self, f: impl FnOnce(&mut Session, &mut Env) -> R) -> R {
            let mut cx = Cx::new(&mut self.cfg, 0);
            let mut env = Env { cx: &mut cx, m: &FixedMetrics, clip: &mut self.clip };
            f(&mut self.s, &mut env)
        }
        fn click(&mut self, x: f32, y: f32) -> Resp {
            self.env(|s, e| {
                s.press(x, y, false, e);
                s.release(x, y, e)
            })
        }
        fn click_item(&mut self, id: ItemId) -> Resp {
            let r = self.s.layout.placed_of(&self.s.items, id).unwrap().rect;
            self.click(r.cx(), r.cy() - self.s.scroll.offset)
        }
        fn key(&mut self, k: K) -> Resp {
            self.env(|s, e| s.key(KeyEv::plain(k), e))
        }
        fn keym(&mut self, k: K, shift: bool, ctrl: bool) -> Resp {
            self.env(|s, e| s.key(KeyEv { k, shift, ctrl, alt: false }, e))
        }
        fn type_text(&mut self, t: &str) -> Resp {
            self.env(|s, e| s.text(t, e))
        }
        fn events(&self) -> Vec<(ItemId, Event)> {
            self.log.borrow().clone()
        }
    }

    #[test]
    fn clicking_a_toggle_fires_toggled_with_the_new_value() {
        let mut t = setup(vec![toggle(1), toggle(2)], 0);
        t.click_item(2);
        assert_eq!(t.events(), [(2, Event::Toggled(true))]);
        t.click_item(2);
        assert_eq!(t.events()[1], (2, Event::Toggled(false)), "items() is asked again after an event");
    }

    #[test]
    fn a_press_released_elsewhere_does_not_activate() {
        let mut t = setup(vec![toggle(1), toggle(2)], 0);
        let a = t.s.layout.placed_of(&t.s.items, 1).unwrap().rect;
        let b = t.s.layout.placed_of(&t.s.items, 2).unwrap().rect;
        t.env(|s, e| {
            s.press(a.cx(), a.cy(), false, e);
            s.release(b.cx(), b.cy(), e);
        });
        assert!(t.events().is_empty());
        // Released outside the viewport.
        t.env(|s, e| {
            s.press(a.cx(), a.cy(), false, e);
            s.release(a.cx(), -20.0, e);
        });
        assert!(t.events().is_empty());
    }

    #[test]
    fn segmented_click_picks_the_segment_and_ignores_the_selected_one() {
        let seg = Item::Segmented { id: 1, label: "S".into(), options: vec!["a".into(), "b".into(), "c".into()], sel: 0 };
        let mut t = setup(vec![seg], 0);
        let ctl = t.s.layout.placed[0].ctl;
        let third = seg_rect(ctl, 3, 2);
        t.click(third.cx(), third.cy());
        assert_eq!(t.events(), [(1, Event::Picked(2))]);
        let first = seg_rect(ctl, 3, 0);
        t.click(first.cx(), first.cy());
        assert_eq!(t.events().len(), 1, "the selected segment does nothing");
    }

    #[test]
    fn keyboard_walks_the_interactive_items_and_activates() {
        let mut t = setup(vec![Item::Group("g".into()), toggle(1), Item::Note("n".into()), toggle(2)], 0);
        assert_eq!(t.s.focus, None);
        t.key(K::Down);
        assert_eq!(t.s.focus, Some(Foc::Item(1)));
        assert!(t.s.ring, "keyboard focus draws the ring");
        t.key(K::Tab);
        assert_eq!(t.s.focus, Some(Foc::Item(2)));
        t.key(K::Space);
        assert_eq!(t.events(), [(2, Event::Toggled(true))]);
        t.key(K::Tab);
        assert_eq!(t.s.focus, Some(Foc::Item(1)), "wraps");
        t.keym(K::Tab, true, false);
        assert_eq!(t.s.focus, Some(Foc::Item(2)), "Shift+Tab goes back");
        t.key(K::Home);
        assert_eq!(t.s.focus, Some(Foc::Item(1)));
        t.key(K::End);
        assert_eq!(t.s.focus, Some(Foc::Item(2)));
        t.key(K::Enter);
        assert_eq!(t.events().last().unwrap(), &(2, Event::Toggled(false)));
        // Esc and Backspace ask to go back when nothing is being edited.
        assert!(t.key(K::Esc).back);
        assert!(t.key(K::Backspace).back);
        assert!(t.env(|s, e| s.key(KeyEv { k: K::Left, shift: false, ctrl: false, alt: true }, e)).back);
    }

    #[test]
    fn clicking_does_not_draw_the_keyboard_ring() {
        let mut t = setup(vec![toggle(1)], 0);
        t.click_item(1);
        assert_eq!(t.s.focus, Some(Foc::Item(1)));
        assert!(!t.s.ring);
    }

    #[test]
    fn left_and_right_change_a_focused_segmented_control() {
        let seg = Item::Segmented { id: 1, label: "S".into(), options: vec!["a".into(), "b".into(), "c".into()], sel: 1 };
        let mut t = setup(vec![seg], 0);
        t.key(K::Down);
        t.key(K::Right);
        t.key(K::Left);
        t.key(K::Left);
        assert_eq!(t.events(), [(1, Event::Picked(2)), (1, Event::Picked(0)), (1, Event::Picked(0))]);
    }

    #[test]
    fn typing_in_a_field_sends_edited_and_blur_commits_once() {
        let mut t = setup(vec![field(1, "ab", FieldKind::Text), toggle(2)], 0);
        t.click_item(1);
        assert!(t.s.edit.is_some());
        t.type_text("c");
        t.type_text("d");
        assert_eq!(t.events(), [(1, Event::Edited("abc".into())), (1, Event::Edited("abcd".into()))]);
        // Clicking another item blurs: exactly one Committed with the final text.
        t.click_item(2);
        let ev = t.events();
        assert_eq!(ev.iter().filter(|(_, e)| matches!(e, Event::Committed(_))).count(), 1);
        assert!(ev.contains(&(1, Event::Committed("abcd".into()))));
        assert!(t.s.edit.is_none());
        // The page now shows the committed value.
        assert!(matches!(&t.s.items[0], Item::Field { value, .. } if value == "abcd"));
    }

    #[test]
    fn enter_commits_without_leaving_the_field_and_not_twice() {
        let mut t = setup(vec![field(1, "x", FieldKind::Text)], 0);
        t.click_item(1);
        t.type_text("y");
        t.key(K::Enter);
        assert!(t.events().contains(&(1, Event::Committed("xy".into()))));
        assert!(t.s.edit.is_some(), "still focused");
        let n = t.events().len();
        t.key(K::Enter);
        assert_eq!(t.events().len(), n, "nothing changed: no second commit");
        // Blur after the commit does not commit again.
        t.key(K::Tab);
        assert_eq!(t.events().len(), n);
    }

    #[test]
    fn page_leave_flush_commits_a_dirty_field() {
        let mut t = setup(vec![field(1, "x", FieldKind::Text)], 0);
        t.click_item(1);
        t.type_text("!");
        let mut cx = Cx::new(&mut t.cfg, 0);
        let fx = t.s.flush(&mut cx, &FixedMetrics);
        assert!(fx.is_empty());
        assert!(t.log.borrow().contains(&(1, Event::Committed("x!".into()))));
        // Clean field: flush sends nothing.
        let n = t.log.borrow().len();
        t.s.flush(&mut cx, &FixedMetrics);
        assert_eq!(t.log.borrow().len(), n);
    }

    #[test]
    fn esc_in_a_field_reverts_then_blurs_then_goes_back() {
        let mut t = setup(vec![field(1, "keep", FieldKind::Text)], 0);
        t.click_item(1);
        t.type_text("XYZ");
        let r = t.key(K::Esc);
        assert!(!r.back, "the first Esc reverts the edit");
        assert_eq!(t.s.edit.as_ref().unwrap().ed.text(), "keep");
        assert_eq!(t.events().last().unwrap(), &(1, Event::Edited("keep".into())), "the model is told");
        assert!(t.events().iter().all(|(_, e)| !matches!(e, Event::Committed(_))), "a reverted edit is never committed");
        let r = t.key(K::Esc);
        assert!(!r.back && t.s.edit.is_none(), "the second Esc leaves the field");
        assert_eq!(t.s.focus, Some(Foc::Item(1)));
        assert!(t.key(K::Esc).back, "the third goes back");
    }

    #[test]
    fn tab_into_a_field_selects_everything_and_click_does_not() {
        let mut t = setup(vec![toggle(1), field(2, "hello", FieldKind::Text)], 0);
        t.key(K::Tab);
        t.key(K::Tab);
        assert_eq!(t.s.edit.as_ref().unwrap().ed.selection(), Some((0, 5)));
        t.type_text("z");
        assert_eq!(t.s.edit.as_ref().unwrap().ed.text(), "z", "typing replaces the selection");
        let mut t = setup(vec![field(2, "hello", FieldKind::Text)], 0);
        t.click_item(2);
        assert_eq!(t.s.edit.as_ref().unwrap().ed.selection(), None);
    }

    #[test]
    fn click_in_a_field_places_the_caret_by_x() {
        let mut t = setup(vec![field(1, "abcdef", FieldKind::Text)], 0);
        let ctl = t.s.layout.placed[0].ctl;
        let (ox, _) = text_origin(ctl, false);
        let cw = crate::daemon::tray::page::edit::FIELD_PX * FixedMetrics::EM;
        t.click(ox + 2.2 * cw, ctl.cy());
        assert_eq!(t.s.edit.as_ref().unwrap().ed.caret, 2);
        // Dragging extends the selection.
        t.env(|s, e| {
            s.press(ox + 1.0 * cw, ctl.cy(), false, e);
            s.pointer(Some((ox + 4.0 * cw, ctl.cy())), true, e);
            s.release(ox + 4.0 * cw, ctl.cy(), e);
        });
        assert_eq!(t.s.edit.as_ref().unwrap().ed.selection(), Some((1, 4)));
        // Double-click selects the word.
        let mut t = setup(vec![field(1, "foo bar", FieldKind::Text)], 0);
        let ctl = t.s.layout.placed[0].ctl;
        t.env(|s, e| {
            s.press(ox + 5.0 * cw, ctl.cy(), true, e);
            s.release(ox + 5.0 * cw, ctl.cy(), e);
        });
        assert_eq!(t.s.edit.as_ref().unwrap().ed.selected_text(), "bar");
    }

    #[test]
    fn clipboard_shortcuts_in_a_field() {
        let mut t = setup(vec![field(1, "hello", FieldKind::Text)], 0);
        t.click_item(1);
        t.keym(K::Letter('a'), false, true);
        t.keym(K::Letter('c'), false, true);
        assert_eq!(t.clip.0.as_deref(), Some("hello"));
        t.keym(K::Letter('x'), false, true);
        assert_eq!(t.s.edit.as_ref().unwrap().ed.text(), "");
        t.clip.0 = Some("pa\nste".into());
        t.keym(K::Letter('v'), false, true);
        assert_eq!(t.s.edit.as_ref().unwrap().ed.text(), "pa ste");
        // A secret field refuses copy.
        let mut t = setup(vec![field(1, "sk-123", FieldKind::Secret)], 0);
        t.click_item(1);
        t.keym(K::Letter('a'), false, true);
        t.keym(K::Letter('c'), false, true);
        assert_eq!(t.clip.0, None);
    }

    #[test]
    fn multiline_up_down_stay_in_the_text_until_the_edge() {
        let mut t = setup(vec![toggle(1), field(2, "one\ntwo\nthree", FieldKind::Multiline { rows: 4 }), toggle(3)], 0);
        t.click_item(2);
        // The click put the caret somewhere; go to the middle line.
        t.env(|s, e| {
            let fe = s.edit.as_mut().unwrap();
            fe.ed.move_to(5, false);
            let _ = e;
        });
        t.key(K::Up);
        assert_eq!(t.s.focus, Some(Foc::Item(2)), "Up from the middle line moves the caret");
        t.key(K::Up);
        assert_eq!(t.s.focus, Some(Foc::Item(1)), "Up on the first line leaves the field");
        t.key(K::Down);
        t.env(|s, _| {
            let fe = s.edit.as_mut().unwrap();
            let n = fe.ed.len();
            fe.ed.move_to(n, false);
        });
        t.key(K::Down);
        assert_eq!(t.s.focus, Some(Foc::Item(3)), "Down on the last line leaves it");
        // Enter inserts a newline in a text area and commits nothing.
        let mut t = setup(vec![field(1, "a", FieldKind::Multiline { rows: 3 })], 0);
        t.click_item(1);
        t.key(K::Enter);
        assert_eq!(t.s.edit.as_ref().unwrap().ed.text(), "a\n");
        assert!(t.events().iter().all(|(_, e)| !matches!(e, Event::Committed(_))));
    }

    #[test]
    fn hotkey_fields_do_not_edit_and_activate_on_click() {
        let mut t = setup(vec![field(1, "Control+Super", FieldKind::Hotkey)], 0);
        t.click_item(1);
        assert!(t.s.edit.is_none());
        assert_eq!(t.events(), [(1, Event::Activated)]);
        // While capturing, the keyboard belongs to the hook: keys are swallowed.
        t.s.capture = Some((1, String::new()));
        let r = t.key(K::Esc);
        assert!(r.handled && !r.back);
        let fx = t.env(|s, e| s.capture_update("Control".into(), e));
        assert!(fx.is_empty());
        assert_eq!(t.events().last().unwrap(), &(1, Event::CaptureUpdate("Control".into())));
        t.env(|s, e| s.capture_done("Control+Super".into(), e));
        assert_eq!(t.events().last().unwrap(), &(1, Event::CaptureDone("Control+Super".into())));
        assert!(t.s.capture.is_none());
    }

    #[test]
    fn enter_on_a_field_left_with_esc_edits_it_again() {
        let mut t = setup(vec![field(1, "abc", FieldKind::Text)], 0);
        t.click_item(1);
        assert!(t.s.edit.is_some());
        t.key(K::Esc);
        assert!(t.s.edit.is_none() && t.s.focus == Some(Foc::Item(1)), "a clean Esc leaves the field, focus stays");
        let r = t.key(K::Enter);
        assert!(r.handled && r.effects.is_empty());
        let fe = t.s.edit.as_ref().expect("editing again");
        assert_eq!(fe.ed.selection(), Some((0, 3)), "like arriving with Tab: everything selected");
    }

    #[test]
    fn dragging_reports_the_thumb_and_the_selection_drag() {
        let mut t = setup(vec![field(1, "abc def", FieldKind::Text)], 0);
        assert!(!t.s.dragging());
        let c = t.s.layout.placed_of(&t.s.items, 1).unwrap().ctl;
        t.env(|s, e| s.press(c.l + 30.0, c.cy(), false, e));
        assert!(t.s.dragging(), "a press in a text field starts a selection drag");
        t.env(|s, e| s.release(c.l + 30.0, c.cy(), e));
        assert!(!t.s.dragging());
    }

    #[test]
    fn a_cancelled_capture_ends_quietly_and_a_click_elsewhere_ends_it() {
        let mut t = setup(vec![field(1, "Control+Super", FieldKind::Hotkey), field(2, "x", FieldKind::Text)], 0);
        let before = t.events().len();
        t.s.capture = Some((1, "Control".into()));
        // Esc alone arrives as an empty accelerator: the capture ends, the model hears nothing.
        let fx = t.env(|s, e| s.capture_done(String::new(), e));
        assert!(fx.is_empty() && t.s.capture.is_none());
        assert_eq!(t.events().len(), before);
        // Clicking another item while capturing asks the pager to end the capture.
        t.s.capture = Some((1, String::new()));
        let p = t.s.layout.placed_of(&t.s.items, 2).unwrap().ctl;
        let r = t.env(|s, e| s.press(p.cx(), p.cy(), false, e));
        assert!(r.effects.contains(&Effect::EndHotkeyCapture));
        t.s.capture = Some((1, String::new()));
        let h = t.s.layout.placed_of(&t.s.items, 1).unwrap().ctl;
        let r = t.env(|s, e| s.press(h.cx(), h.cy(), false, e));
        assert!(!r.effects.contains(&Effect::EndHotkeyCapture), "pressing the captured box itself keeps it");
    }

    #[test]
    fn virtual_rows_activate_through_row_event_and_delete_with_the_key() {
        let mut t = setup(vec![], 100);
        let rect = t.s.layout.virt_rect(3).unwrap();
        t.click(rect.l + 40.0, rect.cy());
        assert_eq!(t.rows.borrow().as_slice(), [(3, Event::Activated)]);
        // The trailing delete button.
        t.click(rect.r - 17.0, rect.cy());
        assert_eq!(t.rows.borrow().last().unwrap(), &(3, Event::Deleted));
        // Keyboard: Down focuses row 0, Delete deletes it, End goes to the last row and scrolls to it.
        t.s.focus = None;
        t.key(K::Down);
        assert_eq!(t.s.focus, Some(Foc::Row(0)));
        t.key(K::Delete);
        assert_eq!(t.rows.borrow().last().unwrap(), &(0, Event::Deleted));
        t.key(K::End);
        assert_eq!(t.s.focus, Some(Foc::Row(99)));
        assert!(t.s.scroll.offset > 4000.0, "the focused row is scrolled into view ({})", t.s.scroll.offset);
        t.key(K::Enter);
        assert_eq!(t.rows.borrow().last().unwrap(), &(99, Event::Activated));
    }

    #[test]
    fn focus_scrolls_reveal_and_wheel_moves_hover() {
        let items: Vec<Item> = (0..30).map(toggle).collect();
        let mut t = setup(items, 0);
        for _ in 0..12 {
            t.key(K::Down);
        }
        let r = t.s.layout.rect_of(&t.s.items, t.s.focus.unwrap()).unwrap();
        assert!(r.b - t.s.scroll.offset <= VIEW_H && r.t - t.s.scroll.offset >= 0.0, "focus is visible");
        // The wheel scrolls and the hovered item follows the pointer.
        t.env(|s, e| s.pointer(Some((100.0, 20.0)), false, e));
        let before = t.s.hover;
        assert!(t.s.wheel(-120).redraw);
        assert_ne!(t.s.hover, before);
        assert!(!t.s.wheel(120 * 1000).redraw || t.s.scroll.offset == 0.0);
    }

    #[test]
    fn page_keys_scroll_and_ctrl_home_end_jump() {
        let items: Vec<Item> = (0..30).map(toggle).collect();
        let mut t = setup(items, 0);
        t.key(K::PgDn);
        assert!(t.s.scroll.offset > 200.0);
        t.keym(K::End, false, true);
        assert_eq!(t.s.scroll.offset, t.s.scroll.max());
        t.keym(K::Home, false, true);
        assert_eq!(t.s.scroll.offset, 0.0);
        t.key(K::PgDn);
        t.key(K::PgUp);
        assert_eq!(t.s.scroll.offset, 0.0);
    }

    #[test]
    fn scrollbar_drag_and_track_clicks() {
        let items: Vec<Item> = (0..30).map(toggle).collect();
        let mut t = setup(items, 0);
        let (top, len) = t.s.scroll.thumb().unwrap();
        let x = CARD_W_PAGE - 3.0;
        t.env(|s, e| {
            s.press(x, top + len * 0.5, false, e);
            s.pointer(Some((x, top + len * 0.5 + 40.0)), true, e);
        });
        assert!(t.s.scroll.offset > 0.0 && t.s.scroll.dragging());
        t.env(|s, e| s.release(x, 100.0, e));
        assert!(!t.s.scroll.dragging());
        assert!(t.events().is_empty(), "scrollbar input never reaches the model");
        // Hovering the track is flagged.
        t.env(|s, e| s.pointer(Some((x, 50.0)), false, e));
        assert!(t.s.bar_hover);
    }

    #[test]
    fn a_vanished_item_drops_focus_and_editor() {
        let mut t = setup(vec![field(1, "x", FieldKind::Text), toggle(2)], 0);
        t.click_item(1);
        let mut cfg = Config::default();
        // Rebuild with the field gone.
        let model = Rec { items: vec![toggle(2)], log: t.log.clone(), rows: 0, row_log: t.rows.clone(), reply: Vec::new() };
        t.s.model = Box::new(model);
        t.s.rebuild(&Cx::new(&mut cfg, 0), &FixedMetrics);
        assert_eq!(t.s.focus, None);
        assert!(t.s.edit.is_none());
    }

    #[test]
    fn the_caret_position_follows_the_text_for_the_ime() {
        let mut t = setup(vec![field(1, "abcd", FieldKind::Text)], 0);
        assert_eq!(t.s.caret_pos(&FixedMetrics), None, "no editor, no caret");
        t.click_item(1);
        let (x, y) = t.s.caret_pos(&FixedMetrics).unwrap();
        let ctl = t.s.layout.placed[0].ctl;
        let (ox, oy) = text_origin(ctl, false);
        let cw = crate::daemon::tray::page::edit::FIELD_PX * FixedMetrics::EM;
        assert!((x - (ox + 4.0 * cw)).abs() < 0.01 && (y - (oy + LINE_H)).abs() < 0.01, "{x} {y}");
        t.type_text("e");
        assert!(t.s.caret_pos(&FixedMetrics).unwrap().0 > x);
    }

    #[test]
    fn keys_map_from_virtual_key_codes() {
        assert_eq!(key_from_vk(0x26, false), Some(K::Up));
        assert_eq!(key_from_vk(0x1B, false), Some(K::Esc));
        assert_eq!(key_from_vk(0x41, true), Some(K::Letter('a')));
        assert_eq!(key_from_vk(0x41, false), None, "letters are text, not keys");
        assert_eq!(key_from_vk(0x56, true), Some(K::Letter('v')));
        assert_eq!(key_from_vk(0x7B, false), None);
    }

    #[test]
    fn buttons_press_and_disabled_buttons_do_not() {
        let items = vec![
            Item::Button { id: 1, label: "Run".into(), style: BtnStyle::Primary, enabled: true },
            Item::Button { id: 2, label: "Off".into(), style: BtnStyle::Outline, enabled: false },
        ];
        let mut t = setup(items, 0);
        let b = t.s.layout.placed[0].ctl;
        t.click(b.cx(), b.cy());
        let d = t.s.layout.placed[1].ctl;
        t.click(d.cx(), d.cy());
        assert_eq!(t.events(), [(1, Event::Pressed)]);
    }
}
