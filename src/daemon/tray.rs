//! System tray icon (Shell_NotifyIconW) + popup menu. TASK win-output implements.
//! Left click -> TrayCmd::OpenSettings; right click -> menu.
//! Menu: Start/Stop listening, Show/Hide floating icon, Open OpenTalk, History, Dictionary,
//! Retry last, separator, Quit OpenTalk. Tooltip "OpenTalk - Voice Dictation".

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrayCmd {
    ToggleListening,
    ToggleFloatingIcon,
    OpenSettings,
    OpenHistory,
    OpenDictionary,
    RetryLast,
    Quit,
}

pub struct Tray {
    _private: (),
}

#[derive(Clone, Copy, Debug, Default)]
pub struct TrayState {
    pub listening: bool,
    pub floating_icon_visible: bool,
    pub can_retry: bool,
}

impl Tray {
    /// Add the icon. Shell notifications arrive at `hwnd` as message `callback_msg`.
    pub fn create(hwnd: isize, callback_msg: u32) -> Result<Tray, String> {
        let _ = (hwnd, callback_msg);
        todo!("win-output")
    }
    /// Handle a `callback_msg` (lparam from the window proc). Shows the menu on right click.
    pub fn handle(&mut self, hwnd: isize, lparam: isize, state: TrayState) -> Option<TrayCmd> {
        let _ = (hwnd, lparam, state);
        todo!("win-output")
    }
    /// Re-add after Explorer restarts (TaskbarCreated).
    pub fn readd(&mut self) {
        todo!("win-output")
    }
    pub fn remove(&mut self) {
        todo!("win-output")
    }
}
