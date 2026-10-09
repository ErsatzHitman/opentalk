//! OpenWhispr design tokens (docs/SPEC.md) as an egui palette, light + dark.
//!
//! Colours are plain sRGB; "alpha" variants (`fg_a`, `primary_a`, ...) mimic Tailwind's `/NN`
//! opacity modifiers, which the original CSS leans on heavily.
use eframe::egui::{self, Color32};

#[derive(Clone, Copy, Debug)]
pub struct Palette {
    pub dark: bool,
    pub primary: Color32,
    pub primary_fg: Color32,
    pub background: Color32,
    pub foreground: Color32,
    pub card: Color32,
    pub muted: Color32,
    pub muted_fg: Color32,
    pub border: Color32,
    pub border_hover: Color32,
    pub input: Color32,
    pub surface0: Color32,
    pub surface1: Color32,
    pub surface2: Color32,
    pub surface3: Color32,
    pub raised: Color32,
    pub window: Color32,
    pub border_subtle: Color32,
    pub destructive: Color32,
    pub success: Color32,
    pub warning: Color32,
}

const fn rgb(hex: u32) -> Color32 {
    Color32::from_rgb((hex >> 16) as u8, (hex >> 8) as u8, hex as u8)
}

impl Palette {
    pub const LIGHT: Palette = Palette {
        dark: false,
        primary: rgb(0x4577e9),
        primary_fg: rgb(0xffffff),
        background: rgb(0xffffff),
        foreground: rgb(0x171717),
        card: rgb(0xffffff),
        muted: rgb(0xf5f5f5),
        muted_fg: rgb(0x525252),
        border: rgb(0xe5e5e5),
        border_hover: rgb(0xd4d4d4),
        input: rgb(0xf9f6f1),
        surface0: rgb(0xffffff),
        surface1: rgb(0xfafafa),
        surface2: rgb(0xffffff),
        surface3: rgb(0xf5f5f5),
        raised: rgb(0xf0f0f0),
        window: rgb(0xf7f7f7),
        border_subtle: rgb(0xe5e5e5),
        destructive: rgb(0xdc2626),
        success: rgb(0x16a34a),
        warning: rgb(0xd97706),
    };

    pub const DARK: Palette = Palette {
        dark: true,
        primary: rgb(0x3e89ff),
        primary_fg: rgb(0xfcfcfc),
        background: rgb(0x191b1d),
        foreground: rgb(0xe8e8e8),
        card: rgb(0x24272a),
        muted: rgb(0x1e2022),
        muted_fg: rgb(0x8f8f8f),
        border: rgb(0x383b3e),
        border_hover: rgb(0x4a4d53),
        input: rgb(0x202225),
        surface0: rgb(0x191b1d),
        surface1: rgb(0x202225),
        surface2: rgb(0x24272a),
        surface3: rgb(0x2b2e32),
        raised: rgb(0x32363b),
        window: rgb(0x121416),
        border_subtle: rgb(0x343539),
        destructive: rgb(0xee343b),
        success: rgb(0x45ba50),
        warning: rgb(0xe0a030),
    };

    pub fn for_dark(dark: bool) -> Palette {
        if dark { Self::DARK } else { Self::LIGHT }
    }

    /// `foreground/NN` (alpha 0..=100 in percent, like Tailwind).
    pub fn fg_a(&self, pct: u8) -> Color32 {
        with_alpha(self.foreground, pct)
    }
    pub fn muted_a(&self, pct: u8) -> Color32 {
        with_alpha(self.muted_fg, pct)
    }
    pub fn primary_a(&self, pct: u8) -> Color32 {
        with_alpha(self.primary, pct)
    }
    pub fn destructive_a(&self, pct: u8) -> Color32 {
        with_alpha(self.destructive, pct)
    }
    pub fn border_a(&self, pct: u8) -> Color32 {
        with_alpha(self.border, pct)
    }
    pub fn success_a(&self, pct: u8) -> Color32 {
        with_alpha(self.success, pct)
    }
    pub fn warning_a(&self, pct: u8) -> Color32 {
        with_alpha(self.warning, pct)
    }
    /// White overlay in dark mode / black in light, for `hover:bg-black/5 dark:hover:bg-white/5`.
    pub fn overlay(&self, pct: u8) -> Color32 {
        if self.dark { with_alpha(Color32::WHITE, pct) } else { with_alpha(Color32::BLACK, pct) }
    }

    /// Card fill used by list panels: `bg-card/50` (light) / `bg-surface-2/60` (dark).
    pub fn panel_fill(&self) -> Color32 {
        if self.dark { with_alpha(self.surface2, 60) } else { with_alpha(self.card, 50) }
    }
    /// Panel border: `border-border/70` (light) / `border-white/10` (dark).
    pub fn panel_border(&self) -> Color32 {
        if self.dark { with_alpha(Color32::WHITE, 10) } else { with_alpha(self.border, 70) }
    }
    /// Row dividers: `divide-border/60`.
    pub fn divider(&self) -> Color32 {
        if self.dark { with_alpha(Color32::WHITE, 8) } else { with_alpha(self.border, 60) }
    }
    /// Border of inputs: `border-border/70` / `border-border-subtle/60`.
    pub fn input_border(&self) -> Color32 {
        if self.dark { with_alpha(self.border_subtle, 90) } else { with_alpha(self.border, 90) }
    }
    pub fn input_fill(&self) -> Color32 {
        if self.dark { self.surface1 } else { self.input }
    }
}

/// Alpha in percent (0..=100) applied to an opaque colour.
pub fn with_alpha(c: Color32, pct: u8) -> Color32 {
    let a = (pct.min(100) as f32 / 100.0 * 255.0).round() as u8;
    Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), a)
}

/// Linear blend of two colours (`t` = 0 gives `a`, 1 gives `b`).
pub fn mix(a: Color32, b: Color32, t: f32) -> Color32 {
    // Alpha-weighted so fading from/to a transparent colour keeps the other colour's hue
    // (a plain RGB lerp would pass through black).
    let (wa, wb) = (a.a() as f32 * (1.0 - t), b.a() as f32 * t);
    let alpha = wa + wb;
    if alpha <= 0.0 {
        return Color32::TRANSPARENT;
    }
    let c = |x: u8, y: u8| ((x as f32 * wa + y as f32 * wb) / alpha).round().clamp(0.0, 255.0) as u8;
    Color32::from_rgba_unmultiplied(c(a.r(), b.r()), c(a.g(), b.g()), c(a.b(), b.b()), alpha.round().clamp(0.0, 255.0) as u8)
}

/// Resolve the effective dark flag from the configured theme.
pub fn resolve_dark(theme: crate::core::config::Theme) -> bool {
    use crate::core::config::Theme;
    match theme {
        Theme::Light => false,
        Theme::Dark => true,
        Theme::Auto => !system_apps_use_light_theme(),
    }
}

/// `HKCU\...\Themes\Personalize\AppsUseLightTheme` (1 = light). Missing key => light.
pub fn system_apps_use_light_theme() -> bool {
    use windows::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RegGetValueW};
    use windows::core::w;
    let mut data: u32 = 1;
    let mut len = std::mem::size_of::<u32>() as u32;
    // SAFETY: `data`/`len` are valid for writes of a DWORD for the duration of the call.
    let r = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            w!("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize"),
            w!("AppsUseLightTheme"),
            RRF_RT_REG_DWORD,
            None,
            Some(&mut data as *mut u32 as *mut _),
            Some(&mut len),
        )
    };
    r.is_err() || data != 0
}

/// Configure egui's global style to the palette so stock widgets (TextEdit, ScrollArea,
/// popups) blend in. Custom widgets paint themselves from the `Palette` directly.
pub fn apply(ctx: &egui::Context, p: &Palette) {
    let mut style = (*ctx.global_style()).clone();
    let v = &mut style.visuals;
    *v = if p.dark { egui::Visuals::dark() } else { egui::Visuals::light() };
    v.panel_fill = p.background;
    v.window_fill = p.background;
    v.extreme_bg_color = p.input_fill();
    v.faint_bg_color = p.muted;
    v.override_text_color = Some(p.foreground);
    v.hyperlink_color = p.primary;
    v.selection.bg_fill = with_alpha(p.primary, 35);
    v.selection.stroke = egui::Stroke::new(1.0, p.primary);
    v.window_stroke = egui::Stroke::new(1.0, p.border);
    v.window_corner_radius = egui::CornerRadius::same(12);
    v.menu_corner_radius = egui::CornerRadius::same(8);
    v.popup_shadow = egui::Shadow {
        offset: [0, 8],
        blur: 24,
        spread: 0,
        color: Color32::from_black_alpha(if p.dark { 110 } else { 45 }),
    };
    v.window_shadow = v.popup_shadow;
    for w in [&mut v.widgets.noninteractive, &mut v.widgets.inactive, &mut v.widgets.hovered, &mut v.widgets.active, &mut v.widgets.open] {
        w.corner_radius = egui::CornerRadius::same(6);
    }
    v.widgets.noninteractive.bg_stroke = egui::Stroke::new(1.0, p.border);
    v.widgets.noninteractive.fg_stroke = egui::Stroke::new(1.0, p.foreground);
    v.widgets.inactive.bg_fill = p.input_fill();
    v.widgets.inactive.weak_bg_fill = Color32::TRANSPARENT;
    v.widgets.inactive.fg_stroke = egui::Stroke::new(1.0, p.foreground);
    v.widgets.hovered.bg_fill = p.surface3;
    v.widgets.hovered.weak_bg_fill = p.overlay(5);
    v.widgets.hovered.fg_stroke = egui::Stroke::new(1.0, p.foreground);
    v.widgets.active.bg_fill = p.raised;
    v.widgets.active.weak_bg_fill = p.overlay(8);
    v.widgets.active.fg_stroke = egui::Stroke::new(1.0, p.foreground);
    v.widgets.open.weak_bg_fill = p.overlay(5);
    style.spacing.item_spacing = egui::vec2(8.0, 8.0);
    style.spacing.button_padding = egui::vec2(10.0, 4.0);
    style.spacing.scroll.bar_width = 6.0;
    style.spacing.scroll.floating = true;
    style.spacing.scroll.floating_width = 4.0;
    style.spacing.scroll.floating_allocated_width = 0.0;
    style.spacing.scroll.foreground_color = false;
    style.animation_time = 0.12;
    ctx.set_global_style(style);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alpha_percent() {
        assert_eq!(with_alpha(Color32::WHITE, 0).a(), 0);
        assert_eq!(with_alpha(Color32::WHITE, 100).a(), 255);
        assert_eq!(with_alpha(Color32::WHITE, 50).a(), 128);
    }

    #[test]
    fn mix_endpoints() {
        let (a, b) = (Color32::from_rgb(0, 0, 0), Color32::from_rgb(200, 100, 50));
        assert_eq!(mix(a, b, 0.0), a);
        assert_eq!(mix(a, b, 1.0), b);
    }

    #[test]
    fn tokens_match_spec() {
        assert_eq!(Palette::LIGHT.primary, Color32::from_rgb(0x45, 0x77, 0xe9));
        assert_eq!(Palette::DARK.window, Color32::from_rgb(0x12, 0x14, 0x16));
        assert_eq!(Palette::DARK.destructive, Color32::from_rgb(0xee, 0x34, 0x3b));
    }
}
