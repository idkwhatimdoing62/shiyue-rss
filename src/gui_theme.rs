//! Visual tokens for the desktop adapter.
//!
//! Keeping theme policy outside the large event/rendering module prevents
//! individual pages from inventing slightly different colors.

use eframe::egui;

#[derive(Clone, Copy)]
pub(crate) struct ReaderTheme {
    pub(crate) canvas: egui::Color32,
    pub(crate) panel: egui::Color32,
    pub(crate) text: egui::Color32,
    pub(crate) muted: egui::Color32,
    pub(crate) subtle: egui::Color32,
    pub(crate) accent: egui::Color32,
    pub(crate) link: egui::Color32,
    pub(crate) border: egui::Color32,
    pub(crate) code_bg: egui::Color32,
    pub(crate) selected_bg: egui::Color32,
}

impl ReaderTheme {
    pub(crate) fn sspai() -> Self {
        Self {
            // A warm neutral canvas gives the reading surface a clear edge
            // without adding decoration or heavy shadows.
            canvas: egui::Color32::from_rgb(247, 248, 250),
            panel: egui::Color32::from_rgb(255, 255, 255),
            text: egui::Color32::from_rgb(31, 41, 51),
            muted: egui::Color32::from_rgb(82, 94, 106),
            // Metadata stays quiet, but remains readable at small sizes on white.
            subtle: egui::Color32::from_rgb(99, 110, 122),
            // The accent is intentionally dark enough for text and focus indicators.
            accent: egui::Color32::from_rgb(183, 70, 62),
            link: egui::Color32::from_rgb(177, 58, 49),
            border: egui::Color32::from_rgb(218, 224, 230),
            code_bg: egui::Color32::from_rgb(241, 244, 247),
            selected_bg: egui::Color32::from_rgb(255, 239, 236),
        }
    }
}
