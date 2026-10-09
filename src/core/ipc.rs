//! Daemon <-> settings-process signalling via window messages to the daemon's hidden
//! message window (class `DAEMON_WINDOW_CLASS`). Config/history live in files; messages only notify.
//! TASK core-data implements `notify_daemon` (FindWindowW + PostMessageW) and `daemon_running`.

pub const DAEMON_WINDOW_CLASS: &str = "OpenTalkDaemon";
pub const SINGLE_INSTANCE_MUTEX: &str = "Local\\OpenTalk.Daemon";

/// WM_APP-based message ids handled by the daemon window.
pub const WM_APP: u32 = 0x8000;
pub const WM_OT_RELOAD: u32 = WM_APP + 1; // config.json changed
pub const WM_OT_SUSPEND_HOTKEYS: u32 = WM_APP + 2; // wparam: 1 suspend, 0 resume (hotkey capture)
pub const WM_OT_OPEN_SETTINGS: u32 = WM_APP + 3; // second instance launched
pub const WM_OT_TOGGLE: u32 = WM_APP + 4; // toggle dictation (bench/automation)
pub const WM_OT_CANCEL: u32 = WM_APP + 5;
pub const WM_OT_RETRY_LAST: u32 = WM_APP + 6;
pub const WM_OT_QUIT: u32 = WM_APP + 7;
/// First id the daemon may use for its own internal messages.
pub const WM_OT_INTERNAL_BASE: u32 = WM_APP + 0x100;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IpcMsg { Reload, SuspendHotkeys(bool), OpenSettings, Toggle, Cancel, RetryLast, Quit }

impl IpcMsg {
    pub fn encode(self) -> (u32, usize) {
        match self {
            IpcMsg::Reload => (WM_OT_RELOAD, 0),
            IpcMsg::SuspendHotkeys(b) => (WM_OT_SUSPEND_HOTKEYS, b as usize),
            IpcMsg::OpenSettings => (WM_OT_OPEN_SETTINGS, 0),
            IpcMsg::Toggle => (WM_OT_TOGGLE, 0),
            IpcMsg::Cancel => (WM_OT_CANCEL, 0),
            IpcMsg::RetryLast => (WM_OT_RETRY_LAST, 0),
            IpcMsg::Quit => (WM_OT_QUIT, 0),
        }
    }
}

/// Post `msg` to the running daemon. Returns false if no daemon window exists.
pub fn notify_daemon(msg: IpcMsg) -> bool { let _ = msg; todo!("core-data") }
pub fn daemon_running() -> bool { todo!("core-data") }
