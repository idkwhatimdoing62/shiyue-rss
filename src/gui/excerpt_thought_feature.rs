//! Private desktop adapter for Excerpt & Thought interactions.
//!
//! Text selection and navigation remain Desktop Interaction concerns. This
//! adapter owns the note/delete drafts, their Modals, validation, and calls to
//! the Excerpt & Thought Lifecycle. The GUI root adopts returned projections.

use eframe::egui;

use crate::db::Db;
use crate::excerpt_thought_lifecycle::{
    Clock, ExcerptCapture, ExcerptTarget, ExcerptThoughtChange, ExcerptThoughtLifecycle,
    ExcerptThoughtProjection, ExcerptView, LifecycleFailure, ProjectionScope,
};
use crate::gui_modal::{self, InitialFocus, ModalHostAction};
use crate::gui_state::ModalKind;

pub(super) struct Dependencies<'a> {
    pub(super) db: &'a Db,
    pub(super) clock: &'a dyn Clock,
}

#[derive(Debug, Clone)]
pub(super) struct CommentDialog {
    pub(super) quote: super::SelectedQuote,
    pub(super) target: ExcerptTarget,
    pub(super) draft: String,
    pub(super) original: String,
    pub(super) error: Option<String>,
    pub(super) focus_input: bool,
}

impl CommentDialog {
    pub(super) fn is_dirty(&self) -> bool {
        self.draft != self.original
    }
}

#[derive(Debug, Clone)]
pub(super) struct DeleteExcerptDialog {
    pub(super) excerpt_id: i64,
    pub(super) selected_text: String,
    pub(super) has_thought: bool,
    pub(super) refresh_scope: ProjectionScope,
}

pub(super) fn new_comment_dialog(
    quote: super::SelectedQuote,
    projection: Option<&ExcerptThoughtProjection>,
) -> CommentDialog {
    let capture = quote.capture();
    let existing = projection.and_then(|projection| projection.match_capture(&capture));
    let target = existing
        .map(|excerpt| ExcerptTarget::Existing(excerpt.id))
        .unwrap_or_else(|| ExcerptTarget::Captured(capture));
    let draft = existing
        .and_then(|excerpt| excerpt.thought.as_ref())
        .map(|thought| thought.content.clone())
        .unwrap_or_default();
    CommentDialog {
        quote,
        target,
        original: draft.clone(),
        draft,
        error: None,
        focus_input: true,
    }
}

pub(super) fn new_edit_dialog(excerpt: &ExcerptView) -> CommentDialog {
    let draft = excerpt
        .thought
        .as_ref()
        .map(|thought| thought.content.clone())
        .unwrap_or_default();
    CommentDialog {
        quote: super::SelectedQuote::from_excerpt(excerpt),
        target: ExcerptTarget::Existing(excerpt.id),
        original: draft.clone(),
        draft,
        error: None,
        focus_input: true,
    }
}

pub(super) fn new_delete_dialog(
    excerpt: &ExcerptView,
    refresh_scope: ProjectionScope,
) -> DeleteExcerptDialog {
    DeleteExcerptDialog {
        excerpt_id: excerpt.id,
        selected_text: excerpt.selected_text.clone(),
        has_thought: excerpt.thought.is_some(),
        refresh_scope,
    }
}

pub(super) enum ModalDraft<'a> {
    Thought(&'a mut CommentDialog),
    Delete(&'a DeleteExcerptDialog),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum InteractionIntent {
    None,
    CloseModal,
    CompleteModal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct DeleteRequest {
    excerpt_id: i64,
    scope: ProjectionScope,
}

pub(super) struct Outcome {
    pub(super) modal_action: ModalHostAction,
    pub(super) interaction: InteractionIntent,
    pub(super) projection: Option<ExcerptThoughtProjection>,
    pub(super) notice: Option<String>,
    pub(super) pending_delete: Option<DeleteRequest>,
}

impl Outcome {
    fn idle(modal_action: ModalHostAction) -> Self {
        Self {
            modal_action,
            interaction: InteractionIntent::None,
            projection: None,
            notice: None,
            pending_delete: None,
        }
    }

    fn success(
        interaction: InteractionIntent,
        projection: ExcerptThoughtProjection,
        notice: impl Into<String>,
    ) -> Self {
        Self {
            modal_action: ModalHostAction::None,
            interaction,
            projection: Some(projection),
            notice: Some(notice.into()),
            pending_delete: None,
        }
    }
}

pub(super) fn show_modal(
    context: &egui::Context,
    draft: ModalDraft<'_>,
    show_discard: bool,
    dependencies: &Dependencies<'_>,
) -> Outcome {
    match draft {
        ModalDraft::Thought(draft) => {
            show_thought_modal(context, draft, show_discard, dependencies)
        }
        ModalDraft::Delete(draft) => show_delete_modal(context, draft, dependencies),
    }
}

pub(super) fn ensure_excerpt(
    capture: ExcerptCapture,
    scope: ProjectionScope,
    dependencies: &Dependencies<'_>,
) -> Outcome {
    apply_change(
        ExcerptThoughtChange::EnsureExcerpt { capture },
        scope,
        dependencies,
        "摘录",
        "已摘录，可在左侧「摘录与想法」查看",
    )
}

pub(super) fn remove_thought(
    excerpt_id: i64,
    scope: ProjectionScope,
    dependencies: &Dependencies<'_>,
) -> Outcome {
    apply_change(
        ExcerptThoughtChange::RemoveThought { excerpt_id },
        scope,
        dependencies,
        "删除想法",
        "想法已删除，摘录仍然保留",
    )
}

pub(super) fn delete_excerpt(
    excerpt_id: i64,
    scope: ProjectionScope,
    dependencies: &Dependencies<'_>,
) -> Outcome {
    apply_change(
        ExcerptThoughtChange::DeleteExcerpt { excerpt_id },
        scope,
        dependencies,
        "删除摘录",
        "摘录已删除",
    )
}

pub(super) fn execute_delete(request: DeleteRequest, dependencies: &Dependencies<'_>) -> Outcome {
    delete_excerpt(request.excerpt_id, request.scope, dependencies)
}

fn apply_change(
    change: ExcerptThoughtChange,
    scope: ProjectionScope,
    dependencies: &Dependencies<'_>,
    action: &str,
    notice: &str,
) -> Outcome {
    match ExcerptThoughtLifecycle::new(dependencies.db, dependencies.clock).apply(change, scope) {
        Ok(outcome) => Outcome::success(InteractionIntent::None, outcome.projection, notice),
        Err(error) => {
            report_failure(action, &error);
            let mut outcome = Outcome::idle(ModalHostAction::None);
            outcome.notice = Some(format!("{action}失败：{}", error.user_message));
            outcome
        }
    }
}

fn show_thought_modal(
    context: &egui::Context,
    dialog: &mut CommentDialog,
    show_discard: bool,
    dependencies: &Dependencies<'_>,
) -> Outcome {
    let mut submit = false;
    let mut cancel = false;
    let response = gui_modal::show(
        context,
        ModalKind::WriteThought,
        show_discard,
        |ui, focus| {
            ui.label("选中的文字：");
            egui::Frame::group(ui.style()).show(ui, |ui| {
                ui.add(
                    egui::Label::new(
                        egui::RichText::new(&dialog.quote.text)
                            .size(17.0)
                            .color(ui.visuals().weak_text_color()),
                    )
                    .wrap(),
                );
            });
            ui.add_space(8.0);
            ui.label("想法内容：");
            let input = ui.add(
                egui::TextEdit::multiline(&mut dialog.draft)
                    .desired_rows(4)
                    .desired_width(440.0)
                    .hint_text("写下你的想法…"),
            );
            if focus == InitialFocus::PrimaryField && dialog.focus_input {
                input.request_focus();
                dialog.focus_input = false;
            }
            if let Some(error) = &dialog.error {
                ui.colored_label(egui::Color32::RED, error);
            }
            ui.horizontal(|ui| {
                if ui.button("保存想法").clicked() {
                    submit = true;
                }
                if ui.button("取消").clicked() {
                    cancel = true;
                }
            });
        },
    );
    let mut outcome = Outcome::idle(response.action);
    if cancel {
        outcome.interaction = InteractionIntent::CloseModal;
        return outcome;
    }
    if !submit {
        return outcome;
    }
    let draft = dialog.draft.clone();
    if draft.trim().is_empty() {
        dialog.error = Some("想法内容不能为空".to_owned());
        return outcome;
    }
    match ExcerptThoughtLifecycle::new(dependencies.db, dependencies.clock).apply(
        ExcerptThoughtChange::PutThought {
            target: dialog.target.clone(),
            content: draft,
        },
        ProjectionScope::Article(dialog.quote.article_id),
    ) {
        Ok(result) => Outcome::success(
            InteractionIntent::CompleteModal,
            result.projection,
            "想法已保存，可在左侧「摘录与想法」查看",
        ),
        Err(error) => {
            dialog.error = Some(format!("想法保存失败：{}", error.user_message));
            tracing::warn!(detail = %error.technical_detail, "put thought failed");
            outcome
        }
    }
}

fn show_delete_modal(
    context: &egui::Context,
    dialog: &DeleteExcerptDialog,
    _dependencies: &Dependencies<'_>,
) -> Outcome {
    let mut confirm = false;
    let mut cancel = false;
    let response = gui_modal::show(context, ModalKind::DeleteExcerpt, false, |ui, _| {
        ui.label("确定删除这条摘录吗？");
        egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.add(egui::Label::new(&dialog.selected_text).wrap());
        });
        if dialog.has_thought {
            ui.colored_label(
                ui.visuals().warn_fg_color,
                "附在这条摘录上的想法也会一起删除，此操作无法撤销。",
            );
        }
        ui.horizontal(|ui| {
            if ui.button("删除摘录").clicked() {
                confirm = true;
            }
            if ui.button("取消").clicked() {
                cancel = true;
            }
        });
    });
    let mut outcome = Outcome::idle(response.action);
    if cancel {
        outcome.interaction = InteractionIntent::CloseModal;
        return outcome;
    }
    if !confirm {
        return outcome;
    }
    outcome.interaction = InteractionIntent::CompleteModal;
    outcome.pending_delete = Some(DeleteRequest {
        excerpt_id: dialog.excerpt_id,
        scope: dialog.refresh_scope,
    });
    outcome
}

fn report_failure(action: &str, error: &LifecycleFailure) {
    tracing::warn!(
        action,
        kind = ?error.kind,
        operation = ?error.operation,
        detail = %error.technical_detail,
        "excerpt/thought lifecycle failed"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn comment_draft_dirty_state_is_owned_by_the_draft() {
        let quote = super::super::SelectedQuote {
            article_id: 7,
            text: "excerpt".into(),
            start_offset: Some(0),
            end_offset: Some(7),
            anchor_prefix: String::new(),
            anchor_suffix: String::new(),
        };
        let draft = CommentDialog {
            quote,
            target: ExcerptTarget::Existing(3),
            draft: "edited".into(),
            original: "original".into(),
            error: None,
            focus_input: false,
        };
        assert!(draft.is_dirty());
    }

    #[test]
    fn gui_root_no_longer_calls_excerpt_lifecycle_writes() {
        let gui_root = include_str!("../gui.rs");
        for delegated in [
            "ExcerptThoughtChange::EnsureExcerpt",
            "ExcerptThoughtChange::PutThought",
            "ExcerptThoughtChange::RemoveThought",
            "ExcerptThoughtChange::DeleteExcerpt",
            "fn show_comment_dialog",
            "fn show_delete_excerpt_dialog",
        ] {
            assert!(
                !gui_root.contains(delegated),
                "GUI root still owns {delegated}"
            );
        }
    }
}
