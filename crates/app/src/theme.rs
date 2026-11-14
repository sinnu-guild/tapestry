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
}

/// A small woven square, drawn in code: amber weft over teal warp.
pub fn window_icon() -> egui::IconData {
    const N: u32 = 64;
    let mut rgba = Vec::with_capacity((N * N * 4) as usize);
    let band = 8u32;
    for y in 0..N {
        for x in 0..N {
            let (bx, by) = (x / band, y / band);
            let in_x_gap = x % band == 0 || x % band == band - 1;
            let in_y_gap = y % band == 0 || y % band == band - 1;
            // Rounded-square mask.
            let (cx, cy) = (x as f32 - 31.5, y as f32 - 31.5);
            let r = 12.0;
            let qx = (cx.abs() - (32.0 - r)).max(0.0);
            let qy = (cy.abs() - (32.0 - r)).max(0.0);
            let inside = (qx * qx + qy * qy).sqrt() <= r;
            let over = (bx + by) % 2 == 0;
            let c = if !inside {
                [0, 0, 0, 0]
            } else if over && !in_y_gap {
                [AMBER.r(), AMBER.g(), AMBER.b(), 255]
            } else if !over && !in_x_gap {
                [TEAL.r(), TEAL.g(), TEAL.b(), 255]
            } else {
                [0x15, 0x17, 0x1B, 255]
            };
            rgba.extend_from_slice(&c);
        }
    }
    egui::IconData { rgba, width: N, height: N }
}
