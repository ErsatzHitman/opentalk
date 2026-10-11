//! Is there somewhere to paste? Before the paste keystroke, the focused element of the foreground
//! window is asked whether it takes text. When it clearly does not (the desktop, a button, a web
//! page with no field selected), the dictation is left on the clipboard instead and the pill says
//! so; it is never lost to a keystroke that lands nowhere.
//!
//! Signals, cheapest first: a system caret (classic edit controls, Office, Notepad), the terminal
//! list, then UI Automation's focused element (an editable value, an edit control or a text
//! document). Anything that cannot be decided reports `None` and the paste goes ahead as before:
//! only a confident "no" changes the behaviour.
use super::paste;
use windows::Win32::Foundation::HWND;
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize};
use windows::Win32::UI::Accessibility::{
    CUIAutomation, IUIAutomation, IUIAutomation2, IUIAutomationElement, IUIAutomationValuePattern, UIA_DocumentControlTypeId, UIA_EditControlTypeId,
    UIA_TextPatternId, UIA_ValuePatternId,
};
use windows::Win32::UI::WindowsAndMessaging::{GUITHREADINFO, GetForegroundWindow, GetGUIThreadInfo, GetWindowThreadProcessId};
use windows_core::Interface;

/// Shell surfaces that never take text: the desktop and the taskbars.
const SHELL_CLASSES: &[&str] = &["Progman", "WorkerW", "Shell_TrayWnd", "Shell_SecondaryTrayWnd"];

/// UI Automation gives up on an app that does not answer within this (it would otherwise wait
/// 20 s on a hung window); the paste then goes ahead.
const UIA_TIMEOUT_MS: u32 = 400;

/// Chromium-based apps build their accessibility tree when the first UI Automation client asks, so
/// a "no" is asked again once after this.
const RECHECK_MS: u64 = 80;

/// What UI Automation says about the focused element.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Facts {
    /// `UIA_EditControlTypeId`.
    pub edit: bool,
    /// `UIA_DocumentControlTypeId`.
    pub document: bool,
    /// `Some(read_only)` when the element has a value pattern.
    pub value_read_only: Option<bool>,
    pub text_pattern: bool,
}

/// Does an element with these facts take typed or pasted text?
pub fn takes_text(f: &Facts) -> bool {
    match f.value_read_only {
        Some(false) => true,
        // a read-only text box or page
        Some(true) => false,
        // edit controls without a value pattern (rich edits) and text documents (Word, editors)
        None => f.edit || (f.document && f.text_pattern),
    }
}

/// Whether the focused element of the foreground window takes text: `Some(false)` only when that is
/// clear, `None` when it cannot be told (the caller pastes as before). Blocks for a few ms, up to
/// [`UIA_TIMEOUT_MS`] for an app that does not answer; call it off the UI thread.
pub fn text_input_focused() -> Option<bool> {
    // SAFETY: plain window queries on whatever is in the foreground.
    let fg = unsafe { GetForegroundWindow() };
    if fg.0.is_null() {
        return Some(false);
    }
    let class = paste::class_of(fg);
    if SHELL_CLASSES.contains(&class.as_str()) {
        return Some(false);
    }
    if has_caret(fg) {
        return Some(true);
    }
    let exe = paste::exe_path_of(paste::pid_of(fg)).unwrap_or_default();
    if paste::is_terminal(&class, paste::file_name(&exe)) {
        return Some(true);
    }
    match focused_facts().map(|f| takes_text(&f)) {
        Some(false) => {
            std::thread::sleep(std::time::Duration::from_millis(RECHECK_MS));
            if has_caret(fg) {
                return Some(true);
            }
            focused_facts().map(|f| takes_text(&f))
        }
        other => other,
    }
}

/// The window's thread shows a caret (a blinking text cursor).
fn has_caret(hwnd: HWND) -> bool {
    // SAFETY: `info` is a properly sized GUITHREADINFO; the thread id comes from a live window.
    unsafe {
        let tid = GetWindowThreadProcessId(hwnd, None);
        let mut info = GUITHREADINFO { cbSize: std::mem::size_of::<GUITHREADINFO>() as u32, ..Default::default() };
        GetGUIThreadInfo(tid, &mut info).is_ok() && !info.hwndCaret.0.is_null()
    }
}

fn focused_facts() -> Option<Facts> {
    // SAFETY: COM is initialised for this call and balanced when it succeeded; every interface call is
    // on an object created here.
    unsafe {
        let init = CoInitializeEx(None, COINIT_MULTITHREADED).is_ok();
        let facts = (|| {
            let uia: IUIAutomation = CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER).ok()?;
            if let Ok(u2) = uia.cast::<IUIAutomation2>() {
                let _ = u2.SetConnectionTimeout(UIA_TIMEOUT_MS);
                let _ = u2.SetTransactionTimeout(UIA_TIMEOUT_MS);
            }
            let el = uia.GetFocusedElement().ok()?;
            Some(element_facts(&el))
        })();
        if init {
            CoUninitialize();
        }
        facts
    }
}

/// # Safety
/// `el` is a live UI Automation element on a COM-initialised thread.
unsafe fn element_facts(el: &IUIAutomationElement) -> Facts {
    unsafe {
        let ct = el.CurrentControlType().ok();
        let value_read_only = el.GetCurrentPatternAs::<IUIAutomationValuePattern>(UIA_ValuePatternId).ok().and_then(|v| v.CurrentIsReadOnly().ok()).map(|b| b.as_bool());
        let text_pattern = el.GetCurrentPattern(UIA_TextPatternId).is_ok_and(|p| !p.as_raw().is_null());
        Facts { edit: ct == Some(UIA_EditControlTypeId), document: ct == Some(UIA_DocumentControlTypeId), value_read_only, text_pattern }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn editable_values_edit_controls_and_text_documents_take_text() {
        assert!(takes_text(&Facts { value_read_only: Some(false), ..Facts::default() }), "any editable value");
        assert!(takes_text(&Facts { edit: true, ..Facts::default() }), "a rich edit without a value pattern");
        assert!(takes_text(&Facts { document: true, text_pattern: true, ..Facts::default() }), "a Word document");
    }

    #[test]
    fn read_only_pages_buttons_and_lists_do_not() {
        assert!(!takes_text(&Facts { document: true, text_pattern: true, value_read_only: Some(true), ..Facts::default() }), "a web page");
        assert!(!takes_text(&Facts { edit: true, value_read_only: Some(true), ..Facts::default() }), "a read-only text box");
        assert!(!takes_text(&Facts::default()), "a button or a list item");
        assert!(!takes_text(&Facts { document: true, ..Facts::default() }), "a document without text");
    }
}
