//! The dark theme. The colour tokens match `docs/solution.md`, section 6.

use eframe::egui::{
    self, Color32, CornerRadius, FontFamily, FontId, Margin, Stroke, TextStyle, Theme, Vec2,
};

pub const BG: Color32 = Color32::from_rgb(0x0E, 0x10, 0x14);
pub const PANEL: Color32 = Color32::from_rgb(0x15, 0x18, 0x20);
pub const RAISED: Color32 = Color32::from_rgb(0x1B, 0x1F, 0x29);
pub const HOVER: Color32 = Color32::from_rgb(0x23, 0x28, 0x34);
pub const BORDER: Color32 = Color32::from_rgb(0x26, 0x2B, 0x36);
pub const TEXT: Color32 = Color32::from_rgb(0xE7, 0xE9, 0xEE);
pub const MUTED: Color32 = Color32::from_rgb(0x8A, 0x93, 0xA6);
pub const ACCENT: Color32 = Color32::from_rgb(0x3D, 0xD6, 0xC4);
pub const ACCENT_2: Color32 = Color32::from_rgb(0x8B, 0x5C, 0xF6);
pub const DANGER: Color32 = Color32::from_rgb(0xF2, 0x71, 0x6B);
pub const WARNING: Color32 = Color32::from_rgb(0xF5, 0xB8, 0x4D);
/// Text on accent-coloured buttons.
pub const ON_ACCENT: Color32 = Color32::from_rgb(0x05, 0x1B, 0x18);

pub fn apply(ctx: &egui::Context) {
    ctx.set_theme(Theme::Dark);
    ctx.style_mut_of(Theme::Dark, |style| {
        style.text_styles = [
            (
                TextStyle::Heading,
                FontId::new(20.0, FontFamily::Proportional),
            ),
            (TextStyle::Body, FontId::new(14.0, FontFamily::Proportional)),
            (
                TextStyle::Button,
                FontId::new(14.0, FontFamily::Proportional),
            ),
            (
                TextStyle::Small,
                FontId::new(11.5, FontFamily::Proportional),
            ),
            (
                TextStyle::Monospace,
                FontId::new(13.0, FontFamily::Monospace),
            ),
        ]
        .into();
        // No animations: every animated frame is a redraw, and redraws cost energy.
        style.animation_time = 0.0;
        style.interaction.selectable_labels = false;

        let s = &mut style.spacing;
        s.item_spacing = Vec2::new(8.0, 6.0);
        s.button_padding = Vec2::new(12.0, 5.0);
        s.interact_size.y = 26.0;
        s.window_margin = Margin::same(18);

        let v = &mut style.visuals;
        v.dark_mode = true;
        v.panel_fill = PANEL;
        v.window_fill = RAISED;
        v.extreme_bg_color = BG;
        v.faint_bg_color = Color32::from_rgb(0x18, 0x1B, 0x24);
        v.code_bg_color = RAISED;
        v.window_stroke = Stroke::new(1.0, BORDER);
        v.window_corner_radius = CornerRadius::same(12);
        v.menu_corner_radius = CornerRadius::same(8);
        v.hyperlink_color = ACCENT;
        v.warn_fg_color = WARNING;
        v.error_fg_color = DANGER;
        v.weak_text_color = Some(MUTED);
        v.selection.bg_fill = Color32::from_rgb(0x1E, 0x4D, 0x48);
        v.selection.stroke = Stroke::new(1.5, ACCENT);
        // A blinking cursor wakes the app twice a second.
        v.text_cursor.blink = false;

        let w = &mut v.widgets;
        w.noninteractive.bg_fill = PANEL;
        w.noninteractive.weak_bg_fill = PANEL;
        w.noninteractive.bg_stroke = Stroke::new(1.0, BORDER);
        w.noninteractive.fg_stroke = Stroke::new(1.0, TEXT);
        w.noninteractive.corner_radius = CornerRadius::same(8);
        for (state, fill, stroke) in [
            (&mut w.inactive, RAISED, BORDER),
            (&mut w.hovered, HOVER, Color32::from_rgb(0x3A, 0x41, 0x50)),
            (&mut w.active, HOVER, ACCENT),
            (&mut w.open, HOVER, BORDER),
        ] {
            state.bg_fill = fill;
            state.weak_bg_fill = fill;
            state.bg_stroke = Stroke::new(1.0, stroke);
            state.fg_stroke = Stroke::new(1.5, TEXT);
            state.corner_radius = CornerRadius::same(8);
            state.expansion = 0.0;
        }
    });
}
