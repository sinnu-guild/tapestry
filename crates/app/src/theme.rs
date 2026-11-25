//! Look & feel: a dark, low-glare wall that stays out of the way next to
//! your work, with one warm "thread" accent and loud red only for motion.

use eframe::egui::{self, Color32, Rounding, Stroke};

pub const BG: Color32 = Color32::from_rgb(0x0E, 0x0F, 0x12);
pub const PANEL: Color32 = Color32::from_rgb(0x15, 0x17, 0x1B);
pub const RAISED: Color32 = Color32::from_rgb(0x1D, 0x20, 0x25);
pub const TILE: Color32 = Color32::from_rgb(0x08, 0x09, 0x0B);
pub const LINE: Color32 = Color32::from_rgb(0x2A, 0x2E, 0x35);
pub const TEXT: Color32 = Color32::from_rgb(0xE6, 0xE8, 0xEB);
pub const MUTED: Color32 = Color32::from_rgb(0x8B, 0x92, 0x9C);
/// The "thread" accent.
pub const AMBER: Color32 = Color32::from_rgb(0xE8, 0xA3, 0x3D);
pub const TEAL: Color32 = Color32::from_rgb(0x3D, 0xB8, 0xB0);
pub const MOTION: Color32 = Color32::from_rgb(0xF0, 0x4A, 0x4F);
pub const LIVE: Color32 = Color32::from_rgb(0x3C, 0xC7, 0x7A);
pub const WARN: Color32 = Color32::from_rgb(0xF2, 0xC1, 0x4E);

pub fn apply(ctx: &egui::Context) {
    let mut v = egui::Visuals::dark();
    v.panel_fill = PANEL;
    v.window_fill = PANEL;
    v.extreme_bg_color = BG;
    v.faint_bg_color = RAISED;
    v.override_text_color = Some(TEXT);
    v.window_stroke = Stroke::new(1.0_f32, LINE);
    v.window_rounding = Rounding::same(10.0);
    v.menu_rounding = Rounding::same(8.0);
    v.selection.bg_fill = AMBER.gamma_multiply(0.45);
    v.selection.stroke = Stroke::new(1.0_f32, AMBER);
    v.hyperlink_color = AMBER;
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0_f32, LINE);
    v.widgets.inactive.weak_bg_fill = RAISED;
    v.widgets.inactive.bg_fill = RAISED;
    v.widgets.inactive.rounding = Rounding::same(6.0);
    v.widgets.hovered.weak_bg_fill = Color32::from_rgb(0x27, 0x2B, 0x31);
    v.widgets.hovered.rounding = Rounding::same(6.0);
    v.widgets.active.weak_bg_fill = Color32::from_rgb(0x30, 0x35, 0x3C);
    v.widgets.active.rounding = Rounding::same(6.0);
    ctx.set_visuals(v);

    let mut style = (*ctx.style()).clone();
    style.spacing.item_spacing = egui::vec2(8.0, 6.0);
    style.spacing.button_padding = egui::vec2(10.0, 5.0);
    ctx.set_style(style);

    // egui's UI font (Ubuntu Light) lacks arrows and shapes like → ● ▲ ▼ ◐,
    // which then render as empty boxes. Hack (already bundled with egui, used
    // for monospace) has them, so use it as the last fallback.
    let mut fonts = egui::FontDefinitions::default();
    fonts.families.entry(egui::FontFamily::Proportional).or_default().push("Hack".to_owned());
    ctx.set_fonts(fonts);
}

/// Window/taskbar icon: the same artwork as the .exe icon (assets/tapestry.ico),
/// stored as raw 64×64 RGBA so no image decoder is needed.
pub fn window_icon() -> egui::IconData {
    egui::IconData { rgba: include_bytes!("../assets/icon-64.rgba").to_vec(), width: 64, height: 64 }
}
