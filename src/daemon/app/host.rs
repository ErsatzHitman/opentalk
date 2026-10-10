//! The daemon's hidden top-level window (class `ipc::DAEMON_WINDOW_CLASS`), its message loop and
//! the [`App`] that owns every component. Everything here runs on the main thread; the hook,
//! capture, animation and worker threads reach it only through posted messages.
//!
//! Re-entrancy: the window procedure keeps the `App` in a [`Gate`]. A message that arrives while
//! it is borrowed (a nested loop inside a popup menu, a COM call that pumps) is parked in a
//! bounded queue and replayed, in arrival order, as soon as the outermost borrow ends (no
//! re-post, so a nested loop never spins on its own message). Long modal calls (the tray menu)
//! run with the `App` released.
use std::cell::RefCell;
use std::collections::VecDeque;
use std::ffi::c_void;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext};
use windows::Win32::UI::WindowsAndMessaging::{
    ChangeWindowMessageFilterEx, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW, KillTimer,
    MSG, MSGFLT_ALLOW, PostMessageW, PostQuitMessage, RegisterClassW, SetTimer, TranslateMessage, WINDOW_EX_STYLE,
    WM_DESTROY, WM_ENDSESSION, WM_SETTINGCHANGE, WM_TIMER, WNDCLASSW, WS_POPUP,
};
use windows::core::{HSTRING, PCWSTR};

use super::dictation::{ActiveJob, Failed, RecSession};
use super::machine::{Action, Input, Machine, Phase};
use super::micpolicy;
use super::settings::{self, SettingsProc};
use crate::core::config::{Config, SttProviderId};
use crate::core::ipc::{self, IpcMsg};
use crate::core::timing::now_ms;
use crate::core::{autostart, history, secrets};
use crate::daemon::hotkey::{self, HotkeyEvent};
use crate::daemon::pill::{Pill, PillEvent, PillMode, PillOptions};
use crate::daemon::tray::{Tray, TrayCmd, TrayState, WM_TRAY_POPUP_CMD, command_from_popup};
use crate::daemon::{media, sound};
use crate::{log_err, log_info, log_warn};

pub(super) const WM_HOTKEY_EVT: u32 = ipc::WM_OT_INTERNAL_BASE + 1;
pub(super) const WM_PILL_EVT: u32 = ipc::WM_OT_INTERNAL_BASE + 2;
pub(super) const WM_TRAY_EVT: u32 = ipc::WM_OT_INTERNAL_BASE + 3;
/// Helper threads posted notes into the dictation mailbox.
pub(super) const WM_NOTE: u32 = ipc::WM_OT_INTERNAL_BASE + 4;
/// The Settings child process exited (posted by its watcher thread).
pub(super) const WM_SETTINGS_EXITED: u32 = ipc::WM_OT_INTERNAL_BASE + 5;
/// Start-up: add the tray icon once the message loop runs.
const WM_CREATE_TRAY: u32 = ipc::WM_OT_INTERNAL_BASE + 6;

pub(super) const TIMER_ARM: usize = 1;
pub(super) const TIMER_CAP: usize = 2;
const TIMER_PRUNE: usize = 3;
const TIMER_SUSPEND: usize = 4;
const PRUNE_EVERY_MS: u32 = 24 * 60 * 60 * 1000;
/// Hotkeys suspended by a Settings hotkey capture come back by themselves after this long, in
/// case the Settings process dies without resuming them (they also come back when it exits).
const SUSPEND_LIMIT_MS: u32 = 2 * 60 * 1000;

// Window messages without a constant in the enabled `windows` features.
const WM_POWERBROADCAST: u32 = 0x0218;
const PBT_APMRESUMEAUTOMATIC: usize = 0x12;
const WM_WTSSESSION_CHANGE: u32 = 0x02B1;
const WTS_SESSION_LOCK: usize = 0x7;
const WTS_SESSION_UNLOCK: usize = 0x8;

/// The registered `TaskbarCreated` broadcast id (0 until the window exists).
static TASKBAR_CREATED: AtomicU32 = AtomicU32::new(0);

pub struct App {
    pub hwnd: isize,
    pub cfg: Config,
    pub machine: Machine,
    pub pill: Pill,
    pub tray: Option<Tray>,
    /// Live microphone level shared with the pill's waveform.
    pub level: Arc<AtomicU32>,
    pub rec: Option<RecSession>,
    pub job: Option<ActiveJob>,
    pub last_failed: Option<Failed>,
    pub settings: SettingsProc,
    /// "Hide for now" / tray "Hide floating icon": the idle pill stays hidden.
    pub icon_hidden: bool,
    pub seq: u64,
    /// When the hotkey press that is arming (hold mode) arrived.
    pub arm_t0: f64,
    /// The dictation being transcribed was stopped by the recording cap (notice after the paste).
    pub capped: bool,
    /// Hotkeys are suspended for a Settings hotkey capture.
    pub hotkeys_suspended: bool,
}

/// Most messages parked while the app is busy; beyond it the oldest is dropped.
const DEFER_CAP: usize = 256;

/// What a [`Deferrals::push`] did to the queue.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Overflow {
    /// Queued, nothing lost.
    No,
    /// Queue full: the oldest entry was dropped to make room (the first time).
    First,
    /// As [`Overflow::First`], on a later occasion.
    Again,
}

/// Bounded FIFO of parked messages plus the "a drain is running" flag.
struct Deferrals<T> {
    queue: VecDeque<T>,
    cap: usize,
    dropped: u64,
    draining: bool,
}

impl<T> Deferrals<T> {
    const fn new(cap: usize) -> Self {
        Deferrals { queue: VecDeque::new(), cap, dropped: 0, draining: false }
    }

    /// Park `item` at the back, dropping the oldest entry when full.
    fn push(&mut self, item: T) -> Overflow {
        let mut overflow = Overflow::No;
        if self.queue.len() >= self.cap {
            self.queue.pop_front();
            self.dropped += 1;
            overflow = if self.dropped == 1 { Overflow::First } else { Overflow::Again };
        }
        self.queue.push_back(item);
        overflow
    }

    /// Claim the drain: `false` when there is nothing to replay or a drain is already running
    /// (it will also pick up whatever is parked meanwhile).
    fn begin_drain(&mut self) -> bool {
        if self.draining || self.queue.is_empty() {
            return false;
        }
        self.draining = true;
        true
    }

    /// Next entry to replay, oldest first; `None` (and the drain ends) once the queue is empty.
    fn next(&mut self) -> Option<T> {
        let item = self.queue.pop_front();
        if item.is_none() {
            self.draining = false;
        }
        item
    }
}

/// A value that only one caller may use at a time, with a parking lot for the messages that find
/// it in use. The `RefCell` is the lock: a nested message loop (popup menu, COM) re-enters the
/// window procedure while the outer handler still holds the borrow.
struct Gate<S, M> {
    state: RefCell<Option<S>>,
    deferred: RefCell<Deferrals<M>>,
}

impl<S, M> Gate<S, M> {
    const fn new(cap: usize) -> Self {
        Gate { state: RefCell::new(None), deferred: RefCell::new(Deferrals::new(cap)) }
    }

    fn set(&self, state: S) {
        *self.state.borrow_mut() = Some(state);
    }

    fn take(&self) -> Option<S> {
        self.state.borrow_mut().take()
    }

    /// Run `f` on the state. `false` means it was busy (a re-entrant call) and `f` did not run;
    /// the caller then [`defer`](Self::defer)s its message. When the outermost call ends, every
    /// parked message is handed to `replay`, in order.
    fn run(&self, f: impl FnOnce(&mut S), replay: &mut dyn FnMut(M)) -> bool {
        let ran = match self.state.try_borrow_mut() {
            Ok(mut guard) => {
                if let Some(state) = guard.as_mut() {
                    if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(state))).is_err() {
                        log_err!("panic while handling a window message (recovered)");
                    }
                }
                true
            }
            Err(_) => false,
        };
        if ran {
            self.drain(replay);
        }
        ran
    }

    /// Park a message the busy state could not take.
    fn defer(&self, item: M) {
        let overflow = self.deferred.borrow_mut().push(item);
        if overflow == Overflow::First {
            log_warn!("window messages piled up while busy; dropping the oldest (capacity {DEFER_CAP})");
        }
    }

    /// Replay parked messages until none are left, including those parked while replaying. A
    /// replay that runs `f` again is not outermost for long: its own drain call is a no-op while
    /// this loop runs, so the recursion depth stays at one.
    fn drain(&self, replay: &mut dyn FnMut(M)) {
        if !self.deferred.borrow_mut().begin_drain() {
            return;
        }
        // Each entry is taken in its own statement: the queue is not borrowed while `replay` runs.
        loop {
            let next = self.deferred.borrow_mut().next();
            let Some(item) = next else { break };
            if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| replay(item))).is_err() {
                log_err!("panic while replaying a deferred window message (recovered)");
            }
        }
    }
}

/// A window message parked while the app was busy.
struct Deferred {
    hwnd: isize,
    msg: u32,
    wparam: usize,
    lparam: isize,
    /// When the window procedure first saw it.
    t0: f64,
}

thread_local! {
    static APP: Gate<App, Deferred> = const { Gate::new(DEFER_CAP) };
}

/// Run `f` on the app. `false` means the app was busy (re-entrant call) and `f` did not run.
/// Returning from the outermost call replays the messages parked meanwhile.
pub(super) fn with_app(f: impl FnOnce(&mut App)) -> bool {
    APP.with(|gate| gate.run(f, &mut replay))
}

/// Handle a parked message now that the app is free.
fn replay(d: Deferred) {
    if dispatch(hwnd_of(d.hwnd), d.msg, d.wparam, d.lparam, d.t0) == Some(false) {
        // Unreachable while only the outermost call drains; never park it again (it would spin).
        log_warn!("deferred window message {:#x} found the app busy; dropped", d.msg);
    }
}

pub(super) fn hwnd_of(h: isize) -> HWND {
    HWND(h as *mut c_void)
}

/// Post `msg` to the daemon window from any thread.
pub(super) fn post(hwnd: isize, msg: u32, wparam: usize) {
    // SAFETY: PostMessageW on a window handle; a destroyed window just makes the call fail.
    unsafe {
        let _ = PostMessageW(Some(hwnd_of(hwnd)), msg, WPARAM(wparam), LPARAM(0));
    }
}

fn pill_options(cfg: &Config) -> PillOptions {
    PillOptions {
        position: cfg.panel_position,
        auto_hide: cfg.floating_icon_auto_hide,
        show_app_bubble: cfg.show_app_bubble,
        theme: cfg.theme,
        hotkey_label: hotkey::label(&cfg.hotkey),
    }
}

/// True when no speech-to-text provider has credentials yet (first run).
pub(super) fn needs_setup(cfg: &Config) -> bool {
    let any_key = crate::stt::registry::PROVIDERS.iter().any(|p| p.id != SttProviderId::Custom && secrets::get(p.secret).is_some());
    !any_key && cfg.stt.custom_base_url.trim().is_empty()
}

impl App {
    fn new(hwnd: isize, cfg: Config) -> Result<App, String> {
        let level = Arc::new(AtomicU32::new(0f32.to_bits()));
        let pill = Pill::create(
            level.clone(),
            pill_options(&cfg),
            Box::new(move |ev| {
                // The pill calls this from its window procedure: only post, never re-enter.
                post(hwnd, WM_PILL_EVT, ev as usize);
            }),
        )?;
        let machine = Machine::new(cfg.activation_mode);
        Ok(App {
            hwnd,
            cfg,
            machine,
            pill,
            tray: None,
            level,
            rec: None,
            job: None,
            last_failed: None,
            settings: SettingsProc::new(hwnd, WM_SETTINGS_EXITED),
            icon_hidden: false,
            seq: 0,
            arm_t0: 0.0,
            capped: false,
            hotkeys_suspended: false,
        })
    }

    pub(super) fn win(&self) -> HWND {
        hwnd_of(self.hwnd)
    }

    pub(super) fn set_timer(&self, id: usize, ms: u32) {
        // SAFETY: timer on our own window; replaces an existing timer with the same id.
        unsafe {
            SetTimer(Some(self.win()), id, ms, None);
        }
    }

    pub(super) fn kill_timer(&self, id: usize) {
        // SAFETY: as above; killing an absent timer just fails.
        unsafe {
            let _ = KillTimer(Some(self.win()), id);
        }
    }

    /// Resting pill mode.
    pub(super) fn idle_mode(&self) -> PillMode {
        if self.icon_hidden { PillMode::Hidden } else { PillMode::Idle }
    }

    pub(super) fn set_idle(&mut self) {
        let mode = self.idle_mode();
        self.pill.set_mode(mode);
        self.pill.set_app(None);
    }

    // ---- inputs -------------------------------------------------------------------------

    fn on_hotkey(&mut self, wparam: usize, t0: f64) {
        let input = match HotkeyEvent::from_wparam(wparam) {
            Some(HotkeyEvent::Down) => Input::HotkeyDown,
            Some(HotkeyEvent::Up) => Input::HotkeyUp,
            Some(HotkeyEvent::Escape) => Input::Escape,
            None => return,
        };
        self.step(input, t0);
    }

    fn on_pill_event(&mut self, ev: PillEvent, t0: f64) {
        match ev {
            PillEvent::Clicked => self.step(Input::PillClick, t0),
            PillEvent::CancelClicked => self.step(Input::CancelClick, t0),
            PillEvent::ErrorRetry => self.retry_last(),
            PillEvent::ErrorDismissed => {}
            PillEvent::MenuToggleListening => self.step(Input::Toggle, t0),
            PillEvent::MenuHideForNow => self.set_icon_hidden(true),
            PillEvent::MenuOpenSettings => self.settings.open(settings::PAGE_HISTORY),
        }
    }

    fn on_tray_cmd(&mut self, cmd: TrayCmd, t0: f64) {
        match cmd {
            TrayCmd::ToggleListening => self.step(Input::Toggle, t0),
            TrayCmd::ToggleFloatingIcon => {
                let hide = !self.icon_hidden;
                self.set_icon_hidden(hide);
            }
            TrayCmd::OpenSettings => self.settings.open(settings::PAGE_SETTINGS),
            TrayCmd::OpenHistory => self.settings.open(settings::PAGE_HISTORY),
            TrayCmd::OpenDictionary => self.settings.open(settings::PAGE_DICTIONARY),
            TrayCmd::RetryLast => self.retry_last(),
            TrayCmd::Quit => self.quit(),
        }
    }

    fn set_icon_hidden(&mut self, hidden: bool) {
        self.icon_hidden = hidden;
        if self.machine.phase == Phase::Idle {
            let mode = self.idle_mode();
            self.pill.set_mode(mode);
        }
    }

    /// Feed `input` to the state machine and carry out the resulting action.
    pub(super) fn step(&mut self, input: Input, t0: f64) {
        let action = self.machine.step(input);
        match action {
            Action::None => {}
            Action::BeginArming => {
                self.arm_t0 = t0;
                self.set_timer(TIMER_ARM, super::machine::ARM_MS);
            }
            Action::CancelArming => self.kill_timer(TIMER_ARM),
            Action::StartRecording { hold } => {
                self.kill_timer(TIMER_ARM);
                let key_down = if hold { self.arm_t0 } else { t0 };
                self.start_recording(t0, key_down);
            }
            Action::StopRecording => self.stop_recording(),
            Action::CancelRecording => self.cancel_recording(),
            Action::CancelJob => self.cancel_job(),
        }
        hotkey::set_esc_capture(self.machine.is_busy());
    }

    fn on_timer(&mut self, id: usize) {
        match id {
            TIMER_ARM => {
                self.kill_timer(TIMER_ARM);
                let t0 = now_ms();
                self.step(Input::ArmTimer, t0);
            }
            TIMER_CAP => {
                self.kill_timer(TIMER_CAP);
                log_warn!("recording hit the {} min safety cap; stopping", super::machine::MAX_RECORDING_MS / 60_000);
                if let Some(rec) = self.rec.as_mut() {
                    rec.capped = true;
                }
                let t0 = now_ms();
                self.step(Input::CapTimer, t0);
            }
            TIMER_PRUNE => self.prune_history(),
            TIMER_SUSPEND => {
                log_warn!("hotkeys were suspended for {} s without a resume; resuming", SUSPEND_LIMIT_MS / 1000);
                self.suspend_hotkeys(false);
            }
            _ => {}
        }
    }

    fn prune_history(&self) {
        let days = self.cfg.history_retention_days;
        if days == 0 {
            return;
        }
        let spawned = std::thread::Builder::new().name("history-prune".into()).spawn(move || prune_history_now(days));
        if let Err(e) = spawned {
            log_warn!("history: cannot start the prune thread: {e}");
        }
    }

    // ---- IPC ----------------------------------------------------------------------------

    fn on_ipc(&mut self, msg: u32, wparam: usize, t0: f64) {
        match msg {
            ipc::WM_OT_RELOAD => self.reload(),
            ipc::WM_OT_SUSPEND_HOTKEYS => self.suspend_hotkeys(wparam != 0),
            ipc::WM_OT_OPEN_SETTINGS => self.settings.open(settings::PAGE_HISTORY),
            ipc::WM_OT_TOGGLE => self.step(Input::Toggle, t0),
            ipc::WM_OT_START => self.step(Input::Start, t0),
            ipc::WM_OT_STOP => self.step(Input::Stop, t0),
            ipc::WM_OT_CANCEL => self.step(Input::Cancel, t0),
            ipc::WM_OT_RETRY_LAST => self.retry_last(),
            ipc::WM_OT_QUIT => self.quit(),
            _ => {}
        }
    }

    /// Settings captures a new hotkey with its own hook: ours must not fire meanwhile.
    fn suspend_hotkeys(&mut self, on: bool) {
        hotkey::set_suspended(on);
        self.hotkeys_suspended = on;
        if on {
            self.set_timer(TIMER_SUSPEND, SUSPEND_LIMIT_MS);
        } else {
            self.kill_timer(TIMER_SUSPEND);
        }
    }

    /// The Settings process exited (its watcher thread posted this).
    fn on_settings_exited(&mut self) {
        let _ = self.settings.is_open(); // reaps the child
        if self.hotkeys_suspended {
            log_info!("settings closed during a hotkey capture; resuming hotkeys");
            self.suspend_hotkeys(false);
        }
    }

    /// Windows silently drops a low-level hook whose callback once took too long (typically
    /// around sleep or under heavy load), so the hook is reinstalled after resume and unlock.
    fn reinstall_hook(&mut self, why: &str) {
        hotkey::stop();
        match hotkey::start(self.hwnd, WM_HOTKEY_EVT, &self.cfg.hotkey) {
            Ok(()) => log_info!("hotkey hook reinstalled ({why})"),
            Err(e) => log_warn!("hotkey hook reinstall failed ({why}): {e}"),
        }
        hotkey::set_start_menu_mask(self.cfg.suppress_start_menu);
        hotkey::set_esc_capture(self.machine.is_busy());
    }

    /// `config.json` changed (Settings saved it): re-apply everything that can change live.
    fn reload(&mut self) {
        let new = Config::load();
        let old = std::mem::replace(&mut self.cfg, new);
        let cfg = &self.cfg;
        if cfg.hotkey != old.hotkey {
            match hotkey::set_hotkey(&cfg.hotkey) {
                Ok(()) => log_info!("hotkey changed to {}", cfg.hotkey),
                Err(e) => {
                    log_warn!("hotkey '{}' rejected ({e}); keeping '{}'", cfg.hotkey, old.hotkey);
                    self.cfg.hotkey = old.hotkey.clone();
                }
            }
        }
        hotkey::set_start_menu_mask(self.cfg.suppress_start_menu);
        self.machine.set_mode(self.cfg.activation_mode);
        let opts = pill_options(&self.cfg);
        self.pill.set_options(opts);
        if self.cfg.mic_device_id != old.mic_device_id || self.cfg.mic_warm_hold_secs != old.mic_warm_hold_secs {
            micpolicy::prepare_in_background(self.cfg.mic_device_id.clone(), self.cfg.mic_warm_hold_secs);
        }
        if self.cfg.pause_media && !old.pause_media {
            media::warm();
        }
        if self.cfg.audio_cues && !old.audio_cues {
            sound::init();
        }
        if !self.cfg.keep_failed_audio && self.last_failed.take().is_some() {
            log_info!("keep failed audio turned off: dropped the recording kept for Retry");
        }
        if self.cfg.history_retention_days != old.history_retention_days {
            self.prune_history();
        }
        log_info!("config reloaded");
    }

    fn quit(&mut self) {
        log_info!("quit requested");
        self.cancel_everything();
        hotkey::stop();
        if let Some(tray) = self.tray.as_mut() {
            tray.remove();
        }
        // SAFETY: ends this thread's message loop.
        unsafe { PostQuitMessage(0) };
    }

    // ---- tray ---------------------------------------------------------------------------

    pub(super) fn tray_state(&self) -> TrayState {
        TrayState {
            listening: self.machine.is_recording(),
            transcribing: self.machine.phase == Phase::Thinking,
            floating_icon_visible: !self.icon_hidden,
            can_retry: self.last_failed.is_some(),
            hotkey: hotkey::label(&self.cfg.hotkey),
            dark: crate::daemon::pill::kit::resolve_dark(self.cfg.theme),
            // Only the tail of the history file is read; nothing when history is off.
            recent: if self.cfg.history_enabled { history::recent_ok(2).into_iter().map(|e| e.text).collect() } else { Vec::new() },
        }
    }
}

/// True when `OPENTALK_HOME` redirects the app's files (a test or benchmark instance).
fn isolated_home() -> bool {
    std::env::var_os("OPENTALK_HOME").is_some_and(|v| !v.is_empty())
}

/// Drop history entries older than `days` (0 = keep forever). Blocking; call off the UI thread.
fn prune_history_now(days: u32) {
    if days == 0 {
        return;
    }
    match history::prune(days) {
        Ok(0) => {}
        Ok(n) => log_info!("history: pruned {n} entries older than {days} days"),
        Err(e) => log_warn!("history: prune failed: {e}"),
    }
}

/// Inverse of `ev as usize` for the events the pill posts.
fn event_from_code(code: usize) -> Option<PillEvent> {
    use PillEvent as E;
    [E::Clicked, E::CancelClicked, E::ErrorRetry, E::ErrorDismissed, E::MenuToggleListening, E::MenuHideForNow, E::MenuOpenSettings]
        .into_iter()
        .find(|e| *e as usize == code)
}

// ---- window procedure --------------------------------------------------------------------

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let t0 = now_ms();
    if msg == ipc::WM_OT_QUERY_PHASE {
        return LRESULT(query_phase() as isize);
    }
    match dispatch(hwnd, msg, wparam.0, lparam.0, t0) {
        Some(true) => LRESULT(0),
        // The app was busy (a nested loop up the stack): the outermost call replays it on return.
        Some(false) => {
            APP.with(|gate| gate.defer(Deferred { hwnd: hwnd.0 as isize, msg, wparam: wparam.0, lparam: lparam.0, t0 }));
            LRESULT(0)
        }
        // SAFETY: default handling of a message we do not own.
        None => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

/// Answer [`ipc::WM_OT_QUERY_PHASE`]: the dictation phase code, or 0 when the app is busy (a
/// nested loop up the stack). Never parked: the sender wants the answer now and retries.
fn query_phase() -> usize {
    let mut code = 0;
    with_app(|a| {
        code = match a.machine.phase {
            Phase::Idle => ipc::DaemonPhase::Idle,
            Phase::Arming => ipc::DaemonPhase::Arming,
            Phase::Recording { .. } => ipc::DaemonPhase::Recording,
            Phase::Thinking => ipc::DaemonPhase::Thinking,
        }
        .code();
    });
    code
}

/// Handle one message. `None`: not ours. `Some(false)`: the app was busy, so nothing ran.
/// `t0` is when the window procedure first saw the message (kept across a deferral).
fn dispatch(hwnd: HWND, msg: u32, wparam: usize, lparam: isize, t0: f64) -> Option<bool> {
    match msg {
        WM_HOTKEY_EVT => {
            // The hook thread stamped when it saw the key, so the time spent queued counts.
            let t_event = hotkey::event_time(lparam, t0);
            Some(with_app(|a| a.on_hotkey(wparam, t_event)))
        }
        WM_PILL_EVT => Some(with_app(|a| {
            if let Some(ev) = event_from_code(wparam) {
                a.on_pill_event(ev, t0);
            }
        })),
        WM_NOTE => Some(with_app(|a| a.on_notes())),
        WM_SETTINGS_EXITED => Some(with_app(|a| a.on_settings_exited())),
        WM_CREATE_TRAY => Some(with_app(|a| match Tray::create(a.hwnd, WM_TRAY_EVT) {
            Ok(t) => a.tray = Some(t),
            Err(e) => log_warn!("tray unavailable: {e}"),
        })),
        WM_TIMER => Some(with_app(|a| a.on_timer(wparam))),
        WM_POWERBROADCAST if wparam == PBT_APMRESUMEAUTOMATIC => Some(with_app(|a| a.reinstall_hook("resume"))),
        // A swallowed hotkey released on the secure desktop never sends its key-up: end the press.
        WM_WTSSESSION_CHANGE if wparam == WTS_SESSION_LOCK => Some(with_app(|_| hotkey::reset())),
        WM_WTSSESSION_CHANGE if wparam == WTS_SESSION_UNLOCK => Some(with_app(|a| a.reinstall_hook("unlock"))),
        // The light/dark theme changed: re-add the tray icon so its glyph follows the taskbar
        // (the pill window receives the broadcast itself).
        WM_SETTINGCHANGE if is_color_set_change(LPARAM(lparam)) => Some(with_app(|a| {
            if let Some(t) = a.tray.as_mut() {
                t.readd();
            }
        })),
        WM_TRAY_EVT => Some(on_tray_message(hwnd, lparam)),
        WM_TRAY_POPUP_CMD => Some(with_app(|a| {
            if let Some(cmd) = command_from_popup(wparam) {
                a.on_tray_cmd(cmd, t0);
            }
        })),
        m if (ipc::WM_APP + 1..=ipc::WM_OT_STOP).contains(&m) => Some(with_app(|a| a.on_ipc(m, wparam, t0))),
        m if m != 0 && m == TASKBAR_CREATED.load(Ordering::Relaxed) => Some(with_app(|a| {
            if let Some(t) = a.tray.as_mut() {
                t.readd();
            }
        })),
        WM_ENDSESSION if wparam != 0 => Some(with_app(|a| a.quit())),
        WM_DESTROY => {
            // SAFETY: normal teardown of the main window.
            unsafe { PostQuitMessage(0) };
            Some(true)
        }
        _ => None,
    }
}

/// `WM_SETTINGCHANGE` with `lParam` = "ImmersiveColorSet" (light/dark theme switch).
fn is_color_set_change(lparam: LPARAM) -> bool {
    if lparam.0 == 0 {
        return false;
    }
    // SAFETY: for WM_SETTINGCHANGE a non-null lParam points to a NUL-terminated UTF-16 string
    // that stays valid for the duration of the call; `wide_eq` stops at the first mismatch or NUL.
    unsafe { wide_eq(lparam.0 as *const u16, "ImmersiveColorSet") }
}

/// Compare a NUL-terminated UTF-16 string with `s`.
///
/// # Safety
/// `p` must point to a readable NUL-terminated UTF-16 string.
unsafe fn wide_eq(p: *const u16, s: &str) -> bool {
    let mut i = 0;
    for w in s.encode_utf16() {
        // SAFETY: every unit read so far was non-NUL, so `p + i` is still inside the string.
        if unsafe { *p.add(i) } != w {
            return false;
        }
        i += 1;
    }
    // SAFETY: as above.
    unsafe { *p.add(i) == 0 }
}

/// Let lower-integrity processes (a non-elevated second instance or Settings window while the
/// daemon itself runs elevated) post our IPC messages, which UIPI would otherwise drop.
fn allow_ipc_from_lower_integrity(hwnd: HWND) {
    let taskbar = TASKBAR_CREATED.load(Ordering::Relaxed);
    let msgs = (ipc::WM_OT_RELOAD..=ipc::WM_OT_LAST).chain((taskbar != 0).then_some(taskbar));
    for m in msgs {
        // SAFETY: changes the message filter of our own window only.
        if let Err(e) = unsafe { ChangeWindowMessageFilterEx(hwnd, m, MSGFLT_ALLOW, None) } {
            log_warn!("cannot allow message {m:#x} from lower-integrity senders: {e}");
        }
    }
}

/// Session lock/unlock notifications (to reinstall the hook after an unlock).
fn register_session_notifications(hwnd: HWND) {
    use windows::Win32::System::RemoteDesktop::{NOTIFY_FOR_THIS_SESSION, WTSRegisterSessionNotification};
    // SAFETY: registers our own window; the registration ends when the window is destroyed.
    if let Err(e) = unsafe { WTSRegisterSessionNotification(hwnd, NOTIFY_FOR_THIS_SESSION) } {
        log_warn!("session notifications unavailable: {e}");
    }
}

/// Tray callback: an activation toggles the popup. The popup is its own window on this thread (no
/// modal loop), and the row the user picks comes back as [`WM_TRAY_POPUP_CMD`].
/// `false` when the app was busy: the caller defers the message instead of dropping the click.
fn on_tray_message(hwnd: HWND, lparam: isize) -> bool {
    let mut open = None;
    if !with_app(|a| {
        if let Some(act) = a.tray.as_ref().and_then(|t| t.activation(lparam)) {
            open = Some((act, a.tray_state()));
        }
    }) {
        return false;
    }
    if let Some((act, state)) = open {
        // The app is released: the popup only reads the state it was handed.
        crate::daemon::tray::activate(hwnd.0 as isize, act, &state);
    }
    true
}

fn register_and_create() -> Result<isize, String> {
    let class = HSTRING::from(ipc::DAEMON_WINDOW_CLASS);
    // SAFETY: registering a class and creating a hidden top-level window on this thread. A
    // top-level (not message-only) window is required to receive the `TaskbarCreated` broadcast.
    unsafe {
        let inst = GetModuleHandleW(PCWSTR::null()).map_err(|e| e.to_string())?;
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: inst.into(),
            lpszClassName: PCWSTR(class.as_ptr()),
            ..Default::default()
        };
        if RegisterClassW(&wc) == 0 {
            return Err("RegisterClassW failed".into());
        }
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            &class,
            &HSTRING::from("OpenTalk Daemon"),
            WS_POPUP,
            0,
            0,
            0,
            0,
            None,
            None,
            Some(inst.into()),
            None,
        )
        .map_err(|e| e.to_string())?;
        Ok(hwnd.0 as isize)
    }
}

/// Daemon entry point. Returns the process exit code.
pub fn run(hidden: bool) -> i32 {
    let Some(_instance) = ipc::acquire_single_instance() else {
        // Another daemon owns the tray: ask it to open the window (a login launch just exits).
        if !hidden && !ipc::notify_daemon(IpcMsg::OpenSettings) {
            log_warn!("another instance holds the lock but has no window yet");
        }
        return 0;
    };
    std::panic::set_hook(Box::new(|info| {
        log_err!("panic: {info}");
    }));
    // SAFETY: process-wide DPI mode; the manifest normally set it already, in which case this fails harmlessly.
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok();
    }
    let cfg = Config::load();
    log_info!("OpenTalk {} starting (hidden={hidden}, hotkey={}, mode={:?}, stt={:?})", env!("CARGO_PKG_VERSION"), cfg.hotkey, cfg.activation_mode, cfg.stt.provider);

    let hwnd = match register_and_create() {
        Ok(h) => h,
        Err(e) => {
            log_err!("cannot create the daemon window: {e}");
            return 1;
        }
    };
    TASKBAR_CREATED.store(crate::daemon::tray::taskbar_created_message(), Ordering::Relaxed);
    allow_ipc_from_lower_integrity(hwnd_of(hwnd));
    register_session_notifications(hwnd_of(hwnd));

    // Only an interactive launch opens Settings, so a login launch skips the key lookups.
    let first_run = !hidden && needs_setup(&cfg);
    let mut app = match App::new(hwnd, cfg) {
        Ok(a) => a,
        Err(e) => {
            log_err!("cannot create the pill: {e}");
            return 1;
        }
    };
    app.pill.set_mode(PillMode::Idle);
    match hotkey::start(hwnd, WM_HOTKEY_EVT, &app.cfg.hotkey) {
        Ok(()) => {}
        Err(e) => {
            log_warn!("hotkey '{}' unavailable ({e}); falling back to F8", app.cfg.hotkey);
            if let Err(e2) = hotkey::start(hwnd, WM_HOTKEY_EVT, "F8") {
                log_err!("fallback hotkey failed: {e2}");
                app.pill.show_error("Hotkey unavailable", &format!("{e}. Choose another hotkey in Settings."), false);
            } else {
                app.cfg.hotkey = "F8".into();
                let opts = pill_options(&app.cfg);
                app.pill.set_options(opts);
            }
        }
    }
    hotkey::set_start_menu_mask(app.cfg.suppress_start_menu);
    let mic = app.cfg.mic_device_id.clone();
    let warm_hold = app.cfg.mic_warm_hold_secs;
    let pause_media = app.cfg.pause_media;
    let audio_cues = app.cfg.audio_cues;
    let start_minimized = app.cfg.start_minimized;
    // Shell_NotifyIconW is a synchronous call into Explorer (occasionally ~200 ms): add the tray
    // icon from the message loop, after the hotkey is already live.
    post(hwnd, WM_CREATE_TRAY, 0);
    let retention = app.cfg.history_retention_days;
    APP.with(|gate| gate.set(app));

    // Warm-ups run on one short-lived thread so the message loop (and the hotkey) is live at once.
    // None of them opens the microphone: a prepared stream is initialised but not started, so the
    // mic indicator stays off.
    if pause_media {
        media::warm();
    }
    let warm = std::thread::Builder::new().name("startup-warm".into()).spawn(move || {
        micpolicy::prepare_now(mic.as_deref(), warm_hold);
        if audio_cues {
            sound::init();
        }
        // An isolated instance (tests, benchmarks: OPENTALK_HOME set) must never re-point the
        // user's real launch-at-login entry at its own executable.
        if !isolated_home() {
            if let Err(e) = autostart::sync() {
                log_warn!("autostart sync failed: {e}");
            }
        }
        prune_history_now(retention);
    });
    if let Err(e) = warm {
        log_warn!("cannot start the warm-up thread ({e}); dictations start cold");
    }
    // SAFETY: timer on our own window.
    unsafe {
        SetTimer(Some(hwnd_of(hwnd)), TIMER_PRUNE, PRUNE_EVERY_MS, None);
    }
    if !hidden {
        if first_run {
            log_info!("no speech-to-text key yet: opening Settings");
            with_app(|a| a.settings.open(settings::PAGE_STT));
        } else if !start_minimized {
            with_app(|a| a.settings.open(settings::PAGE_HISTORY));
        }
    }
    log_info!("ready in {:.0} ms", now_ms());

    let mut msg = MSG::default();
    // SAFETY: standard message loop; GetMessageW blocks, so an idle daemon uses no CPU.
    unsafe {
        while GetMessageW(&mut msg, None, 0, 0).0 > 0 {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    // Teardown: drop the app (pill window, tray) before the window goes away.
    drop(APP.with(|gate| gate.take()));
    // SAFETY: destroying our own window (it may already be gone).
    unsafe {
        let _ = DestroyWindow(hwnd_of(hwnd));
    }
    log_info!("exited");
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drained<T>(d: &mut Deferrals<T>) -> Vec<T> {
        assert!(d.begin_drain() || d.queue.is_empty());
        std::iter::from_fn(|| d.next()).collect()
    }

    #[test]
    fn deferrals_keep_arrival_order() {
        let mut d = Deferrals::new(8);
        for i in 0..5 {
            assert_eq!(d.push(i), Overflow::No);
        }
        assert_eq!(drained(&mut d), vec![0, 1, 2, 3, 4]);
        assert!(!d.begin_drain(), "empty: nothing to drain");
    }

    #[test]
    fn deferrals_drop_the_oldest_and_report_the_first_overflow_once() {
        let mut d = Deferrals::new(3);
        assert_eq!(d.push(1), Overflow::No);
        assert_eq!(d.push(2), Overflow::No);
        assert_eq!(d.push(3), Overflow::No);
        assert_eq!(d.push(4), Overflow::First);
        assert_eq!(d.push(5), Overflow::Again);
        assert_eq!(d.dropped, 2);
        assert_eq!(drained(&mut d), vec![3, 4, 5], "newest kept, oldest dropped");
    }

    #[test]
    fn a_running_drain_cannot_be_claimed_twice() {
        let mut d = Deferrals::new(4);
        d.push(1);
        assert!(d.begin_drain());
        d.push(2);
        assert!(!d.begin_drain(), "already draining");
        assert_eq!(d.next(), Some(1));
        assert_eq!(d.next(), Some(2), "parked meanwhile, same pass");
        assert_eq!(d.next(), None);
        d.push(3);
        assert!(d.begin_drain(), "the next burst gets a new drain");
    }

    /// The window procedure in miniature: each message runs on the gate or is parked; the
    /// handler of message 1 pumps messages 2 and 3 the way a nested modal loop would.
    struct Harness {
        gate: Gate<Vec<u32>, u32>,
        /// Order in which messages ran.
        ran: RefCell<Vec<u32>>,
        /// Messages a handler pumps while it holds the gate.
        nested: RefCell<Vec<(u32, Vec<u32>)>>,
        depth: std::cell::Cell<u32>,
        max_depth: std::cell::Cell<u32>,
    }

    impl Harness {
        fn new(cap: usize) -> Self {
            let h = Harness {
                gate: Gate::new(cap),
                ran: RefCell::default(),
                nested: RefCell::default(),
                depth: Default::default(),
                max_depth: Default::default(),
            };
            h.gate.set(Vec::new());
            h
        }

        fn deliver(&self, msg: u32) {
            self.depth.set(self.depth.get() + 1);
            self.max_depth.set(self.max_depth.get().max(self.depth.get()));
            let ran = self.gate.run(
                |log| {
                    log.push(msg);
                    self.ran.borrow_mut().push(msg);
                    let pumped = {
                        let mut n = self.nested.borrow_mut();
                        n.iter().position(|(m, _)| *m == msg).map(|i| n.remove(i).1).unwrap_or_default()
                    };
                    for p in pumped {
                        self.deliver(p); // arrives while the gate is borrowed
                    }
                },
                &mut |m| self.deliver(m),
            );
            if !ran {
                self.gate.defer(msg);
            }
            self.depth.set(self.depth.get() - 1);
        }
    }

    #[test]
    fn busy_messages_run_in_order_after_the_outermost_call() {
        let h = Harness::new(16);
        h.nested.borrow_mut().push((1, vec![2, 3]));
        h.deliver(1);
        assert_eq!(*h.ran.borrow(), vec![1, 2, 3], "parked while 1 ran, replayed in order after it");
        assert_eq!(h.gate.state.borrow().as_deref(), Some(&[1, 2, 3][..]));
        assert!(h.gate.deferred.borrow().queue.is_empty());
    }

    #[test]
    fn a_message_parked_during_the_drain_is_drained_in_the_same_pass() {
        let h = Harness::new(16);
        // 1 pumps 2 and 4; replaying 2 pumps 3, which must run before 4 would only if parked
        // earlier: 4 was parked first, so the order is 1, 2, 4, 3.
        h.nested.borrow_mut().push((1, vec![2, 4]));
        h.nested.borrow_mut().push((2, vec![3]));
        h.deliver(1);
        assert_eq!(*h.ran.borrow(), vec![1, 2, 4, 3]);
        assert!(h.gate.deferred.borrow().queue.is_empty());
        assert!(!h.gate.deferred.borrow().draining, "the drain ended");
    }

    #[test]
    fn replaying_does_not_recurse_without_bound() {
        let h = Harness::new(512);
        // A chain: every replayed message pumps the next one, 200 deep.
        for m in 1..200u32 {
            h.nested.borrow_mut().push((m, vec![m + 1]));
        }
        h.deliver(1);
        assert_eq!(h.ran.borrow().len(), 200);
        assert_eq!(*h.ran.borrow(), (1..=200).collect::<Vec<_>>());
        assert!(h.max_depth.get() <= 3, "replay is a loop, not recursion (depth {})", h.max_depth.get());
    }

    #[test]
    fn overflow_while_busy_keeps_the_newest() {
        let h = Harness::new(3);
        h.nested.borrow_mut().push((1, vec![10, 11, 12, 13, 14]));
        h.deliver(1);
        assert_eq!(*h.ran.borrow(), vec![1, 12, 13, 14]);
    }

    #[test]
    fn a_panicking_handler_does_not_wedge_the_gate() {
        let gate: Gate<u32, u32> = Gate::new(4);
        gate.set(0);
        gate.defer(7);
        let seen = RefCell::new(Vec::new());
        assert!(gate.run(|_| {}, &mut |m| {
            seen.borrow_mut().push(m);
            if m == 7 {
                gate.defer(8);
                panic!("boom");
            }
        }));
        assert_eq!(*seen.borrow(), vec![7, 8], "later messages still replay");
        assert!(gate.run(|s| *s += 1, &mut |_| {}), "gate usable afterwards");
        assert_eq!(gate.take(), Some(1));
    }

    #[test]
    fn run_without_state_still_reports_not_busy() {
        let gate: Gate<u32, u32> = Gate::new(4);
        let mut touched = false;
        assert!(gate.run(|_| touched = true, &mut |_| {}));
        assert!(!touched, "no state yet: the closure does not run, the message is dropped");
    }
}
