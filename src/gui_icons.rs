//! Small, offline Remix Icon surface used by the desktop presentation layer.
//!
//! Only icons used by the application are embedded. SVG parsing and texture
//! caching remain owned by egui's installed image loaders.

use eframe::egui;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RemixIcon {
    Dashboard,
    Search,
    Star,
    Resources,
    ReadLater,
    Excerpts,
    Archive,
    Storage,
    Refresh,
    Settings,
    Add,
    Remove,
}

impl RemixIcon {
    fn source(self, filled: bool) -> egui::ImageSource<'static> {
        match (self, filled) {
            (Self::Dashboard, _) => {
                egui::include_image!("../assets/remixicon/dashboard-3-line.svg")
            }
            (Self::Search, _) => egui::include_image!("../assets/remixicon/search-line.svg"),
            (Self::Star, false) => egui::include_image!("../assets/remixicon/star-line.svg"),
            (Self::Star, true) => egui::include_image!("../assets/remixicon/star-fill.svg"),
            (Self::Resources, false) => {
                egui::include_image!("../assets/remixicon/archive-drawer-line.svg")
            }
            (Self::Resources, true) => {
                egui::include_image!("../assets/remixicon/archive-drawer-fill.svg")
            }
            (Self::ReadLater, false) => {
                egui::include_image!("../assets/remixicon/time-line.svg")
            }
            (Self::ReadLater, true) => egui::include_image!("../assets/remixicon/time-fill.svg"),
            (Self::Excerpts, _) => {
                egui::include_image!("../assets/remixicon/double-quotes-l.svg")
            }
            (Self::Archive, false) => {
                egui::include_image!("../assets/remixicon/inbox-archive-line.svg")
            }
            (Self::Archive, true) => {
                egui::include_image!("../assets/remixicon/inbox-archive-fill.svg")
            }
            (Self::Storage, false) => {
                egui::include_image!("../assets/remixicon/database-2-line.svg")
            }
            (Self::Storage, true) => {
                egui::include_image!("../assets/remixicon/database-2-fill.svg")
            }
            (Self::Refresh, _) => egui::include_image!("../assets/remixicon/refresh-line.svg"),
            (Self::Settings, _) => {
                egui::include_image!("../assets/remixicon/settings-3-line.svg")
            }
            (Self::Add, _) => egui::include_image!("../assets/remixicon/add-line.svg"),
            (Self::Remove, _) => egui::include_image!("../assets/remixicon/subtract-line.svg"),
        }
    }

    pub(crate) fn image(
        self,
        filled: bool,
        color: egui::Color32,
        size: f32,
    ) -> egui::Image<'static> {
        egui::Image::new(self.source(filled))
            .fit_to_exact_size(egui::vec2(size, size))
            .tint(color)
    }
}

pub(crate) struct NavigationButton<'a> {
    pub(crate) icon: RemixIcon,
    pub(crate) selected: bool,
    pub(crate) label: &'a str,
    pub(crate) trailing: Option<String>,
    pub(crate) color: egui::Color32,
    pub(crate) selected_fill: egui::Color32,
    pub(crate) width: f32,
}

impl NavigationButton<'_> {
    pub(crate) fn widget(self) -> egui::Button<'static> {
        let mut button = egui::Button::image_and_text(
            self.icon.image(self.selected, self.color, 18.0),
            egui::RichText::new(self.label.to_owned())
                .size(15.0)
                .color(self.color),
        );
        if let Some(trailing) = self.trailing {
            button = button.right_text(egui::RichText::new(trailing).color(self.color));
        }
        button
            .fill(if self.selected {
                self.selected_fill
            } else {
                egui::Color32::TRANSPARENT
            })
            .stroke(egui::Stroke::NONE)
            .corner_radius(egui::CornerRadius::same(4))
            .min_size(egui::vec2(self.width, 34.0))
    }
}
