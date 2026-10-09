//! Launch at login via HKCU\Software\Microsoft\Windows\CurrentVersion\Run\OpenTalk = "<exe>" --hidden.
//! `is_enabled` must also honour `...\Explorer\StartupApproved\Run\OpenTalk` (first byte 0x03 = disabled
//! by Task Manager/Settings), mirroring OpenWhispr's executableWillLaunchAtLogin lesson.
//! TASK core-data implements this.
pub fn is_enabled() -> bool { todo!("core-data") }
pub fn set_enabled(enabled: bool) -> std::io::Result<()> { let _ = enabled; todo!("core-data") }
