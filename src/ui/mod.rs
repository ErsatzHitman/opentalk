//! Control panel (egui/eframe, glow) — separate on-demand process. TASK ui implements:
//! shell (sidebar Home/History, Dictionary, Settings, Support), History page, Dictionary +
//! Snippets page, Settings modal (Preferences, Hotkeys, Speech-to-Text, Cleanup, Privacy & Data,
//! System) with OpenWhispr tokens from docs/SPEC.md. Fonts from C:\Windows\Fonts (Segoe UI
//! Variable / Segoe UI + Segoe Fluent Icons). After saving config, call
//! `core::ipc::notify_daemon(IpcMsg::Reload)`.

/// `page`: None | "history" | "dictionary" | "settings[:section]".
pub fn run(page: Option<&str>) {
    let _ = page;
    todo!("ui")
}
