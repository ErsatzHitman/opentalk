//! Design tokens (docs/SPEC.md "Pill") and colour helpers.

/// Straight (non-premultiplied) RGBA, components 0..=1.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rgba {
    pub r: f32,
    pub g: f32,
    pub b: f32,
    pub a: f32,
}

impl Rgba {
    pub const TRANSPARENT: Rgba = Rgba { r: 0.0, g: 0.0, b: 0.0, a: 0.0 };

    pub const fn new(r: f32, g: f32, b: f32, a: f32) -> Self {
        Rgba { r, g, b, a }
    }

    /// `0xRRGGBB` -> opaque colour.
    pub const fn hex(v: u32) -> Self {
        Rgba {
            r: ((v >> 16) & 0xff) as f32 / 255.0,
            g: ((v >> 8) & 0xff) as f32 / 255.0,
            b: (v & 0xff) as f32 / 255.0,
            a: 1.0,
        }
    }

    /// Same colour with alpha multiplied by `a` (Tailwind `/60` modifiers).
    pub fn with_alpha(self, a: f32) -> Self {
        Rgba { a: self.a * a, ..self }
    }

    /// Premultiplied components.
    #[inline]
    pub fn premul(self) -> [f32; 4] {
        [self.r * self.a, self.g * self.a, self.b * self.a, self.a]
    }

    pub fn from_premul(p: [f32; 4]) -> Self {
        if p[3] <= 1e-6 {
            return Rgba::TRANSPARENT;
        }
        Rgba { r: p[0] / p[3], g: p[1] / p[3], b: p[2] / p[3], a: p[3] }
    }

    /// CSS-style interpolation: premultiplied sRGB.
    pub fn lerp(self, to: Rgba, t: f32) -> Rgba {
        let a = self.premul();
        let b = to.premul();
        Rgba::from_premul([
            a[0] + (b[0] - a[0]) * t,
            a[1] + (b[1] - a[1]) * t,
            a[2] + (b[2] - a[2]) * t,
            a[3] + (b[3] - a[3]) * t,
        ])
    }

    pub fn to_color(self) -> tiny_skia::Color {
        tiny_skia::Color::from_rgba(
            self.r.clamp(0.0, 1.0),
            self.g.clamp(0.0, 1.0),
            self.b.clamp(0.0, 1.0),
            self.a.clamp(0.0, 1.0),
        )
        .unwrap_or(tiny_skia::Color::TRANSPARENT)
    }
}

/// Light/dark token set used by the pill, the tooltip and the error card.
#[derive(Clone, Copy, Debug)]
pub struct Palette {
    pub dark: bool,
    pub fg: Rgba,
    pub muted_fg: Rgba,
    pub border: Rgba,
    pub border_hover: Rgba,
    pub surface0: Rgba,
    pub surface1: Rgba,
    pub surface3: Rgba,
    pub popover: Rgba,
    /// Signal-glow layer opacities (SPEC: halo .8 / comet .95; light .7 / .85).
    pub halo_opacity: f32,
    pub comet_opacity: f32,
}

impl Palette {
    pub const LIGHT: Palette = Palette {
        dark: false,
        fg: Rgba::hex(0x171717),
        muted_fg: Rgba::hex(0x525252),
        border: Rgba::hex(0xe5e5e5),
        border_hover: Rgba::hex(0xd4d4d4),
        surface0: Rgba::hex(0xffffff),
        surface1: Rgba::hex(0xfafafa),
        surface3: Rgba::hex(0xf5f5f5),
        popover: Rgba::hex(0xffffff),
        halo_opacity: 0.7,
        comet_opacity: 0.85,
    };
    pub const DARK: Palette = Palette {
        dark: true,
        fg: Rgba::hex(0xe8e8e8),
        muted_fg: Rgba::hex(0x8f8f8f),
        border: Rgba::hex(0x383b3e),
        border_hover: Rgba::hex(0x4a4d53),
        surface0: Rgba::hex(0x191b1d),
        surface1: Rgba::hex(0x202225),
        surface3: Rgba::hex(0x2b2e32),
        popover: Rgba::hex(0x292b30),
        halo_opacity: 0.8,
        comet_opacity: 0.95,
    };

    pub fn get(dark: bool) -> Palette {
        if dark { Self::DARK } else { Self::LIGHT }
    }
}

/// Signal-glow colours (identical in both themes).
pub const SIGNAL_BRIGHT: Rgba = Rgba::hex(0x38bdf8);
pub const SIGNAL_CORE: Rgba = Rgba::hex(0x2563eb);
pub const SIGNAL_DEEP: Rgba = Rgba::hex(0x6366f1);
pub const SIGNAL_HEAD: Rgba = Rgba::hex(0xeaf6ff);

/// `true` when Windows apps use the dark theme (`AppsUseLightTheme == 0`).
pub fn system_prefers_dark() -> bool {
    use windows::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RegGetValueW};
    use windows::core::w;
    let mut data: u32 = 1;
    let mut len = std::mem::size_of::<u32>() as u32;
    // SAFETY: `data`/`len` outlive the call and describe a DWORD output buffer.
    let r = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            w!("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize"),
            w!("AppsUseLightTheme"),
            RRF_RT_REG_DWORD,
            None,
            Some(&mut data as *mut u32 as *mut core::ffi::c_void),
            Some(&mut len),
        )
    };
    r.is_ok() && data == 0
}

/// Resolve the configured theme to dark/light.
pub fn resolve_dark(theme: crate::core::config::Theme) -> bool {
    use crate::core::config::Theme;
    match theme {
        Theme::Dark => true,
        Theme::Light => false,
        Theme::Auto => system_prefers_dark(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_and_lerp() {
        let c = Rgba::hex(0xff8000);
        assert!((c.r - 1.0).abs() < 1e-6 && (c.g - 128.0 / 255.0).abs() < 1e-6 && c.b == 0.0);
        let m = Rgba::hex(0x000000).lerp(Rgba::hex(0xffffff), 0.5);
        assert!((m.r - 0.5).abs() < 1e-6 && m.a == 1.0);
        // premultiplied interpolation keeps the colour of the opaque end
        let t = Rgba::TRANSPARENT.lerp(Rgba::hex(0x2563eb), 0.5);
        assert!((t.a - 0.5).abs() < 1e-6 && (t.b - Rgba::hex(0x2563eb).b).abs() < 1e-5);
    }

    #[test]
    fn palettes_match_spec() {
        assert_eq!(Palette::LIGHT.surface1, Rgba::hex(0xfafafa));
        assert_eq!(Palette::DARK.border_hover, Rgba::hex(0x4a4d53));
        assert!(Palette::DARK.halo_opacity > Palette::LIGHT.halo_opacity);
    }
}
