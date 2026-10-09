//! Fonts come from `C:\Windows\Fonts` (egui's bundled fonts are compiled out to keep the binary
//! small): Segoe UI Variable / Segoe UI for text, Segoe UI Semibold for emphasis and Segoe Fluent
//! Icons (or Segoe MDL2 Assets) for glyphs.
use eframe::egui::{self, FontData, FontDefinitions, FontFamily, FontId};
use std::sync::Arc;

pub const SEMIBOLD: &str = "semibold";
pub const ICONS: &str = "icons";

fn read_first(names: &[&str]) -> Option<Vec<u8>> {
    let dir = std::env::var_os("WINDIR").map(std::path::PathBuf::from).unwrap_or_else(|| r"C:\Windows".into()).join("Fonts");
    names.iter().find_map(|n| std::fs::read(dir.join(n)).ok())
}

/// Install the Windows fonts. Missing files degrade to egui's built-in fallback metrics
/// (egui requires at least one font, so with none found text would not render at all).
pub fn install(ctx: &egui::Context) {
    let mut defs = FontDefinitions::empty();
    let regular = read_first(&["segoeui.ttf", "SegUIVar.ttf"]);
    let semibold = read_first(&["seguisb.ttf", "segoeuib.ttf"]).or_else(|| regular.clone());
    let icons = read_first(&["SegoeIcons.ttf", "segmdl2.ttf"]);
    let symbols = read_first(&["seguisym.ttf"]);

    let mut prop: Vec<String> = Vec::new();
    if let Some(b) = regular {
        defs.font_data.insert("regular".into(), Arc::new(FontData::from_owned(b)));
        prop.push("regular".into());
    }
    if let Some(b) = symbols {
        defs.font_data.insert("symbols".into(), Arc::new(FontData::from_owned(b)));
        prop.push("symbols".into());
    }
    let mut semi: Vec<String> = Vec::new();
    if let Some(b) = semibold {
        defs.font_data.insert(SEMIBOLD.into(), Arc::new(FontData::from_owned(b)));
        semi.push(SEMIBOLD.into());
    }
    semi.extend(prop.iter().cloned());
    if let Some(b) = icons {
        defs.font_data.insert(ICONS.into(), Arc::new(FontData::from_owned(b)));
        defs.families.insert(FontFamily::Name(ICONS.into()), vec![ICONS.into()]);
    }
    if prop.is_empty() {
        // No system fonts at all (not expected on Windows): use whatever egui has registered.
        ctx.set_fonts(FontDefinitions::default());
        return;
    }
    defs.families.insert(FontFamily::Proportional, prop.clone());
    defs.families.insert(FontFamily::Monospace, prop);
    defs.families.insert(FontFamily::Name(SEMIBOLD.into()), semi);
    if !defs.families.contains_key(&FontFamily::Name(ICONS.into())) {
        defs.families.insert(FontFamily::Name(ICONS.into()), defs.families[&FontFamily::Proportional].clone());
    }
    ctx.set_fonts(defs);
}

pub fn regular(size: f32) -> FontId {
    FontId::new(size, FontFamily::Proportional)
}
pub fn semibold(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name(SEMIBOLD.into()))
}
pub fn icon(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name(ICONS.into()))
}
