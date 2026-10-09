//! The daemon's hidden top-level window (class `ipc::DAEMON_WINDOW_CLASS`), its message loop and
//! the [`App`] that owns every component. Everything here runs on the main thread; the hook,
//! capture, animation and worker threads reach it only through posted messages.
//!
//! Re-entrancy: the window procedure keeps the `App` in a `RefCell`. A message that arrives while
//! it is borrowed (a nested loop inside a popup menu, a COM call that pumps) is re-posted instead
//! of handled, and long modal calls (the tray menu) run with the `App` released.
use std::cell::RefCell;
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
use crate::daemon::tray::{Tray, TrayCmd, TrayState};
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

thread_local! {
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
}

/// Run `f` on the app. `false` means the app was busy (re-entrant call) and `f` did not run.
pub(super) fn with_app(f: impl FnOnce(&mut App)) -> bool {
    APP.with(|cell| match cell.try_borrow_mut() {
        Ok(mut guard) => {
            if let Some(app) = guard.as_mut() {
                if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(app))).is_err() {
                    log_err!("panic while handling a window message (recovered)");
                }
            }
            true
        }
        Err(_) => false,
    })
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
            TrayCmd::OpenSettings => self.settings.open(settings::PAGE_HISTORY),
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
            floating_icon_visible: !self.icon_hidden,
            can_retry: self.last_failed.is_some(),
        }
    }
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
    let handled = match msg {
        WM_HOTKEY_EVT => Some(with_app(|a| a.on_hotkey(wparam.0, t0))),
        WM_PILL_EVT => Some(with_app(|a| {
            if let Some(ev) = event_from_code(wparam.0) {
                a.on_pill_event(ev, t0);
            }
        })),
        WM_NOTE => Some(with_app(|a| a.on_notes())),
        WM_SETTINGS_EXITED => Some(with_app(|a| a.on_settings_exited())),
        WM_CREATE_TRAY => Some(with_app(|a| match Tray::create(a.hwnd, WM_TRAY_EVT) {
            Ok(t) => a.tray = Some(t),
            Err(e) => log_warn!("tray unavailable: {e}"),
        })),
        WM_TIMER => Some(with_app(|a| a.on_timer(wparam.0))),
        WM_POWERBROADCAST if wparam.0 == PBT_APMRESUMEAUTOMATIC => Some(with_app(|a| a.reinstall_hook("resume"))),
        WM_WTSSESSION_CHANGE if wparam.0 == WTS_SESSION_UNLOCK => Some(with_app(|a| a.reinstall_hook("unlock"))),
        // The light/dark theme changed: re-add the tray icon so its glyph follows the taskbar
        // (the pill window receives the broadcast itself).
        WM_SETTINGCHANGE if is_color_set_change(lparam) => Some(with_app(|a| {
            if let Some(t) = a.tray.as_mut() {
                t.readd();
            }
        })),
        WM_TRAY_EVT => {
            on_tray_message(hwnd, lparam, t0);
            Some(true)
        }
        m if (ipc::WM_APP + 1..=ipc::WM_OT_QUIT).contains(&m) => Some(with_app(|a| a.on_ipc(m, wparam.0, t0))),
        m if m != 0 && m == TASKBAR_CREATED.load(Ordering::Relaxed) => Some(with_app(|a| {
            if let Some(t) = a.tray.as_mut() {
                t.readd();
            }
        })),
        WM_ENDSESSION if wparam.0 != 0 => Some(with_app(|a| a.quit())),
        WM_DESTROY => {
            // SAFETY: normal teardown of the main window.
            unsafe { PostQuitMessage(0) };
            Some(true)
        }
        _ => None,
    };
    match handled {
        Some(true) => LRESULT(0),
        // The app was busy (a nested loop up the stack): handle the message later.
        Some(false) => {
            // SAFETY: re-posting the same message to the same window.
            unsafe {
                let _ = PostMessageW(Some(hwnd), msg, wparam, lparam);
            }
            LRESULT(0)
        }
        // SAFETY: default handling of a message we do not own.
        None => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
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
    let msgs = (ipc::WM_OT_RELOAD..=ipc::WM_OT_QUIT).chain((taskbar != 0).then_some(taskbar));
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

/// Tray callback. The popup menu runs a modal loop, so the tray is taken out of the app for it.
fn on_tray_message(hwnd: HWND, lparam: LPARAM, t0: f64) {
    let mut taken = None;
    let mut state = TrayState::default();
    if !with_app(|a| {
        taken = a.tray.take();
        state = a.tray_state();
    }) {
        return;
    }
    let Some(mut tray) = taken else { return };
    let cmd = tray.handle(hwnd.0 as isize, lparam.0, state);
    with_app(|a| {
        a.tray = Some(tray);
        if let Some(cmd) = cmd {
            a.on_tray_cmd(cmd, t0);
        }
    });
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
    APP.with(|cell| *cell.borrow_mut() = Some(app));

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
        if let Err(e) = autostart::sync() {
            log_warn!("autostart sync failed: {e}");
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
    APP.with(|cell| cell.borrow_mut().take());
    // SAFETY: destroying our own window (it may already be gone).
    unsafe {
        let _ = DestroyWindow(hwnd_of(hwnd));
    }
    log_info!("exited");
    0
}
