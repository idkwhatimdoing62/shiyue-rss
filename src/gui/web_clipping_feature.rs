//! Private desktop adapter for Web Clipping interactions.
//!
//! The adapter owns capture and deletion drafts, modal rendering, and calls to
//! the Web Clipping Lifecycle. Desktop Interaction keeps Route/Modal reduction,
//! terminal Capture Lease adoption, Projection acceptance, and Notices.

use eframe::egui;

use crate::article_library_lifecycle::{ArticleLibraryProjection, ProjectionScope};
use crate::gui_modal::{self, InitialFocus, ModalHostAction};
use crate::gui_state::ModalKind;
use crate::gui_theme::ReaderTheme;
use crate::web_clipping_lifecycle::{
    CancelDisposition, CaptureLease, CaptureRequest, DeleteRequest, WebClippingLifecycle,
};

pub(super) struct Dependencies<'a> {
    pub(super) lifecycle: &'a WebClippingLifecycle,
    pub(super) delete_scope: ProjectionScope,
}

#[derive(Debug)]
pub(super) struct WebClipDialog {
    /// 可以是 http(s) 地址，也可以是用户粘贴的完整 HTML / HTML 片段。
    source: String,
    title: String,
    /// 粘贴 HTML 时用于解析相对链接；网址抓取模式会自动使用最终地址。
    base_url: String,
    capture: Option<CaptureLease>,
    error: Option<String>,
    focus_input: bool,
}

impl Default for WebClipDialog {
    fn default() -> Self {
        Self {
            source: String::new(),
            title: String::new(),
            base_url: String::new(),
            capture: None,
            error: None,
            focus_input: true,
        }
    }
}

impl WebClipDialog {
    pub(super) fn capture_snapshot(
        &self,
    ) -> Option<crate::web_clipping_lifecycle::CaptureSnapshot> {
        self.capture
            .as_ref()
            .map(crate::web_clipping_lifecycle::CaptureLease::snapshot)
    }

    pub(super) fn owns_capture(&self, id: crate::web_clipping_lifecycle::CaptureId) -> bool {
        self.capture
            .as_ref()
            .is_some_and(|capture| capture.id() == id)
    }

    pub(super) fn fail_capture(&mut self, message: String) {
        self.capture = None;
        self.error = Some(message);
    }

    pub(super) fn request_cancel(&self) -> Option<CancelDisposition> {
        self.capture
            .as_ref()
            .map(crate::web_clipping_lifecycle::CaptureLease::request_cancel)
    }

    pub(super) fn is_dirty(&self) -> bool {
        !self.source.trim().is_empty()
            || !self.title.trim().is_empty()
            || !self.base_url.trim().is_empty()
            || self.capture.is_some()
    }
}

#[derive(Debug, Clone)]
pub(super) struct DeleteWebClipDialog {
    article_id: i64,
    title: String,
}

impl DeleteWebClipDialog {
    pub(super) fn new(article_id: i64, title: String) -> Self {
        Self { article_id, title }
    }
}

pub(super) enum ModalDraft<'a> {
    Save(&'a mut WebClipDialog),
    Delete(&'a DeleteWebClipDialog),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum InteractionIntent {
    None,
    CompleteModal,
}

pub(super) struct Outcome {
    pub(super) modal_action: ModalHostAction,
    pub(super) interaction: InteractionIntent,
    pub(super) projection: Option<ArticleLibraryProjection>,
    pub(super) deleted_article_id: Option<i64>,
    pub(super) notice: Option<String>,
}

impl Outcome {
    fn idle(modal_action: ModalHostAction) -> Self {
        Self {
            modal_action,
            interaction: InteractionIntent::None,
            projection: None,
            deleted_article_id: None,
            notice: None,
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
        ModalDraft::Save(draft) => show_save_modal(context, draft, show_discard, dependencies),
        ModalDraft::Delete(draft) => show_delete_modal(context, draft, dependencies),
    }
}

fn show_save_modal(
    context: &egui::Context,
    draft: &mut WebClipDialog,
    show_discard: bool,
    dependencies: &Dependencies<'_>,
) -> Outcome {
    let capture_snapshot = draft.capture_snapshot();
    let capture_active = capture_snapshot
        .as_ref()
        .is_some_and(|snapshot| !snapshot.state.is_terminal());
    let theme = ReaderTheme::sspai();
    let mut import = false;
    let mut cancel = false;
    let response = gui_modal::show(
        context,
        ModalKind::SaveWebPage,
        show_discard,
        |ui, focus| {
            ui.label(
                egui::RichText::new("粘贴网页地址，或直接粘贴 HTML 源码")
                    .size(17.0)
                    .color(theme.text),
            );
            ui.add_space(4.0);
            ui.label(
                egui::RichText::new("正文会作为本地快照保存；网页中的远程图片仍需要联网加载。")
                    .size(13.0)
                    .color(theme.muted),
            );
            ui.add_space(12.0);
            ui.label("网页地址 / HTML");
            let source_response = ui.add_enabled(
                !capture_active,
                egui::TextEdit::multiline(&mut draft.source)
                    .desired_rows(10)
                    .desired_width(f32::INFINITY)
                    .hint_text("https://example.com/article\n\n或\n\n<article>…</article>"),
            );
            if focus == InitialFocus::PrimaryField && draft.focus_input {
                source_response.request_focus();
                draft.focus_input = false;
            }
            if source_response.changed() {
                draft.error = None;
            }
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                ui.label("标题（可选）");
                ui.add_enabled(
                    !capture_active,
                    egui::TextEdit::singleline(&mut draft.title)
                        .desired_width(ui.available_width())
                        .hint_text("留空则从 HTML 自动识别"),
                );
            });
            ui.horizontal(|ui| {
                ui.label("基础网址（可选）");
                ui.add_enabled(
                    !capture_active,
                    egui::TextEdit::singleline(&mut draft.base_url)
                        .desired_width(ui.available_width())
                        .hint_text("仅粘贴 HTML 时，用于解析相对图片和链接"),
                );
            });
            if let Some(error) = &draft.error {
                ui.add_space(6.0);
                ui.label(egui::RichText::new(error).color(theme.link).size(13.0));
            }
            ui.add_space(12.0);
            ui.horizontal(|ui| {
                let import_label = match capture_snapshot.as_ref().map(|snapshot| &snapshot.state) {
                    Some(crate::web_clipping_lifecycle::CaptureState::Fetching) => "正在抓取…",
                    Some(crate::web_clipping_lifecycle::CaptureState::Preparing) => "正在整理…",
                    Some(crate::web_clipping_lifecycle::CaptureState::Committing) => "正在保存…",
                    _ => "保存网页",
                };
                if ui
                    .add_enabled(
                        !capture_active,
                        egui::Button::new(import_label)
                            .fill(theme.accent)
                            .stroke(egui::Stroke::NONE),
                    )
                    .clicked()
                {
                    import = true;
                }
                if capture_active {
                    ui.spinner();
                }
                let cancel_label = if capture_active {
                    "关闭窗口"
                } else {
                    "取消"
                };
                if ui.button(cancel_label).clicked() {
                    cancel = true;
                }
            });
        },
    );
    let mut outcome = Outcome::idle(response.action);
    if cancel {
        if let Some(disposition) = draft.request_cancel()
            && disposition == CancelDisposition::CommitAlreadyStarted
        {
            tracing::info!("web clipping modal closed while commit completes");
        }
        outcome.interaction = InteractionIntent::CompleteModal;
    } else if import {
        begin_capture(context, draft, dependencies);
    }
    outcome
}

fn begin_capture(
    context: &egui::Context,
    draft: &mut WebClipDialog,
    dependencies: &Dependencies<'_>,
) {
    if draft
        .capture_snapshot()
        .as_ref()
        .is_some_and(|snapshot| !snapshot.state.is_terminal())
    {
        return;
    }
    let request = CaptureRequest {
        source: draft.source.clone(),
        title_override: non_empty_owned(&draft.title),
        pasted_html_base_url: non_empty_owned(&draft.base_url),
        refresh_scope: ProjectionScope::ArticleBookmarks,
    };
    draft.error = None;
    match dependencies.lifecycle.begin_capture(request) {
        Ok(capture) => {
            draft.capture = Some(capture);
            context.request_repaint_after(std::time::Duration::from_millis(80));
        }
        Err(error) => {
            tracing::warn!(
                kind = ?error.failure.kind,
                detail = %error.failure.technical_detail,
                "web clipping capture admission failed"
            );
            draft.error = Some(error.user_message);
        }
    }
}

fn show_delete_modal(
    context: &egui::Context,
    draft: &DeleteWebClipDialog,
    dependencies: &Dependencies<'_>,
) -> Outcome {
    let response = gui_modal::show(context, ModalKind::DeleteWebPage, false, |ui, _| {
        let mut delete = false;
        let mut cancel = false;
        ui.label(format!("确定永久删除「{}」吗？", draft.title));
        ui.weak("正文快照及其摘录、想法会一起删除，无法撤销。");
        ui.add_space(12.0);
        ui.horizontal(|ui| {
            if ui.button("永久删除").clicked() {
                delete = true;
            }
            if ui.button("取消").clicked() {
                cancel = true;
            }
        });
        (delete, cancel)
    });
    let mut outcome = Outcome::idle(response.action);
    let Some((delete, cancel)) = response.inner else {
        return outcome;
    };
    if cancel {
        outcome.interaction = InteractionIntent::CompleteModal;
        return outcome;
    }
    if !delete {
        return outcome;
    }
    match dependencies.lifecycle.delete(DeleteRequest {
        article_id: draft.article_id,
        refresh_scope: dependencies.delete_scope,
    }) {
        Ok(result) => {
            tracing::info!(
                article_id = result.deleted.article_id,
                detached_resources = ?result.detached_resource_ids,
                "web clipping deleted"
            );
            outcome.projection = Some(result.projection);
            outcome.deleted_article_id = Some(draft.article_id);
            outcome.notice = Some("本地网页已永久删除".into());
        }
        Err(error) => {
            tracing::warn!(
                kind = ?error.kind,
                detail = %error.technical_detail,
                "web clipping delete failed"
            );
            outcome.notice = Some(error.user_message);
        }
    }
    outcome.interaction = InteractionIntent::CompleteModal;
    outcome
}

fn non_empty_owned(value: &str) -> Option<String> {
    (!value.trim().is_empty()).then(|| value.trim().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_web_clip_draft_is_clean() {
        assert!(!WebClipDialog::default().is_dirty());
    }

    #[test]
    fn web_clip_draft_tracks_source_and_optional_fields() {
        let mut draft = WebClipDialog {
            source: "https://example.com/article".into(),
            ..Default::default()
        };
        assert!(draft.is_dirty());
        draft.source.clear();
        draft.title = "Title".into();
        assert!(draft.is_dirty());
    }

    #[test]
    fn delete_draft_keeps_article_identity() {
        let draft = DeleteWebClipDialog::new(42, "Example".into());
        assert_eq!(draft.article_id, 42);
        assert_eq!(draft.title, "Example");
    }

    #[test]
    fn gui_root_does_not_render_web_clip_modals_or_call_lifecycle_changes() {
        let gui_root = include_str!("../gui.rs");
        for delegated in [
            "fn show_web_clip_dialog",
            "fn show_delete_web_clip_dialog",
            "begin_capture(request)",
            ".web_clipping_lifecycle\n                .delete(",
        ] {
            assert!(
                !gui_root.contains(delegated),
                "GUI root still owns {delegated}"
            );
        }
    }
}
