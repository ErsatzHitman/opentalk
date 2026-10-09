//! Text insertion + target capture. TASK win-output implements. Port of OpenWhispr
//! windows-fast-paste.c + clipboard.js Windows path, plus improvements (docs/PLAN.md §8):
//! full clipboard snapshot/restore (all HGLOBAL formats), clipboard-history exclusion formats,
//! wait (<= 1.5 s) for physically held modifiers instead of injecting key-ups, restore focus to the
//! captured HWND, Ctrl+Shift+V for terminals, UIPI (elevated target) detection, restore after 500 ms
//! only if GetClipboardSequenceNumber is unchanged.

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Target {
    pub hwnd: isize,
    pub pid: u32,
    /// Full exe path and file name (lowercase file name used for matching).
    pub exe_path: String,
    pub exe_name: String,
    pub class_name: String,
    pub is_terminal: bool,
    /// Target runs at higher integrity than us (paste will be blocked by UIPI).
    pub elevated: bool,
}

#[derive(Clone, Copy, Debug)]
pub struct PasteOptions {
    pub restore_clipboard: bool,
    pub smart_spacing: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PasteOutcome {
    Pasted,
    /// Text left on the clipboard; reason is user-facing.
    CopiedOnly { reason: String },
}

/// Foreground window at hotkey time (resolves UWP ApplicationFrameHost to the real app process).
pub fn capture_foreground() -> Option<Target> {
    todo!("win-output")
}

/// Paste `text` into `target` (or the current foreground window).
pub fn paste_text(text: &str, target: Option<&Target>, opts: PasteOptions) -> PasteOutcome {
    let _ = (text, target, opts);
    todo!("win-output")
}

/// Put text on the clipboard (excluded from clipboard history). Returns success.
pub fn copy_to_clipboard(text: &str) -> bool {
    let _ = text;
    todo!("win-output")
}

/// OpenWhispr smartSpacing: append one space unless text ends with whitespace or CJK.
pub fn apply_smart_spacing(text: &str) -> String {
    let _ = text;
    todo!("win-output")
}
