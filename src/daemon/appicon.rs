//! Foreground application icon for the Spokenly-style bubble. TASK win-output implements:
//! exe path -> high-quality icon (IShellItemImageFactory / SHGetFileInfo / ExtractIconEx at
//! >= 48 px), UWP via package logo when needed, friendly name from FileDescription; per-exe cache.
use super::paste::Target;

#[derive(Clone, Debug, PartialEq)]
pub struct AppIcon {
    /// Friendly name ("Windows Explorer"), falls back to exe stem.
    pub name: String,
    pub exe_name: String,
    /// Straight (non-premultiplied) RGBA, `size` x `size`.
    pub rgba: Vec<u8>,
    pub size: u32,
}

/// Icon for the target's process at roughly `px` pixels (cached).
pub fn load(target: &Target, px: u32) -> Option<AppIcon> {
    let _ = (target, px);
    todo!("win-output")
}

/// Load an icon directly from an exe/ico path (used by `--render-pill --app-icon`).
pub fn load_path(path: &str, px: u32) -> Option<AppIcon> {
    let _ = (path, px);
    todo!("win-output")
}
