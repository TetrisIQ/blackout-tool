//! The "Classical" design system as egui values: warm light ground, gold accent, serif headings.
//! Token values are copied from the design project's `styles.css`.

use eframe::egui::{self, Color32, FontData, FontDefinitions, FontFamily, FontId, RichText, Stroke, Visuals};

pub const BG: Color32 = Color32::from_rgb(0xf3, 0xf2, 0xf2);
pub const SURFACE: Color32 = Color32::from_rgb(0xea, 0xe9, 0xe9);
pub const TEXT: Color32 = Color32::from_rgb(0x20, 0x1f, 0x1d);
pub const ACCENT: Color32 = Color32::from_rgb(0xb6, 0x82, 0x35);
/// Text colour at 16% over the ground, as `--color-divider`.
pub const DIVIDER: Color32 = Color32::from_rgb(0xd5, 0xd4, 0xd4);
pub const N100: Color32 = Color32::from_rgb(0xf8, 0xf4, 0xf4);
pub const N200: Color32 = Color32::from_rgb(0xea, 0xe7, 0xe7);
pub const N300: Color32 = Color32::from_rgb(0xd7, 0xd3, 0xd3);
pub const N500: Color32 = Color32::from_rgb(0x9b, 0x97, 0x97);
pub const N600: Color32 = Color32::from_rgb(0x7d, 0x79, 0x79);
pub const N700: Color32 = Color32::from_rgb(0x60, 0x5d, 0x5d);
pub const N800: Color32 = Color32::from_rgb(0x44, 0x41, 0x41);
pub const A100: Color32 = Color32::from_rgb(0xff, 0xf3, 0xe4);
pub const A700: Color32 = Color32::from_rgb(0x7d, 0x54, 0x11);
pub const A800: Color32 = Color32::from_rgb(0x5a, 0x3b, 0x0a);

pub fn heading_family() -> FontFamily {
    FontFamily::Name("heading".into())
}

pub fn heading(text: impl Into<String>, size: f32) -> RichText {
    RichText::new(text)
        .font(FontId::new(size, heading_family()))
        .color(TEXT)
}

pub fn small(text: impl Into<String>, color: Color32) -> RichText {
    RichText::new(text).size(12.0).color(color)
}

/// A font file from the usual system locations, by a part of its file name.
fn system_font(needles: &[&str]) -> Option<Vec<u8>> {
    let roots = [
        "/usr/share/fonts",
        "/usr/local/share/fonts",
        "~/.local/share/fonts",
        "~/.fonts",
    ];
    let home = std::env::var("HOME").unwrap_or_default();
    let mut stack: Vec<std::path::PathBuf> = roots.iter().map(|r| r.replace('~', &home).into()).collect();
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for e in rd.flatten() {
            let path = e.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            let name = e.file_name().to_string_lossy().to_ascii_lowercase();
            let ok_ext = name.ends_with(".ttf") || name.ends_with(".otf");
            if ok_ext && needles.iter().all(|n| name.contains(n)) {
                return std::fs::read(path).ok();
            }
        }
    }
    None
}

/// Lora for text and Cormorant Garamond for headings when installed; otherwise egui's defaults
/// (the design's web fonts are not bundled, so a missing font must never break the window).
fn fonts() -> FontDefinitions {
    let mut fonts = FontDefinitions::default();
    let body = system_font(&["lora"]);
    let head = system_font(&["cormorant", "semibold"])
        .or_else(|| system_font(&["cormorantgaramond"]))
        .or_else(|| system_font(&["cormorant"]));
    if let Some(b) = body {
        fonts
            .font_data
            .insert("lora".into(), FontData::from_owned(b).into());
        fonts
            .families
            .entry(FontFamily::Proportional)
            .or_default()
            .insert(0, "lora".into());
    }
    let heading_chain = match head {
        Some(h) => {
            fonts
                .font_data
                .insert("cormorant".into(), FontData::from_owned(h).into());
            vec!["cormorant".to_string()]
        }
        None => Vec::new(),
    };
    let mut chain = heading_chain;
    chain.extend(
        fonts
            .families
            .get(&FontFamily::Proportional)
            .cloned()
            .unwrap_or_default(),
    );
    fonts.families.insert(heading_family(), chain);
    fonts
}

pub fn install(ctx: &egui::Context) {
    ctx.set_fonts(fonts());
    let mut v = Visuals::light();
    v.panel_fill = BG;
    v.window_fill = BG;
    v.extreme_bg_color = BG;
    v.faint_bg_color = N100;
    v.override_text_color = Some(TEXT);
    v.hyperlink_color = A700;
    v.selection.bg_fill = Color32::from_rgba_unmultiplied(0xb6, 0x82, 0x35, 77);
    v.selection.stroke = Stroke::new(1.0, A800);
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, DIVIDER);
    v.widgets.inactive.bg_stroke = Stroke::new(1.0, DIVIDER);
    v.widgets.inactive.bg_fill = Color32::TRANSPARENT;
    v.widgets.inactive.weak_bg_fill = Color32::TRANSPARENT;
    v.widgets.hovered.bg_fill = N200;
    v.widgets.hovered.weak_bg_fill = N200;
    v.widgets.hovered.bg_stroke = Stroke::new(1.0, ACCENT);
    v.window_shadow = egui::Shadow::NONE;
    ctx.set_visuals(v);
    ctx.all_styles_mut(|s| {
        s.spacing.item_spacing = egui::vec2(8.0, 6.0);
        s.text_styles
            .insert(egui::TextStyle::Body, FontId::new(13.0, FontFamily::Proportional));
        s.text_styles.insert(
            egui::TextStyle::Button,
            FontId::new(13.0, FontFamily::Proportional),
        );
    });
}
