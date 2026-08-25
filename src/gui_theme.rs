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
    pub(crate) accent: egui::Color32,
    pub(crate) link: egui::Color32,
    pub(crate) border: egui::Color32,
    pub(crate) code_bg: egui::Color32,
    pub(crate) selected_bg: egui::Color32,
}

impl ReaderTheme {
    pub(crate) fn sspai() -> Self {
        Self {
            canvas: egui::Color32::from_rgb(255, 255, 255),
            panel: egui::Color32::from_rgb(250, 250, 250),
            text: egui::Color32::from_rgb(51, 51, 51),
            muted: egui::Color32::from_rgb(136, 136, 136),
            accent: egui::Color32::from_rgb(255, 126, 121),
            link: egui::Color32::from_rgb(242, 47, 39),
            border: egui::Color32::from_rgb(238, 238, 238),
            code_bg: egui::Color32::from_rgb(248, 248, 248),
            selected_bg: egui::Color32::from_rgb(255, 241, 240),
        }
    }
}
