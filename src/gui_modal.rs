//! The single presentation seam for blocking desktop modals.
//!
//! Feature adapters render fields and perform domain work. This host owns the
//! blocker, window chrome, layering, close semantics and dirty-discard guard.

use eframe::egui;

use crate::gui_state::ModalKind;
use crate::gui_theme::ReaderTheme;

pub(crate) const BLOCKER_ORDER: egui::Order = egui::Order::Middle;
pub(crate) const MODAL_ORDER: egui::Order = egui::Order::Foreground;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InitialFocus {
    None,
    PrimaryField,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Chrome {
    Default,
    Reader,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct ModalPresentation {
    pub(crate) title: &'static str,
    pub(crate) id: &'static str,
    pub(crate) resizable: bool,
    pub(crate) default_width: Option<f32>,
    pub(crate) default_size: Option<egui::Vec2>,
    pub(crate) min_size: Option<egui::Vec2>,
    pub(crate) centered: bool,
    pub(crate) initial_focus: InitialFocus,
    chrome: Chrome,
}

pub(crate) fn presentation(kind: ModalKind) -> ModalPresentation {
    use InitialFocus::{None, PrimaryField};
    use ModalKind::*;

    match kind {
        AddFeed => {
            modal("添加订阅", "modal-add-feed", false, PrimaryField).with_default_width(480.0)
        }
        DeleteFeed => modal("删除订阅", "modal-delete-feed", false, None).centered(),
        Search => modal("全文搜索", "library-full-text-search", true, PrimaryField)
            .reader()
            .with_default_size(760.0, 640.0)
            .with_min_size(520.0, 420.0),
        EditTags => {
            modal("文章标签", "modal-edit-tags", false, PrimaryField).with_default_width(430.0)
        }
        WriteThought => {
            modal("写想法", "modal-write-thought", true, PrimaryField).with_default_width(460.0)
        }
        DeleteExcerpt => {
            modal("删除摘录", "modal-delete-excerpt", false, None).with_default_width(440.0)
        }
        SaveWebPage => modal("保存网页", "modal-save-web-page", true, PrimaryField)
            .reader()
            .with_default_width(620.0)
            .with_min_size(480.0, 0.0),
        DeleteWebPage => {
            modal("删除本地网页", "modal-delete-web-page", false, None).with_default_width(420.0)
        }
        AddResource => modal("添加资源网站", "modal-add-resource", false, PrimaryField)
            .with_default_width(520.0),
        DeleteResource => modal("永久删除资源", "modal-delete-resource", false, None).centered(),
        ImportResources => modal("导入已有网页收藏", "modal-import-resources", true, None)
            .with_default_size(650.0, 520.0),
        RestoreBackup => modal("恢复数据库备份？", "modal-restore-backup", false, None).centered(),
        ClearImages => modal("清空图片缓存？", "modal-clear-images", false, None).centered(),
    }
}

const fn modal(
    title: &'static str,
    id: &'static str,
    resizable: bool,
    initial_focus: InitialFocus,
) -> ModalPresentation {
    ModalPresentation {
        title,
        id,
        resizable,
        default_width: None,
        default_size: None,
        min_size: None,
        centered: false,
        initial_focus,
        chrome: Chrome::Default,
    }
}

impl ModalPresentation {
    const fn with_default_width(mut self, width: f32) -> Self {
        self.default_width = Some(width);
        self
    }

    const fn with_default_size(mut self, width: f32, height: f32) -> Self {
        self.default_size = Some(egui::vec2(width, height));
        self
    }

    const fn with_min_size(mut self, width: f32, height: f32) -> Self {
        self.min_size = Some(egui::vec2(width, height));
        self
    }

    const fn centered(mut self) -> Self {
        self.centered = true;
        self
    }

    const fn reader(mut self) -> Self {
        self.chrome = Chrome::Reader;
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ModalHostAction {
    None,
    RequestClose,
    KeepEditing,
    ConfirmDiscard,
}

pub(crate) struct ModalHostResponse<T> {
    pub(crate) inner: Option<T>,
    pub(crate) action: ModalHostAction,
}

pub(crate) fn show<T>(
    ctx: &egui::Context,
    kind: ModalKind,
    discard_pending: bool,
    body: impl FnOnce(&mut egui::Ui, InitialFocus) -> T,
) -> ModalHostResponse<T> {
    show_blocker(ctx);

    let spec = presentation(kind);
    let mut open = true;
    let mut window = egui::Window::new(spec.title)
        .id(egui::Id::new(spec.id))
        .order(MODAL_ORDER)
        .open(&mut open)
        .collapsible(false)
        .resizable(spec.resizable);
    if let Some(width) = spec.default_width {
        window = window.default_width(width);
    }
    if let Some(size) = spec.default_size {
        window = window.default_size(size);
    }
    if let Some(size) = spec.min_size {
        window = window.min_size(size);
    }
    if spec.centered {
        window = window.anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO);
    }
    if spec.chrome == Chrome::Reader {
        let theme = ReaderTheme::sspai();
        window = window.frame(
            egui::Frame::new()
                .fill(theme.canvas)
                .stroke(egui::Stroke::new(1.0, theme.border))
                .corner_radius(egui::CornerRadius::same(9))
                .inner_margin(egui::Margin::same(14))
                .shadow(egui::Shadow {
                    offset: [0, 5],
                    blur: 18,
                    spread: 0,
                    color: egui::Color32::from_black_alpha(45),
                }),
        );
    }

    let mut guard_action = ModalHostAction::None;
    let inner = window
        .show(ctx, |ui| {
            let result = body(ui, spec.initial_focus);
            if discard_pending {
                ui.separator();
                ui.colored_label(egui::Color32::from_rgb(190, 86, 86), "有尚未保存的修改");
                ui.weak("继续刚才的操作会丢弃这些修改。");
                ui.horizontal(|ui| {
                    if ui.button("继续编辑").clicked() {
                        guard_action = ModalHostAction::KeepEditing;
                    }
                    if ui.button("放弃修改").clicked() {
                        guard_action = ModalHostAction::ConfirmDiscard;
                    }
                });
            }
            result
        })
        .and_then(|response| response.inner);

    let escape = ctx.input_mut(|input| input.consume_key(egui::Modifiers::NONE, egui::Key::Escape));
    let action = resolve_action(open, escape, discard_pending, guard_action);
    ModalHostResponse { inner, action }
}

fn show_blocker(ctx: &egui::Context) {
    let content_rect = ctx.content_rect();
    egui::Area::new(egui::Id::new("modal-background-blocker"))
        .order(BLOCKER_ORDER)
        .fixed_pos(content_rect.min)
        .show(ctx, |ui| {
            let local_rect = egui::Rect::from_min_size(egui::Pos2::ZERO, content_rect.size());
            ui.painter().rect_filled(
                local_rect,
                egui::CornerRadius::ZERO,
                egui::Color32::from_black_alpha(20),
            );
            ui.allocate_rect(local_rect, egui::Sense::click_and_drag());
        });
}

fn resolve_action(
    open: bool,
    escape: bool,
    discard_pending: bool,
    explicit: ModalHostAction,
) -> ModalHostAction {
    if explicit != ModalHostAction::None {
        return explicit;
    }
    if escape && discard_pending {
        ModalHostAction::KeepEditing
    } else if escape || !open {
        ModalHostAction::RequestClose
    } else {
        ModalHostAction::None
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    const ALL_KINDS: [ModalKind; 13] = [
        ModalKind::AddFeed,
        ModalKind::DeleteFeed,
        ModalKind::Search,
        ModalKind::EditTags,
        ModalKind::WriteThought,
        ModalKind::DeleteExcerpt,
        ModalKind::SaveWebPage,
        ModalKind::DeleteWebPage,
        ModalKind::AddResource,
        ModalKind::DeleteResource,
        ModalKind::ImportResources,
        ModalKind::RestoreBackup,
        ModalKind::ClearImages,
    ];

    #[test]
    fn every_modal_has_unique_explicit_presentation_metadata() {
        let mut ids = HashSet::new();
        for kind in ALL_KINDS {
            let spec = presentation(kind);
            assert!(!spec.title.is_empty());
            assert!(ids.insert(spec.id), "duplicate id for {kind:?}");
        }
    }

    #[test]
    fn presentation_matrix_is_stable_for_every_modal_kind() {
        let expected = [
            (ModalKind::AddFeed, false, InitialFocus::PrimaryField),
            (ModalKind::DeleteFeed, false, InitialFocus::None),
            (ModalKind::Search, true, InitialFocus::PrimaryField),
            (ModalKind::EditTags, false, InitialFocus::PrimaryField),
            (ModalKind::WriteThought, true, InitialFocus::PrimaryField),
            (ModalKind::DeleteExcerpt, false, InitialFocus::None),
            (ModalKind::SaveWebPage, true, InitialFocus::PrimaryField),
            (ModalKind::DeleteWebPage, false, InitialFocus::None),
            (ModalKind::AddResource, false, InitialFocus::PrimaryField),
            (ModalKind::DeleteResource, false, InitialFocus::None),
            (ModalKind::ImportResources, true, InitialFocus::None),
            (ModalKind::RestoreBackup, false, InitialFocus::None),
            (ModalKind::ClearImages, false, InitialFocus::None),
        ];
        for (kind, resizable, initial_focus) in expected {
            let spec = presentation(kind);
            assert_eq!(spec.resizable, resizable, "{kind:?}");
            assert_eq!(spec.initial_focus, initial_focus, "{kind:?}");
        }

        assert_eq!(
            presentation(ModalKind::Search).default_size,
            Some(egui::vec2(760.0, 640.0))
        );
        assert_eq!(
            presentation(ModalKind::Search).min_size,
            Some(egui::vec2(520.0, 420.0))
        );
        assert!(presentation(ModalKind::DeleteFeed).centered);
        assert!(presentation(ModalKind::RestoreBackup).centered);
    }

    #[test]
    fn blocker_and_modal_have_a_stable_layer_contract() {
        assert_eq!(BLOCKER_ORDER, egui::Order::Middle);
        assert_eq!(MODAL_ORDER, egui::Order::Foreground);
    }

    #[test]
    fn close_and_escape_share_the_dirty_guard() {
        assert_eq!(
            resolve_action(false, false, false, ModalHostAction::None),
            ModalHostAction::RequestClose
        );
        assert_eq!(
            resolve_action(true, true, false, ModalHostAction::None),
            ModalHostAction::RequestClose
        );
        assert_eq!(
            resolve_action(true, true, true, ModalHostAction::None),
            ModalHostAction::KeepEditing
        );
        assert_eq!(
            resolve_action(true, false, true, ModalHostAction::ConfirmDiscard,),
            ModalHostAction::ConfirmDiscard
        );
    }

    #[test]
    fn destructive_modals_never_request_primary_focus() {
        for kind in [
            ModalKind::DeleteFeed,
            ModalKind::DeleteExcerpt,
            ModalKind::DeleteWebPage,
            ModalKind::DeleteResource,
            ModalKind::RestoreBackup,
            ModalKind::ClearImages,
        ] {
            assert_eq!(presentation(kind).initial_focus, InitialFocus::None);
        }
    }
}
