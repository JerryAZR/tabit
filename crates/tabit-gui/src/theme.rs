//! Theme tokens — every visual constant lives here.
//!
//! The design contract (ROADMAP item 7): widgets consume semantic
//! tokens, never raw colors or magic paddings; a polish pass swaps
//! this module (or an egui theming crate behind it) without touching
//! widget code. Skeleton tokens are deliberately minimal — unused
//! tokens rot.

use egui::Color32;

/// Accent for interactive highlights and the running indicator.
pub const ACCENT: Color32 = Color32::from_rgb(0x6C, 0x9E, 0xD8);
/// Destructive outcomes and failed states.
pub const ERROR: Color32 = Color32::from_rgb(0xD9, 0x6C, 0x6C);
/// Assistant text.
pub const TEXT: Color32 = Color32::from_rgb(0xDD, 0xDD, 0xDD);
/// User-authored text (messages, pending).
pub const USER_TEXT: Color32 = Color32::from_rgb(0xC9, 0xD8, 0xC9);
/// Secondary text: labels, tool rows, reasoning.
pub const MUTED: Color32 = Color32::from_rgb(0x9A, 0x9A, 0x9A);

/// Row inset inside the transcript.
pub const ROW_INSET: f32 = 8.0;
/// Space between transcript rows.
pub const ROW_GAP: f32 = 6.0;

/// Apply the skeleton's base visuals (dark). A theming crate takes
/// over this slot at polish time.
pub fn apply(ctx: &egui::Context) {
    ctx.set_theme(egui::ThemePreference::Dark);
    ctx.style_mut_of(egui::Theme::Dark, |style| {
        style.visuals.panel_fill = Color32::from_rgb(0x1E, 0x1E, 0x22);
    });
}
