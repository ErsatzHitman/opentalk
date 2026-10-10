//! The page stack of the popup: Home is the menu (not a page); every other [`Route`] is a
//! [`Session`] on the stack. The pager owns what is shared by all pages: the popup's working copy
//! of the config, the header (back button, title), the status strip (toast), the confirmation
//! dialog and the effect runner that turns a model's [`Effect`]s into stack changes and
//! [`Outward`] actions for the window. Pure: no Win32, unit-tested.

use std::collections::VecDeque;
use std::path::PathBuf;

use super::layout::wrap_lines;
use super::metrics::TextMetrics;
use super::session::{Clip, CursorKind, Env, K, KeyEv, Session};
use super::{CARD_W_PAGE, ConfirmId, Cx, Effect, PAGE_HEADER_H, PopupAction, Route, TestResult, Waker, model_for};
use crate::core::config::Config;
use crate::daemon::tray::popup::R;

/// Height of the status strip shown under the body while a toast is up.
pub const STRIP_H: f32 = 28.0;
/// The header's back button.
pub const BACK: R = R { l: 8.0, t: 8.0, r: 40.0, b: 40.0 };
/// A toast stays this long.
pub const TOAST_MS: u32 = 2600;

/// Something the window must do for the pager (everything with a side effect outside the model).
#[derive(Clone, Debug, PartialEq)]
pub enum Outward {
    CopyText(String),
    OpenPath(PathBuf),
    /// Queue for the daemon thread (mailbox + wake-up message).
    Post(PopupAction),
    /// Start / stop the in-daemon hotkey capture.
    BeginCapture,
    EndCapture,
    /// A toast appeared (arm the one-shot timer) or went away.
    ToastTimer(bool),
}

/// What an input did, for the window.
#[derive(Default)]
pub struct PResp {
    pub redraw: bool,
    pub out: Vec<Outward>,
    /// The last page was left: show Home again.
    pub home: bool,
    /// `cx.cfg` was saved: re-theme if the theme changed.
    pub saved: bool,
    /// The key was consumed.
    pub handled: bool,
    /// The card height changed (never for now; reserved for status-strip relayout).
    pub relayout: bool,
}

/// A pending confirmation.
#[derive(Clone, Debug, PartialEq)]
pub struct Confirm {
    pub title: String,
    pub body: String,
    pub action: String,
    pub destructive: bool,
    pub on_confirm: ConfirmId,
    /// The action button has the keyboard focus (Cancel has it by default: safe for destructive).
    pub ok_focus: bool,
    pub hover: Option<bool>,
    pub down: Option<bool>,
}

/// Geometry of the confirmation dialog (card coordinates).
#[derive(Clone, Debug, PartialEq)]
pub struct ConfirmGeo {
    pub dlg: R,
    pub title_cy: f32,
    pub body: Vec<String>,
    pub body_top: f32,
    pub cancel: R,
    pub ok: R,
}

pub const DLG_W: f32 = 332.0;
pub const DLG_BODY_LINE: f32 = 17.0;

pub fn confirm_geo(c: &Confirm, card_h: f32, m: &dyn TextMetrics) -> ConfirmGeo {
    let body = wrap_lines(&c.body, 12.5, 400, DLG_W - 36.0, &|s, px, w| m.width(s, px, w));
    let h = 18.0 + 20.0 + 6.0 + body.len() as f32 * DLG_BODY_LINE + 18.0 + 32.0 + 16.0;
    let l = (CARD_W_PAGE - DLG_W) * 0.5;
    let t = ((card_h - h) * 0.5 - 12.0).max(PAGE_HEADER_H);
    let dlg = R::xywh(l, t, DLG_W, h);
    let ok_w = (m.width(&c.action, 13.0, 600) + 28.0).max(84.0);
    let cancel_w = (m.width("Cancel", 13.0, 600) + 28.0).max(84.0);
    let by = dlg.b - 16.0 - 32.0;
    let ok = R::xywh(dlg.r - 16.0 - ok_w, by, ok_w, 32.0);
    let cancel = R::xywh(ok.l - 8.0 - cancel_w, by, cancel_w, 32.0);
    ConfirmGeo { dlg, title_cy: t + 18.0 + 10.0, body, body_top: t + 18.0 + 20.0 + 6.0, cancel, ok }
}

fn unix_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64)
}

pub struct Pager {
    /// The popup's working copy of the config (pages mutate it; `Saved` posts it to the daemon).
    pub cfg: Config,
    pub dark: bool,
    /// Card height (logical px), fixed for the popup's life.
    pub card_h: f32,
    pub stack: Vec<Session>,
    pub toast: Option<String>,
    pub confirm: Option<Confirm>,
    pub back_hover: bool,
    pub back_down: bool,
    waker: Waker,
    now: Option<i64>,
}

impl Pager {
    pub fn new(cfg: Config, dark: bool, card_h: f32, waker: Waker) -> Pager {
        Pager { cfg, dark, card_h, stack: Vec::new(), toast: None, confirm: None, back_hover: false, back_down: false, waker, now: None }
    }

    /// Fix the clock the models see (tests).
    pub fn set_now(&mut self, ms: i64) {
        self.now = Some(ms);
    }

    pub fn body_h(&self) -> f32 {
        self.card_h - PAGE_HEADER_H - if self.toast.is_some() { STRIP_H } else { 0.0 }
    }

    pub fn depth(&self) -> usize {
        self.stack.len()
    }

    pub fn top(&self) -> Option<&Session> {
        self.stack.last()
    }

    pub fn title(&self) -> String {
        self.top().map(|s| s.model.title()).unwrap_or_default()
    }

    pub fn route(&self) -> Option<Route> {
        self.top().map(|s| s.route)
    }

    pub fn cursor(&self) -> CursorKind {
        if self.confirm.is_some() || self.back_hover {
            return CursorKind::Hand;
        }
        self.top().map_or(CursorKind::Arrow, Session::cursor)
    }

    fn cx<'a>(cfg: &'a mut Config, dark: bool, waker: &Waker, now: Option<i64>) -> Cx<'a> {
        Cx { cfg, dark, now_ms: now.unwrap_or_else(unix_ms), waker: waker.clone() }
    }

    fn rebuild_top(&mut self, m: &dyn TextMetrics) {
        let bh = self.body_h();
        let cx = Self::cx(&mut self.cfg, self.dark, &self.waker, self.now);
        if let Some(s) = self.stack.last_mut() {
            s.scroll.view_h = bh;
            s.rebuild(&cx, m);
        }
    }

    // -----------------------------------------------------------------------------------------
    // Stack
    // -----------------------------------------------------------------------------------------

    /// Show `route` as the only page (back goes Home). `Home` itself clears the stack.
    pub fn open(&mut self, route: Route, m: &dyn TextMetrics, clip: &mut dyn Clip) -> PResp {
        let mut r = PResp::default();
        let mut fx = Vec::new();
        while !self.stack.is_empty() {
            fx.extend(self.drop_top(m, &mut r));
        }
        self.confirm = None;
        if route == Route::Home {
            r.home = true;
            return r;
        }
        fx.extend(self.push(route, m, &mut r));
        self.run(fx, m, clip, &mut r);
        r.redraw = true;
        r
    }

    /// Commit the top page, call `leave`, drop it. Returns the commit's side effects.
    fn drop_top(&mut self, m: &dyn TextMetrics, r: &mut PResp) -> Vec<Effect> {
        let mut fx = Vec::new();
        {
            let mut cx = Self::cx(&mut self.cfg, self.dark, &self.waker, self.now);
            if let Some(s) = self.stack.last_mut() {
                fx = s.flush(&mut cx, m);
                s.model.leave(&mut cx);
                if s.capture.take().is_some() {
                    r.out.push(Outward::EndCapture);
                }
            }
        }
        self.stack.pop();
        fx.into_iter().filter(side_effect).collect()
    }

    /// Push a page for `route` (nothing for Home / unknown). Returns the commit effects of the page it covers.
    fn push(&mut self, route: Route, m: &dyn TextMetrics, r: &mut PResp) -> Vec<Effect> {
        let Some(model) = model_for(&route) else { return Vec::new() };
        self.push_model(route, model, m, r)
    }

    pub fn push_model(&mut self, route: Route, model: Box<dyn super::PageModel>, m: &dyn TextMetrics, r: &mut PResp) -> Vec<Effect> {
        let mut fx = Vec::new();
        let bh = self.body_h();
        let mut cx = Self::cx(&mut self.cfg, self.dark, &self.waker, self.now);
        if let Some(s) = self.stack.last_mut() {
            fx = s.flush(&mut cx, m).into_iter().filter(side_effect).collect();
            s.hover = None;
            s.press = None;
            if s.capture.take().is_some() {
                r.out.push(Outward::EndCapture);
            }
        }
        let mut s = Session::new(route, model, bh);
        s.model.enter(&mut cx);
        s.rebuild(&cx, m);
        self.stack.push(s);
        fx
    }

    /// Go back one page; Home when it was the last.
    fn pop(&mut self, m: &dyn TextMetrics, r: &mut PResp) -> Vec<Effect> {
        if self.stack.is_empty() {
            r.home = true;
            return Vec::new();
        }
        let fx = self.drop_top(m, r);
        if self.stack.is_empty() {
            r.home = true;
        } else {
            let bh = self.body_h();
            let mut cx = Self::cx(&mut self.cfg, self.dark, &self.waker, self.now);
            if let Some(s) = self.stack.last_mut() {
                s.scroll.view_h = bh;
                s.model.enter(&mut cx);
                s.rebuild(&cx, m);
            }
        }
        fx
    }

    /// Back (header button, Esc, Backspace, Alt+Left).
    pub fn back(&mut self, m: &dyn TextMetrics, clip: &mut dyn Clip) -> PResp {
        let mut r = PResp { redraw: true, ..PResp::default() };
        let fx = self.pop(m, &mut r);
        self.run(fx, m, clip, &mut r);
        r
    }

    /// Popup closing: commit every dirty field, leave every page, end any capture.
    pub fn close(&mut self, m: &dyn TextMetrics, clip: &mut dyn Clip) -> PResp {
        let mut r = PResp::default();
        let mut fx = Vec::new();
        while !self.stack.is_empty() {
            fx.extend(self.drop_top(m, &mut r));
        }
        self.confirm = None;
        self.run(fx, m, clip, &mut r);
        r
    }

    // -----------------------------------------------------------------------------------------
    // Effects
    // -----------------------------------------------------------------------------------------

    fn set_toast(&mut self, t: Option<String>, m: &dyn TextMetrics) {
        self.toast = t;
        self.rebuild_top(m);
    }

    /// The toast timer fired.
    pub fn toast_expired(&mut self, m: &dyn TextMetrics) -> PResp {
        let had = self.toast.is_some();
        self.set_toast(None, m);
        PResp { redraw: had, out: vec![Outward::ToastTimer(false)], ..PResp::default() }
    }

    /// Run effects in order. Effects a page returns while running (a confirmation accepted, a
    /// focus request) are run too.
    pub fn run(&mut self, effects: Vec<Effect>, m: &dyn TextMetrics, _clip: &mut dyn Clip, r: &mut PResp) {
        let mut queue: VecDeque<Effect> = effects.into();
        let mut steps = 0;
        while let Some(e) = queue.pop_front() {
            steps += 1;
            if steps > 256 {
                break;
            }
            match e {
                Effect::Navigate(route) => {
                    if route == Route::Home {
                        let mut fx = Vec::new();
                        while !self.stack.is_empty() {
                            fx.extend(self.drop_top(m, r));
                        }
                        r.home = true;
                        for f in fx.into_iter().rev() {
                            queue.push_front(f);
                        }
                    } else {
                        let fx = self.push(route, m, r);
                        for f in fx.into_iter().rev() {
                            queue.push_front(f);
                        }
                    }
                    r.redraw = true;
                }
                Effect::Back => {
                    let fx = self.pop(m, r);
                    for f in fx.into_iter().rev() {
                        queue.push_front(f);
                    }
                    r.redraw = true;
                }
                Effect::Toast(t) => {
                    self.set_toast(Some(t), m);
                    r.out.push(Outward::ToastTimer(true));
                    r.redraw = true;
                }
                Effect::CopyText(t) => r.out.push(Outward::CopyText(t)),
                Effect::OpenPath(p) => r.out.push(Outward::OpenPath(p)),
                Effect::BeginHotkeyCapture(id) => {
                    if let Some(s) = self.stack.last_mut() {
                        s.capture = Some((id, String::new()));
                        r.out.push(Outward::BeginCapture);
                        r.redraw = true;
                    }
                }
                Effect::EndHotkeyCapture => {
                    if self.stack.last_mut().is_some_and(|s| s.capture.take().is_some()) {
                        r.out.push(Outward::EndCapture);
                        r.redraw = true;
                    }
                }
                Effect::RunTest(k) => r.out.push(Outward::Post(PopupAction::RunTest(k))),
                Effect::Confirm { title, body, action, destructive, on_confirm } => {
                    self.confirm = Some(Confirm { title, body, action, destructive, on_confirm, ok_focus: !destructive, hover: None, down: None });
                    r.redraw = true;
                }
                Effect::Saved => {
                    r.saved = true;
                    let post = Outward::Post(PopupAction::SetConfig(Box::new(self.cfg.clone())));
                    // Several saves in a row collapse into the newest copy.
                    if matches!(r.out.last(), Some(Outward::Post(PopupAction::SetConfig(_)))) {
                        r.out.pop();
                    }
                    r.out.push(post);
                }
                Effect::Do(a) => r.out.push(Outward::Post(a)),
                Effect::Focus(id) => {
                    let mut cx = Self::cx(&mut self.cfg, self.dark, &self.waker, self.now);
                    if let Some(s) = self.stack.last_mut() {
                        let fx = s.focus_item(id, &mut cx, m);
                        for f in fx.into_iter().rev() {
                            queue.push_front(f);
                        }
                        r.redraw = true;
                    }
                }
            }
        }
        self.rebuild_top(m);
        r.redraw = true;
    }

    // -----------------------------------------------------------------------------------------
    // Input (card-local logical px)
    // -----------------------------------------------------------------------------------------

    fn session_resp(&mut self, sr: super::session::Resp, m: &dyn TextMetrics, clip: &mut dyn Clip) -> PResp {
        let mut r = PResp { redraw: sr.redraw, handled: sr.handled, ..PResp::default() };
        if sr.back {
            let b = self.back(m, clip);
            r.redraw = true;
            r.home |= b.home;
            r.out.extend(b.out);
            r.saved |= b.saved;
        }
        if !sr.effects.is_empty() {
            self.run(sr.effects, m, clip, &mut r);
        }
        r
    }

    fn with_session<T>(&mut self, m: &dyn TextMetrics, clip: &mut dyn Clip, f: impl FnOnce(&mut Session, &mut Env) -> T) -> Option<T> {
        let mut cx = Self::cx(&mut self.cfg, self.dark, &self.waker, self.now);
        let s = self.stack.last_mut()?;
        let mut env = Env { cx: &mut cx, m, clip };
        Some(f(s, &mut env))
    }

    pub fn pointer(&mut self, p: Option<(f32, f32)>, buttons: bool, m: &dyn TextMetrics, clip: &mut dyn Clip) -> PResp {
        let card_h = self.card_h;
        if let Some(c) = self.confirm.as_mut() {
            let g = confirm_geo(c, card_h, m);
            let hover = p.and_then(|(x, y)| if g.ok.contains(x, y) { Some(true) } else if g.cancel.contains(x, y) { Some(false) } else { None });
            let changed = c.hover != hover;
            c.hover = hover;
            return PResp { redraw: changed, ..PResp::default() };
        }
        let in_header = p.is_some_and(|(_, y)| y < PAGE_HEADER_H);
        let back_hover = in_header && p.is_some_and(|(x, y)| BACK.contains(x, y));
        let mut redraw = back_hover != self.back_hover;
        self.back_hover = back_hover;
        let body = match p {
            Some((x, y)) if !in_header && (0.0..CARD_W_PAGE).contains(&x) && y < self.card_h => Some((x, y - PAGE_HEADER_H)),
            // Dragging the thumb or a selection past the edge of the body keeps following the pointer.
            Some((x, y)) if buttons && self.top().is_some_and(Session::dragging) => Some((x.clamp(0.0, CARD_W_PAGE), (y - PAGE_HEADER_H).clamp(0.0, (self.body_h() - 1.0).max(0.0)))),
            _ => None,
        };
        let sr = self.with_session(m, clip, |s, e| s.pointer(body, buttons, e));
        let Some(sr) = sr else { return PResp { redraw, ..PResp::default() } };
        redraw |= sr.redraw;
        let mut r = self.session_resp(sr, m, clip);
        r.redraw |= redraw;
        r
    }

    pub fn leave(&mut self, m: &dyn TextMetrics, clip: &mut dyn Clip) -> PResp {
        self.pointer(None, false, m, clip)
    }

    pub fn press(&mut self, p: (f32, f32), dbl: bool, m: &dyn TextMetrics, clip: &mut dyn Clip) -> PResp {
        let (x, y) = p;
        if let Some(c) = &mut self.confirm {
            let g = confirm_geo(c, self.card_h, m);
            c.down = if g.ok.contains(x, y) { Some(true) } else if g.cancel.contains(x, y) { Some(false) } else { None };
            return PResp { redraw: true, ..PResp::default() };
        }
        if y < PAGE_HEADER_H {
            self.back_down = BACK.contains(x, y);
            return PResp { redraw: true, ..PResp::default() };
        }
        let sr = self.with_session(m, clip, |s, e| s.press(x, y - PAGE_HEADER_H, dbl, e));
        match sr {
            Some(sr) => self.session_resp(sr, m, clip),
            None => PResp::default(),
        }
    }

    pub fn release(&mut self, p: (f32, f32), m: &dyn TextMetrics, clip: &mut dyn Clip) -> PResp {
        let (x, y) = p;
        if self.confirm.is_some() {
            let (g, down) = {
                let c = self.confirm.as_ref().expect("checked");
                (confirm_geo(c, self.card_h, m), c.down)
            };
            if let Some(c) = &mut self.confirm {
                c.down = None;
            }
            let over = if g.ok.contains(x, y) { Some(true) } else if g.cancel.contains(x, y) { Some(false) } else { None };
            return match (down, over) {
                (Some(d), Some(o)) if d == o => self.answer_confirm(o, m, clip),
                _ => PResp { redraw: true, ..PResp::default() },
            };
        }
        if self.back_down {
            self.back_down = false;
            if BACK.contains(x, y) {
                return self.back(m, clip);
            }
            return PResp { redraw: true, ..PResp::default() };
        }
        let sr = self.with_session(m, clip, |s, e| s.release(x, y - PAGE_HEADER_H, e));
        match sr {
            Some(sr) => self.session_resp(sr, m, clip),
            None => PResp::default(),
        }
    }

    pub fn wheel(&mut self, delta: i32) -> PResp {
        if self.confirm.is_some() {
            return PResp::default();
        }
        match self.stack.last_mut() {
            Some(s) => {
                let r = s.wheel(delta);
                PResp { redraw: r.redraw, ..PResp::default() }
            }
            None => PResp::default(),
        }
    }

    /// Accept (`true`) or cancel the pending confirmation.
    fn answer_confirm(&mut self, accept: bool, m: &dyn TextMetrics, clip: &mut dyn Clip) -> PResp {
        let Some(c) = self.confirm.take() else { return PResp::default() };
        let mut r = PResp { redraw: true, ..PResp::default() };
        if accept {
            let fx = {
                let mut cx = Self::cx(&mut self.cfg, self.dark, &self.waker, self.now);
                self.stack.last_mut().map(|s| s.model.confirmed(c.on_confirm, &mut cx)).unwrap_or_default()
            };
            self.run(fx, m, clip, &mut r);
        }
        r
    }

    pub fn key(&mut self, ev: KeyEv, m: &dyn TextMetrics, clip: &mut dyn Clip) -> PResp {
        if let Some(c) = &mut self.confirm {
            let handled = PResp { redraw: true, handled: true, ..PResp::default() };
            return match ev.k {
                K::Esc => self.answer_confirm(false, m, clip),
                K::Left | K::Right | K::Tab | K::Up | K::Down => {
                    c.ok_focus = !c.ok_focus;
                    handled
                }
                K::Enter | K::Space => {
                    let ok = c.ok_focus;
                    self.answer_confirm(ok, m, clip)
                }
                _ => handled,
            };
        }
        let sr = self.with_session(m, clip, |s, e| s.key(ev, e));
        match sr {
            Some(sr) => self.session_resp(sr, m, clip),
            None => PResp::default(),
        }
    }

    /// Printable text.
    pub fn text(&mut self, s: &str, m: &dyn TextMetrics, clip: &mut dyn Clip) -> PResp {
        if self.confirm.is_some() {
            return PResp::default();
        }
        let sr = self.with_session(m, clip, |sess, e| sess.text(s, e));
        match sr {
            Some(sr) => self.session_resp(sr, m, clip),
            None => PResp::default(),
        }
    }

    // -----------------------------------------------------------------------------------------
    // From the outside
    // -----------------------------------------------------------------------------------------

    /// A fresh config from the daemon (after it applied a change).
    pub fn set_cfg(&mut self, cfg: Config, m: &dyn TextMetrics) {
        self.cfg = cfg;
        self.rebuild_top(m);
    }

    pub fn wake(&mut self, m: &dyn TextMetrics) -> PResp {
        {
            let mut cx = Self::cx(&mut self.cfg, self.dark, &self.waker, self.now);
            if let Some(s) = self.stack.last_mut() {
                s.model.wake(&mut cx);
            }
        }
        self.rebuild_top(m);
        PResp { redraw: true, ..PResp::default() }
    }

    pub fn test_done(&mut self, result: TestResult, m: &dyn TextMetrics) -> PResp {
        {
            let mut cx = Self::cx(&mut self.cfg, self.dark, &self.waker, self.now);
            if let Some(s) = self.stack.last_mut() {
                s.model.test_done(result, &mut cx);
            }
        }
        self.rebuild_top(m);
        PResp { redraw: true, ..PResp::default() }
    }

    pub fn capture_update(&mut self, text: String, m: &dyn TextMetrics, clip: &mut dyn Clip) -> PResp {
        let fx = self.with_session(m, clip, |s, e| s.capture_update(text, e)).unwrap_or_default();
        let mut r = PResp { redraw: true, ..PResp::default() };
        self.run(fx, m, clip, &mut r);
        r
    }

    pub fn capture_done(&mut self, accel: String, m: &dyn TextMetrics, clip: &mut dyn Clip) -> PResp {
        let fx = self.with_session(m, clip, |s, e| s.capture_done(accel, e)).unwrap_or_default();
        let mut r = PResp { redraw: true, ..PResp::default() };
        self.run(fx, m, clip, &mut r);
        r
    }

    /// The caret of the edited field in card coordinates (x, bottom of its line).
    pub fn caret_card_pos(&self, m: &dyn TextMetrics) -> Option<(f32, f32)> {
        self.top()?.caret_pos(m).map(|(x, y)| (x, y + PAGE_HEADER_H))
    }

    /// Is a hotkey capture in progress on the top page?
    pub fn capturing(&self) -> bool {
        self.top().is_some_and(|s| s.capture.is_some())
    }
}

/// Effects worth keeping from a page that is being left or covered: its side effects, not its
/// navigation.
fn side_effect(e: &Effect) -> bool {
    matches!(e, Effect::Saved | Effect::Do(_) | Effect::Toast(_) | Effect::CopyText(_) | Effect::OpenPath(_) | Effect::RunTest(_))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::tray::page::metrics::FixedMetrics;
    use crate::daemon::tray::page::session::MemClip;
    use crate::daemon::tray::page::{Event, FieldKind, Item, ItemId, PickerId, Section, noop_waker};
    use std::cell::RefCell;
    use std::rc::Rc;

    type Seen = Rc<RefCell<Vec<String>>>;
    type OnEvent = dyn FnMut(ItemId, &Event, &mut Cx) -> Vec<Effect>;

    /// A page whose reaction to events is scripted per item id.
    struct Page {
        name: &'static str,
        items: Vec<Item>,
        on_event: Box<OnEvent>,
        seen: Seen,
    }

    impl super::super::PageModel for Page {
        fn title(&self) -> String {
            self.name.to_string()
        }
        fn items(&self, _cx: &Cx) -> Vec<Item> {
            self.items.clone()
        }
        fn event(&mut self, id: ItemId, ev: Event, cx: &mut Cx) -> Vec<Effect> {
            self.seen.borrow_mut().push(format!("{}:{id}:{ev:?}", self.name));
            (self.on_event)(id, &ev, cx)
        }
        fn enter(&mut self, _cx: &mut Cx) {
            self.seen.borrow_mut().push(format!("{}:enter", self.name));
        }
        fn leave(&mut self, _cx: &mut Cx) {
            self.seen.borrow_mut().push(format!("{}:leave", self.name));
        }
        fn confirmed(&mut self, id: ConfirmId, _cx: &mut Cx) -> Vec<Effect> {
            self.seen.borrow_mut().push(format!("{}:confirmed:{id:?}", self.name));
            vec![Effect::Toast("done".into())]
        }
    }

    fn tgl(id: ItemId) -> Item {
        Item::Toggle { id, label: "T".into(), desc: String::new(), on: false }
    }

    fn page(name: &'static str, items: Vec<Item>, seen: &Seen, f: impl FnMut(ItemId, &Event, &mut Cx) -> Vec<Effect> + 'static) -> Box<Page> {
        Box::new(Page { name, items, on_event: Box::new(f), seen: seen.clone() })
    }

    fn pager() -> Pager {
        Pager::new(Config::default(), false, 600.0, noop_waker())
    }

    fn push(p: &mut Pager, route: Route, model: Box<Page>) {
        let mut r = PResp::default();
        p.push_model(route, model, &FixedMetrics, &mut r);
    }

    fn ctx() -> MemClip {
        MemClip::default()
    }

    #[test]
    fn navigation_is_a_stack_that_ends_at_home() {
        let mut p = pager();
        let mut clip = ctx();
        let r = p.open(Route::SettingsIndex, &FixedMetrics, &mut clip);
        assert!(!r.home);
        assert_eq!(p.depth(), 1);
        // Index -> section -> picker, through effects (the real constructors give placeholders).
        let mut r = PResp::default();
        p.run(vec![Effect::Navigate(Route::Settings(Section::Preferences))], &FixedMetrics, &mut clip, &mut r);
        p.run(vec![Effect::Navigate(Route::Picker(PickerId::Language))], &FixedMetrics, &mut clip, &mut r);
        assert_eq!(p.depth(), 3);
        assert_eq!(p.route(), Some(Route::Picker(PickerId::Language)));
        assert!(!p.back(&FixedMetrics, &mut clip).home);
        assert_eq!(p.route(), Some(Route::Settings(Section::Preferences)));
        assert!(!p.back(&FixedMetrics, &mut clip).home);
        assert_eq!(p.route(), Some(Route::SettingsIndex));
        let r = p.back(&FixedMetrics, &mut clip);
        assert!(r.home, "the last page goes back to Home");
        assert_eq!(p.depth(), 0);
        // Back with nothing open stays Home.
        assert!(p.back(&FixedMetrics, &mut clip).home);
    }

    #[test]
    fn opening_home_or_another_route_resets_the_stack() {
        let mut p = pager();
        let mut clip = ctx();
        p.open(Route::History, &FixedMetrics, &mut clip);
        p.open(Route::Settings(Section::Hotkeys), &FixedMetrics, &mut clip);
        assert_eq!((p.depth(), p.route()), (1, Some(Route::Settings(Section::Hotkeys))));
        assert!(p.open(Route::Home, &FixedMetrics, &mut clip).home);
        assert_eq!(p.depth(), 0);
        // Navigate(Home) from a page also empties the stack.
        p.open(Route::Dictionary, &FixedMetrics, &mut clip);
        let mut r = PResp::default();
        p.run(vec![Effect::Navigate(Route::Snippets), Effect::Navigate(Route::Home)], &FixedMetrics, &mut clip, &mut r);
        assert!(r.home && p.depth() == 0);
    }

    #[test]
    fn enter_and_leave_bracket_a_pages_visibility() {
        let seen: Seen = Default::default();
        let mut p = pager();
        let mut clip = ctx();
        push(&mut p, Route::SettingsIndex, page("A", vec![tgl(1)], &seen, |_, _, _| vec![]));
        push(&mut p, Route::Settings(Section::About), page("B", vec![tgl(1)], &seen, |_, _, _| vec![]));
        p.back(&FixedMetrics, &mut clip);
        p.back(&FixedMetrics, &mut clip);
        assert_eq!(*seen.borrow(), ["A:enter", "B:enter", "B:leave", "A:enter", "A:leave"]);
    }

    #[test]
    fn closing_commits_dirty_fields_and_leaves_every_page() {
        let seen: Seen = Default::default();
        let mut p = pager();
        let mut clip = ctx();
        let field = Item::Field { id: 1, label: "F".into(), hint: String::new(), value: "a".into(), kind: FieldKind::Text, error: None, max_chars: 0 };
        push(&mut p, Route::SettingsIndex, page("A", vec![field], &seen, |_, ev, cx| {
            if let Event::Committed(t) = ev {
                cx.cfg.hotkey = t.clone();
                return vec![Effect::Saved];
            }
            vec![]
        }));
        // Type into the field.
        let (x, y) = {
            let s = p.top().unwrap();
            let r = s.layout.placed[0].ctl;
            (r.cx(), r.cy() + PAGE_HEADER_H)
        };
        p.press((x, y), false, &FixedMetrics, &mut clip);
        p.release((x, y), &FixedMetrics, &mut clip);
        p.text("b", &FixedMetrics, &mut clip);
        let r = p.close(&FixedMetrics, &mut clip);
        assert_eq!(p.cfg.hotkey, "ab", "the dirty field was committed before the popup closed");
        assert!(r.saved);
        assert!(matches!(r.out.last(), Some(Outward::Post(PopupAction::SetConfig(c))) if c.hotkey == "ab"));
        assert!(seen.borrow().contains(&"A:leave".to_string()));
        assert_eq!(p.depth(), 0);
    }

    #[test]
    fn leaving_a_page_commits_its_field_first() {
        let seen: Seen = Default::default();
        let mut p = pager();
        let mut clip = ctx();
        let field = Item::Field { id: 1, label: "F".into(), hint: String::new(), value: "".into(), kind: FieldKind::Text, error: None, max_chars: 0 };
        push(&mut p, Route::SettingsIndex, page("A", vec![field], &seen, |_, ev, cx| {
            if let Event::Committed(t) = ev {
                cx.cfg.hotkey = t.clone();
                return vec![Effect::Saved];
            }
            vec![]
        }));
        let r = p.top().unwrap().layout.placed[0].ctl;
        let pt = (r.cx(), r.cy() + PAGE_HEADER_H);
        p.press(pt, false, &FixedMetrics, &mut clip);
        p.release(pt, &FixedMetrics, &mut clip);
        p.text("xyz", &FixedMetrics, &mut clip);
        // Esc once: reverts the edit; the field is clean again, nothing is saved.
        let r = p.key(KeyEv::plain(K::Esc), &FixedMetrics, &mut clip);
        assert!(r.out.is_empty() && p.cfg.hotkey != "xyz");
        p.text("q", &FixedMetrics, &mut clip);
        // Header back button: the dirty "q" is committed on the way out, and the SetConfig survives.
        let r = p.back(&FixedMetrics, &mut clip);
        assert_eq!(p.cfg.hotkey, "q");
        assert!(r.home);
        assert!(r.out.iter().any(|o| matches!(o, Outward::Post(PopupAction::SetConfig(_)))));
    }

    #[test]
    fn esc_and_backspace_go_back_but_not_while_editing() {
        let seen: Seen = Default::default();
        let mut p = pager();
        let mut clip = ctx();
        let field = Item::Field { id: 1, label: "F".into(), hint: String::new(), value: "v".into(), kind: FieldKind::Text, error: None, max_chars: 0 };
        push(&mut p, Route::SettingsIndex, page("A", vec![tgl(2), field], &seen, |_, _, _| vec![]));
        push(&mut p, Route::Settings(Section::About), page("B", vec![field_item()], &seen, |_, _, _| vec![]));
        assert_eq!(p.depth(), 2);
        // Focus the field of B with the keyboard (Tab selects it for editing); Backspace deletes text, it does not go back.
        p.key(KeyEv::plain(K::Tab), &FixedMetrics, &mut clip);
        assert!(p.top().unwrap().edit.is_some());
        let r = p.key(KeyEv::plain(K::Backspace), &FixedMetrics, &mut clip);
        assert!(r.handled);
        assert_eq!(p.depth(), 2, "Backspace edits the field");
        // Esc: field has changes -> revert; then clean -> leave the field; then back.
        p.key(KeyEv::plain(K::Esc), &FixedMetrics, &mut clip);
        assert_eq!(p.depth(), 2);
        p.key(KeyEv::plain(K::Esc), &FixedMetrics, &mut clip);
        assert_eq!(p.depth(), 2);
        p.key(KeyEv::plain(K::Esc), &FixedMetrics, &mut clip);
        assert_eq!(p.depth(), 1, "now Esc goes back");
        let r = p.key(KeyEv::plain(K::Backspace), &FixedMetrics, &mut clip);
        assert!(r.home, "and Backspace from the first page returns Home");
    }

    fn field_item() -> Item {
        Item::Field { id: 7, label: "F".into(), hint: String::new(), value: "val".into(), kind: FieldKind::Text, error: None, max_chars: 0 }
    }

    #[test]
    fn saved_posts_the_working_config_and_collapses_repeats() {
        let seen: Seen = Default::default();
        let mut p = pager();
        let mut clip = ctx();
        push(&mut p, Route::SettingsIndex, page("A", vec![tgl(1)], &seen, |_, ev, cx| {
            if let Event::Toggled(v) = ev {
                cx.cfg.audio_cues = *v;
            }
            vec![Effect::Saved, Effect::Saved]
        }));
        let r = p.top().unwrap().layout.placed[0].rect;
        let pt = (r.cx() - 20.0, r.cy() + PAGE_HEADER_H);
        p.press(pt, false, &FixedMetrics, &mut clip);
        let resp = p.release(pt, &FixedMetrics, &mut clip);
        assert!(resp.saved);
        let posts: Vec<_> = resp.out.iter().filter(|o| matches!(o, Outward::Post(PopupAction::SetConfig(_)))).collect();
        assert_eq!(posts.len(), 1, "two Saved in a row = one post");
        assert!(matches!(posts[0], Outward::Post(PopupAction::SetConfig(c)) if c.audio_cues));
    }

    #[test]
    fn effects_run_in_order() {
        let mut p = pager();
        let mut clip = ctx();
        let mut r = PResp::default();
        p.run(
            vec![
                Effect::CopyText("a".into()),
                Effect::Do(PopupAction::HistoryClear),
                Effect::OpenPath("C:/x".into()),
                Effect::RunTest(super::super::TestKind::Stt),
                Effect::Toast("hi".into()),
            ],
            &FixedMetrics,
            &mut clip,
            &mut r,
        );
        assert_eq!(
            r.out,
            [
                Outward::CopyText("a".into()),
                Outward::Post(PopupAction::HistoryClear),
                Outward::OpenPath("C:/x".into()),
                Outward::Post(PopupAction::RunTest(super::super::TestKind::Stt)),
                Outward::ToastTimer(true),
            ]
        );
        assert_eq!(p.toast.as_deref(), Some("hi"));
    }

    #[test]
    fn a_toast_takes_strip_height_from_the_body_until_it_expires() {
        let seen: Seen = Default::default();
        let mut p = pager();
        let mut clip = ctx();
        push(&mut p, Route::SettingsIndex, page("A", vec![tgl(1)], &seen, |_, _, _| vec![]));
        assert_eq!(p.body_h(), 600.0 - PAGE_HEADER_H);
        assert_eq!(p.top().unwrap().scroll.view_h, 600.0 - PAGE_HEADER_H);
        let mut r = PResp::default();
        p.run(vec![Effect::Toast("Saved".into())], &FixedMetrics, &mut clip, &mut r);
        assert_eq!(p.body_h(), 600.0 - PAGE_HEADER_H - STRIP_H);
        assert_eq!(p.top().unwrap().scroll.view_h, p.body_h());
        let r = p.toast_expired(&FixedMetrics);
        assert!(r.redraw && r.out == [Outward::ToastTimer(false)]);
        assert_eq!(p.body_h(), 600.0 - PAGE_HEADER_H);
        assert!(!p.toast_expired(&FixedMetrics).redraw, "nothing to clear");
    }

    #[test]
    fn confirm_accept_calls_confirmed_and_runs_its_effects() {
        let seen: Seen = Default::default();
        let mut p = pager();
        let mut clip = ctx();
        push(&mut p, Route::History, page("H", vec![tgl(1)], &seen, |_, _, _| {
            vec![Effect::Confirm { title: "Clear?".into(), body: "All of it".into(), action: "Clear".into(), destructive: true, on_confirm: ConfirmId::HistoryClear }]
        }));
        let r = p.top().unwrap().layout.placed[0].rect;
        let pt = (r.cx() - 20.0, r.cy() + PAGE_HEADER_H);
        p.press(pt, false, &FixedMetrics, &mut clip);
        p.release(pt, &FixedMetrics, &mut clip);
        let c = p.confirm.clone().expect("dialog is up");
        assert!(c.destructive && !c.ok_focus, "a destructive dialog focuses Cancel");
        // Everything behind the dialog is inert: keys go to the dialog.
        let before = seen.borrow().len();
        p.key(KeyEv::plain(K::Space), &FixedMetrics, &mut clip);
        assert!(p.confirm.is_none(), "Space on Cancel cancels");
        assert_eq!(seen.borrow().len(), before, "no confirmed() call");
        // Again, then move to the action and accept with Enter.
        p.press(pt, false, &FixedMetrics, &mut clip);
        p.release(pt, &FixedMetrics, &mut clip);
        p.key(KeyEv::plain(K::Right), &FixedMetrics, &mut clip);
        assert!(p.confirm.as_ref().unwrap().ok_focus);
        let r = p.key(KeyEv::plain(K::Enter), &FixedMetrics, &mut clip);
        assert!(p.confirm.is_none());
        assert!(seen.borrow().contains(&"H:confirmed:HistoryClear".to_string()));
        assert!(r.out.contains(&Outward::ToastTimer(true)), "the effects of confirmed() ran");
    }

    #[test]
    fn confirm_buttons_and_esc_by_mouse_and_keyboard() {
        let seen: Seen = Default::default();
        let mut p = pager();
        let mut clip = ctx();
        push(&mut p, Route::History, page("H", vec![tgl(1)], &seen, |_, _, _| {
            vec![Effect::Confirm { title: "T".into(), body: "B ".repeat(40), action: "Delete".into(), destructive: true, on_confirm: ConfirmId::HistoryDelete(5) }]
        }));
        let open = |p: &mut Pager, clip: &mut MemClip| {
            let r = p.top().unwrap().layout.placed[0].rect;
            let pt = (r.cx() - 20.0, r.cy() + PAGE_HEADER_H);
            p.press(pt, false, &FixedMetrics, clip);
            p.release(pt, &FixedMetrics, clip);
        };
        open(&mut p, &mut clip);
        let g = confirm_geo(p.confirm.as_ref().unwrap(), p.card_h, &FixedMetrics);
        assert!(g.dlg.l >= 0.0 && g.dlg.r <= CARD_W_PAGE && g.dlg.t >= PAGE_HEADER_H && g.dlg.b <= p.card_h);
        assert!(g.ok.r <= g.dlg.r && g.cancel.r < g.ok.l && g.ok.b <= g.dlg.b);
        // Hover is tracked; pointer events never reach the page behind.
        p.pointer(Some((g.ok.cx(), g.ok.cy())), false, &FixedMetrics, &mut clip);
        assert_eq!(p.confirm.as_ref().unwrap().hover, Some(true));
        // Press on Cancel, release on Delete: nothing.
        p.press((g.cancel.cx(), g.cancel.cy()), false, &FixedMetrics, &mut clip);
        p.release((g.ok.cx(), g.ok.cy()), &FixedMetrics, &mut clip);
        assert!(p.confirm.is_some());
        // Click Delete: confirmed.
        p.press((g.ok.cx(), g.ok.cy()), false, &FixedMetrics, &mut clip);
        p.release((g.ok.cx(), g.ok.cy()), &FixedMetrics, &mut clip);
        assert!(p.confirm.is_none());
        assert!(seen.borrow().contains(&"H:confirmed:HistoryDelete(5)".to_string()));
        // Esc cancels without calling confirmed().
        seen.borrow_mut().clear();
        open(&mut p, &mut clip);
        p.key(KeyEv::plain(K::Esc), &FixedMetrics, &mut clip);
        assert!(p.confirm.is_none() && !seen.borrow().iter().any(|s| s.contains("confirmed")));
        assert_eq!(p.depth(), 1, "Esc closes the dialog, not the page");
    }

    #[test]
    fn header_back_button_pops_on_release_over_it() {
        let mut p = pager();
        let mut clip = ctx();
        p.open(Route::SettingsIndex, &FixedMetrics, &mut clip);
        let mut r = PResp::default();
        p.run(vec![Effect::Navigate(Route::Settings(Section::About))], &FixedMetrics, &mut clip, &mut r);
        assert_eq!(p.depth(), 2);
        let (bx, by) = (BACK.cx(), BACK.cy());
        p.pointer(Some((bx, by)), false, &FixedMetrics, &mut clip);
        assert!(p.back_hover);
        // Press on back, release elsewhere: no navigation.
        p.press((bx, by), false, &FixedMetrics, &mut clip);
        p.release((200.0, 200.0), &FixedMetrics, &mut clip);
        assert_eq!(p.depth(), 2);
        p.press((bx, by), false, &FixedMetrics, &mut clip);
        p.release((bx, by), &FixedMetrics, &mut clip);
        assert_eq!(p.depth(), 1);
        assert_eq!(p.cursor(), CursorKind::Hand);
        p.pointer(Some((200.0, 200.0)), false, &FixedMetrics, &mut clip);
        assert!(!p.back_hover);
        p.leave(&FixedMetrics, &mut clip);
    }

    #[test]
    fn hotkey_capture_begins_ends_and_is_ended_on_leave() {
        let seen: Seen = Default::default();
        let mut p = pager();
        let mut clip = ctx();
        let hk = Item::Field { id: 3, label: "Hotkey".into(), hint: String::new(), value: "Control+Super".into(), kind: FieldKind::Hotkey, error: None, max_chars: 0 };
        push(&mut p, Route::Settings(Section::Hotkeys), page("K", vec![hk], &seen, |id, ev, _| match ev {
            Event::Activated => vec![Effect::BeginHotkeyCapture(id)],
            Event::CaptureDone(_) => vec![Effect::Saved],
            _ => vec![],
        }));
        let r = p.top().unwrap().layout.placed[0].ctl;
        let pt = (r.cx(), r.cy() + PAGE_HEADER_H);
        p.press(pt, false, &FixedMetrics, &mut clip);
        let resp = p.release(pt, &FixedMetrics, &mut clip);
        assert!(resp.out.contains(&Outward::BeginCapture));
        assert!(p.capturing());
        let resp = p.capture_update("Control".into(), &FixedMetrics, &mut clip);
        assert!(resp.redraw);
        assert_eq!(p.top().unwrap().capture, Some((3, "Control".into())));
        let resp = p.capture_done("Control+Shift+K".into(), &FixedMetrics, &mut clip);
        assert!(resp.saved && !p.capturing());
        // Begin again and leave the page: the capture is ended.
        p.press(pt, false, &FixedMetrics, &mut clip);
        p.release(pt, &FixedMetrics, &mut clip);
        assert!(p.capturing());
        let resp = p.back(&FixedMetrics, &mut clip);
        assert!(resp.out.contains(&Outward::EndCapture), "leaving ends the capture");
        assert!(resp.home);
    }

    #[test]
    fn wake_and_test_results_reach_the_top_page() {
        let seen: Seen = Default::default();
        let mut p = pager();
        push(&mut p, Route::Settings(Section::SpeechToText), page("S", vec![tgl(1)], &seen, |_, _, _| vec![]));
        let r = p.wake(&FixedMetrics);
        assert!(r.redraw);
        p.test_done(TestResult { kind: super::super::TestKind::Stt, ok: true, badge: "OK".into(), text: "t".into() }, &FixedMetrics);
        p.set_cfg(Config::default(), &FixedMetrics);
    }

    #[test]
    fn a_navigate_cycle_guard_stops_runaway_effects() {
        // A model that navigates to itself forever would hang the UI thread; the runner is bounded.
        let mut p = pager();
        let mut clip = ctx();
        let mut r = PResp::default();
        let fx: Vec<Effect> = (0..1000).map(|_| Effect::Navigate(Route::SettingsIndex)).collect();
        p.run(fx, &FixedMetrics, &mut clip, &mut r);
        assert!(p.depth() <= 257);
    }

    #[test]
    fn page_card_height_adapts_to_small_work_areas() {
        use crate::daemon::tray::popup::page_card_h;
        assert_eq!(page_card_h(1040, 1.0), 600.0, "tall screens cap at 600");
        assert_eq!(page_card_h(700, 1.0), 676.0f32.min(600.0));
        assert_eq!(page_card_h(512, 1.0), 488.0, "work height minus two edge gaps");
        assert_eq!(page_card_h(768, 1.5), 488.0, "at 150% the 768p work area is 512 logical px");
        assert_eq!(page_card_h(100, 1.0), crate::daemon::tray::popup::PAGE_MIN_H, "never absurdly small");
    }
}
