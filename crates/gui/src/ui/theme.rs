//! Design tokens from `design/design_handoff_bluetooth_device_selector/README.md`.

use eframe::egui::{self, Color32};

pub const BG_WINDOW: Color32 = Color32::from_rgb(0x1c, 0x1d, 0x21);
pub const BG_TOOLBAR: Color32 = Color32::from_rgb(0x20, 0x21, 0x26);
pub const BG_CHIP: Color32 = Color32::from_rgb(0x2a, 0x2b, 0x30);
pub const BG_BADGE: Color32 = Color32::from_rgb(0x26, 0x32, 0x1f);
pub const BORDER: Color32 = Color32::from_rgb(0x35, 0x36, 0x3b);
pub const BORDER_DIM: Color32 = Color32::from_rgb(0x2c, 0x2d, 0x31);

pub const TEXT_PRIMARY: Color32 = Color32::from_rgb(0xe4, 0xe4, 0xe6);
pub const TEXT_SECONDARY: Color32 = Color32::from_rgb(0xc7, 0xc8, 0xcc);
pub const TEXT_DIM: Color32 = Color32::from_rgb(0x98, 0x99, 0x9e);
pub const TEXT_DISABLED: Color32 = Color32::from_rgb(0x6c, 0x6d, 0x72);
pub const TEXT_FAINT: Color32 = Color32::from_rgb(0x54, 0x55, 0x5a);

pub const ACCENT: Color32 = Color32::from_rgb(0xe8, 0x82, 0x3c);
pub const ACCENT_TEXT: Color32 = Color32::from_rgb(0x24, 0x12, 0x02);
pub const GREEN: Color32 = Color32::from_rgb(0x5c, 0xc9, 0x8f);
pub const AMBER: Color32 = Color32::from_rgb(0xe0, 0xb3, 0x4d);
pub const RED: Color32 = Color32::from_rgb(0xe3, 0x5d, 0x5d);
pub const RED_SOFT: Color32 = Color32::from_rgb(0xe8, 0x8a, 0x8a);
pub const RED_BORDER: Color32 = Color32::from_rgb(0x6b, 0x40, 0x40);

pub const LOG_INFO: Color32 = Color32::from_rgb(0x7a, 0xb5, 0xe0);

pub fn apply(ctx: &egui::Context)
{
   ctx.set_theme(egui::ThemePreference::Dark);
   ctx.all_styles_mut(|style| {
        style.visuals = egui::Visuals::dark();
        style.visuals.panel_fill = BG_WINDOW;
        style.visuals.window_fill = BG_TOOLBAR;
        style.visuals.extreme_bg_color = BG_WINDOW;
        style.visuals.override_text_color = Some(TEXT_SECONDARY);
        style.visuals.selection.bg_fill = ACCENT.linear_multiply(0.4);
        style.visuals.widgets.noninteractive.bg_stroke.color = BORDER_DIM;
        style.visuals.widgets.inactive.bg_fill = BG_CHIP;
        style.visuals.widgets.hovered.bg_fill = BORDER;
        style.spacing.item_spacing = egui::vec2(8.0, 4.0);
     });
}

/// Section header text ("DEVICES", "LOG"): uppercase, small, dim.
pub fn section_header(text: &str) -> egui::RichText
{
   egui::RichText::new(text.to_uppercase()).color(TEXT_DIM).size(11.5).strong()
}

pub fn mono(text: impl Into<String>, color: Color32) -> egui::RichText
{
   egui::RichText::new(text.into()).monospace().color(color).size(12.0)
}
