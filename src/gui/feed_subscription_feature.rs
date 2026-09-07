//! Private desktop adapter for Feed Subscription maintenance.
//!
//! Drafts stay in the Desktop Interaction state owned by `gui.rs`. This
//! module renders those drafts, validates settings, invokes the Feed
//! Subscription Lifecycle, and returns typed outcomes for the GUI root to
//! adopt.

use std::path::Path;

use eframe::egui;

use crate::config::parse_duration;
use crate::feed_subscription::{
    ChangeDisposition, FeedSubscriptions, InitialRefreshOutcome, SubscriptionChange,
    SubscriptionError,
};
use crate::gui_modal::{self, InitialFocus, ModalHostAction};
use crate::gui_state::ModalKind;
use crate::history_backfill_workflow::{BackfillStatus, HistoryBackfillWorkflow};
use crate::model::Feed;
use crate::rss_refresh_workflow::RssRefreshWorkflow;

pub(super) struct Dependencies<'a> {
    pub(super) database: &'a Path,
    pub(super) refresh: &'a RssRefreshWorkflow,
    pub(super) history: &'a HistoryBackfillWorkflow,
}

#[derive(Debug)]
pub(super) struct AddDraft {
    url: String,
    error: Option<String>,
    focus_input: bool,
}

impl Default for AddDraft {
    fn default() -> Self {
        Self {
            url: String::new(),
            error: None,
            focus_input: true,
        }
    }
}

impl AddDraft {
    pub(super) fn is_dirty(&self) -> bool {
        !self.url.trim().is_empty()
    }
}

#[derive(Debug, Clone)]
pub(super) struct SettingsDraft {
    feed_id: i64,
    title: String,
    url: String,
    disabled: bool,
    original_disabled: bool,
    interval_draft: String,
    original_interval: String,
    error: Option<String>,
}

impl SettingsDraft {
    pub(super) fn from_feed(feed: &Feed) -> Self {
        let interval = feed
            .interval_secs
            .map(|seconds| format!("{seconds}s"))
            .unwrap_or_default();
        Self {
            feed_id: feed.id,
            title: feed.title.clone().unwrap_or_else(|| feed.url.clone()),
            url: feed.url.clone(),
            disabled: feed.disabled,
            original_disabled: feed.disabled,
            interval_draft: interval.clone(),
            original_interval: interval,
            error: None,
        }
    }

    pub(super) fn feed_id(&self) -> i64 {
        self.feed_id
    }

    pub(super) fn is_dirty(&self) -> bool {
        self.disabled != self.original_disabled
            || self.interval_draft.trim() != self.original_interval
    }
}

#[derive(Debug, Clone)]
pub(super) struct DeleteDraft {
    id: i64,
    title: String,
}

impl DeleteDraft {
    pub(super) fn new(id: i64, title: String) -> Self {
        Self { id, title }
    }
}

pub(super) enum ModalDraft<'a> {
    Add(&'a mut AddDraft),
    Delete(&'a DeleteDraft),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum InteractionIntent {
    None,
    CompleteModal,
    ClosePanel,
    FinishPanel,
    KeepEditing,
    ConfirmDiscard,
}

pub(super) struct Outcome {
    pub(super) modal_action: ModalHostAction,
    pub(super) interaction: InteractionIntent,
    pub(super) reload: bool,
    pub(super) select_feed_id: Option<i64>,
    pub(super) notice: Option<String>,
}

impl Outcome {
    fn idle(modal_action: ModalHostAction) -> Self {
        Self {
            modal_action,
            interaction: InteractionIntent::None,
            reload: false,
            select_feed_id: None,
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
        ModalDraft::Add(draft) => show_add_modal(context, draft, show_discard, dependencies),
        ModalDraft::Delete(draft) => show_delete_modal(context, draft, dependencies),
    }
}

pub(super) fn show_panel(
    ui: &mut egui::Ui,
    draft: &mut SettingsDraft,
    show_discard: bool,
    dependencies: &Dependencies<'_>,
) -> Outcome {
    let mut save = false;
    let mut close = false;
    let mut history_action = None;
    ui.horizontal(|ui| {
        ui.heading("订阅设置");
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.button("关闭").clicked() {
                close = true;
            }
        });
    });
    ui.separator();
    ui.label(egui::RichText::new(&draft.title).strong());
    ui.hyperlink_to(&draft.url, &draft.url);
    ui.add_space(10.0);
    ui.checkbox(&mut draft.disabled, "暂停这个订阅");
    ui.weak("暂停后不再自动抓取；重新启用时会立即抓取一次。");
    ui.add_space(10.0);
    ui.label("刷新间隔");
    ui.add(
        egui::TextEdit::singleline(&mut draft.interval_draft)
            .hint_text("例如 30m、6h；留空使用全局默认")
            .desired_width(f32::INFINITY),
    );
    ui.weak("支持 s / m / h / d；修改间隔不会立刻抓取。");
    {
        ui.separator();
        ui.label(egui::RichText::new("历史文章").strong());
        let mut history = dependencies.history.snapshot();
        let other_busy =
            history.feed_id != Some(draft.feed_id) && history.status == BackfillStatus::Fetching;
        if history.feed_id != Some(draft.feed_id) {
            history.status = BackfillStatus::Idle;
            history.discovered = 0;
            history.processed = 0;
            history.inserted = 0;
            history.failed = 0;
            history.has_more = false;
            history.last_error = None;
            history.source_description = "自动检测归档页，未发现时使用 RSS/Atom 分页".into();
        }
        ui.weak(&history.source_description);
        if other_busy {
            ui.weak("另一个订阅正在回补，请完成或暂停后再开始。");
        }
        ui.weak(format!(
            "按每批 {} 篇抓取，历史文章默认标记为已读。",
            history.batch_size
        ));
        ui.label(format!(
            "已发现 {} · 已处理 {} · 新增 {} · 失败 {}",
            history.discovered, history.processed, history.inserted, history.failed
        ));
        ui.add_enabled_ui(!other_busy, |ui| {
            ui.horizontal_wrapped(|ui| {
                match history.status {
                    BackfillStatus::Idle | BackfillStatus::Completed | BackfillStatus::Failed => {
                        if history.status == BackfillStatus::Failed
                            && history.has_more
                            && ui.button("重试当前分页").clicked()
                        {
                            history_action = Some(HistoryAction::Next);
                        }
                        if ui.button("开始历史回补").clicked() {
                            history_action = Some(HistoryAction::Start);
                        }
                    }
                    BackfillStatus::WaitingNextBatch => {
                        if history.has_more && ui.button("抓取下一批（50 篇）").clicked() {
                            history_action = Some(HistoryAction::Next);
                        }
                        if ui.button("暂停").clicked() {
                            history_action = Some(HistoryAction::Pause);
                        }
                    }
                    BackfillStatus::Fetching => {
                        ui.spinner();
                        if ui.button("暂停").clicked() {
                            history_action = Some(HistoryAction::Pause);
                        }
                    }
                    BackfillStatus::Paused => {
                        if ui.button("继续").clicked() {
                            history_action = Some(HistoryAction::Resume);
                        }
                    }
                }
                if history.failed > 0 && ui.button("重试失败").clicked() {
                    history_action = Some(HistoryAction::Retry);
                }
            })
        });
        if history.status == BackfillStatus::Completed {
            ui.weak("已读完发现的页面；不代表网站全部历史文章。");
        }
        if let Some(error) = &history.last_error {
            ui.colored_label(egui::Color32::RED, error);
        }
    }
    if let Some(error) = &draft.error {
        ui.add_space(8.0);
        ui.colored_label(egui::Color32::RED, error);
    }
    ui.add_space(12.0);
    if ui.button("保存设置").clicked() {
        save = true;
    }

    if let Some(intent) = discard_guard_controls(ui, show_discard) {
        return Outcome {
            interaction: intent,
            ..Outcome::idle(ModalHostAction::None)
        };
    }
    if let Some(action) = history_action {
        let result = match action {
            HistoryAction::Start => dependencies
                .history
                .start_feed(draft.feed_id, draft.url.clone()),
            HistoryAction::Next => dependencies.history.next_batch(),
            HistoryAction::Pause => dependencies.history.pause(),
            HistoryAction::Resume => dependencies.history.resume(),
            HistoryAction::Retry => dependencies.history.retry_failed(),
        };
        return Outcome {
            notice: Some(match result {
                Ok(()) => "历史回补任务已更新".into(),
                Err(error) => format!("无法更新历史回补：{error}"),
            }),
            ..Outcome::idle(ModalHostAction::None)
        };
    }
    if close {
        return Outcome {
            interaction: InteractionIntent::ClosePanel,
            ..Outcome::idle(ModalHostAction::None)
        };
    }
    if !save {
        return Outcome::idle(ModalHostAction::None);
    }

    let disabled_changed = draft.disabled != draft.original_disabled;
    let interval_draft = draft.interval_draft.trim().to_owned();
    let interval_changed = interval_draft != draft.original_interval;
    let interval = if interval_changed {
        if interval_draft.is_empty() {
            draft.error = Some("当前版本暂不支持清除单源间隔，请输入新的间隔".into());
            return Outcome::idle(ModalHostAction::None);
        }
        match parse_duration(&interval_draft) {
            Ok(seconds) if seconds > 0 => Some(seconds),
            _ => {
                draft.error = Some("请输入大于 0 的间隔，例如 30m 或 6h".into());
                return Outcome::idle(ModalHostAction::None);
            }
        }
    } else {
        None
    };

    let result = (|| {
        let subscriptions = subscriptions(dependencies);
        if let Some(seconds) = interval {
            subscriptions.apply(SubscriptionChange::SetInterval {
                id: draft.feed_id,
                seconds,
            })?;
        }
        let refresh = if disabled_changed {
            Some(
                subscriptions
                    .apply(if draft.disabled {
                        SubscriptionChange::Disable { id: draft.feed_id }
                    } else {
                        SubscriptionChange::Enable { id: draft.feed_id }
                    })?
                    .refresh,
            )
            .flatten()
        } else {
            None
        };
        Ok::<_, SubscriptionError>(refresh)
    })();
    match result {
        Ok(refresh) => Outcome {
            interaction: InteractionIntent::FinishPanel,
            reload: true,
            notice: Some(settings_notice(refresh)),
            ..Outcome::idle(ModalHostAction::None)
        },
        Err(error) => {
            tracing::warn!(detail = %error.technical_detail, "save subscription settings failed");
            draft.error = Some(error.user_message);
            Outcome::idle(ModalHostAction::None)
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum HistoryAction {
    Start,
    Next,
    Pause,
    Resume,
    Retry,
}

fn subscriptions<'a>(dependencies: &'a Dependencies<'a>) -> FeedSubscriptions<'a> {
    FeedSubscriptions::session(dependencies.database.to_path_buf(), dependencies.refresh)
}

/// Re-enable a disabled feed and request its immediate refresh through the
/// shared Feed Subscription Lifecycle. Keeping this action in the adapter
/// prevents the GUI root from reaching into persistence or refresh details.
pub(super) fn retry_disabled_feed(feed_id: i64, dependencies: &Dependencies<'_>) -> Outcome {
    match subscriptions(dependencies).apply(SubscriptionChange::Enable { id: feed_id }) {
        Ok(result) => Outcome {
            reload: result.disposition != ChangeDisposition::NotFound,
            notice: Some(retry_notice(result.disposition, result.refresh)),
            ..Outcome::idle(ModalHostAction::None)
        },
        Err(error) => {
            tracing::warn!(detail = %error.technical_detail, "retry disabled subscription failed");
            Outcome {
                notice: Some(error.user_message),
                ..Outcome::idle(ModalHostAction::None)
            }
        }
    }
}

pub(super) fn disabled_feed_status(disabled: bool, fail_count: i64) -> String {
    if !disabled {
        String::new()
    } else if fail_count > 0 {
        format!("已暂停（连续失败 {fail_count} 次）")
    } else {
        "已暂停".into()
    }
}

fn show_add_modal(
    context: &egui::Context,
    draft: &mut AddDraft,
    show_discard: bool,
    dependencies: &Dependencies<'_>,
) -> Outcome {
    let response = gui_modal::show(context, ModalKind::AddFeed, show_discard, |ui, focus| {
        let mut submit = false;
        let mut cancel = false;
        ui.label("粘贴 RSS、Atom 或博客订阅地址");
        let input = ui.add(
            egui::TextEdit::singleline(&mut draft.url)
                .hint_text("https://example.com/feed.xml")
                .desired_width(f32::INFINITY),
        );
        if focus == InitialFocus::PrimaryField && draft.focus_input {
            input.request_focus();
            draft.focus_input = false;
        }
        if let Some(error) = &draft.error {
            ui.colored_label(egui::Color32::RED, error);
        }
        ui.horizontal(|ui| {
            if ui.button("添加并立即抓取").clicked()
                || (input.lost_focus() && ui.input(|input| input.key_pressed(egui::Key::Enter)))
            {
                submit = true;
            }
            if ui.button("取消").clicked() {
                cancel = true;
            }
        });
        (submit.then(|| draft.url.trim().to_owned()), cancel)
    });
    let mut outcome = Outcome::idle(response.action);
    let Some((add_url, cancel)) = response.inner else {
        return outcome;
    };
    if cancel {
        // Request a close through the modal host so dirty drafts still go
        // through the discard confirmation guard, matching the pre-adapter
        // `close_modal` behavior.
        outcome.modal_action = ModalHostAction::RequestClose;
        return outcome;
    }
    let Some(url) = add_url else { return outcome };
    match subscriptions(dependencies).apply(SubscriptionChange::Add { url }) {
        Ok(result) => {
            let id = result
                .subscription
                .as_ref()
                .expect("add returns a durable subscription")
                .id;
            outcome.interaction = InteractionIntent::CompleteModal;
            outcome.reload = true;
            outcome.select_feed_id = Some(id);
            outcome.notice = Some(add_notice(result.refresh.as_ref()));
        }
        Err(error) => {
            tracing::warn!(detail = %error.technical_detail, "add subscription failed");
            draft.error = Some(error.user_message);
        }
    }
    outcome
}

fn show_delete_modal(
    context: &egui::Context,
    draft: &DeleteDraft,
    dependencies: &Dependencies<'_>,
) -> Outcome {
    let response = gui_modal::show(context, ModalKind::DeleteFeed, false, |ui, _| {
        let mut delete = false;
        let mut cancel = false;
        ui.label(format!("确定删除订阅“{}”吗？", draft.title));
        ui.weak("该订阅下的本地文章也会被删除，此操作无法撤销。");
        ui.horizontal(|ui| {
            if ui.button("删除订阅").clicked() {
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
    outcome.interaction = InteractionIntent::CompleteModal;
    match subscriptions(dependencies).apply(SubscriptionChange::Delete {
        target: draft.id.to_string(),
    }) {
        Ok(result) if result.disposition == ChangeDisposition::Deleted => {
            outcome.reload = true;
            outcome.notice = Some("订阅已删除".into());
        }
        Ok(_) => outcome.notice = Some("没有找到该订阅".into()),
        Err(error) => {
            tracing::warn!(detail = %error.technical_detail, "delete subscription failed");
            outcome.notice = Some(error.user_message);
        }
    }
    outcome
}

fn add_notice(refresh: Option<&InitialRefreshOutcome>) -> String {
    match refresh {
        Some(InitialRefreshOutcome::Deferred) => "订阅已添加，将在资料维护结束后刷新".into(),
        Some(InitialRefreshOutcome::Queued) => "订阅已添加，正在抓取文章".into(),
        _ => "订阅已添加".into(),
    }
}

fn settings_notice(refresh: Option<InitialRefreshOutcome>) -> String {
    match refresh {
        Some(InitialRefreshOutcome::Queued) => "订阅设置已保存，正在刷新".into(),
        Some(InitialRefreshOutcome::Deferred) => "订阅设置已保存，将在资料维护结束后刷新".into(),
        _ => "订阅设置已保存".into(),
    }
}

fn retry_notice(disposition: ChangeDisposition, refresh: Option<InitialRefreshOutcome>) -> String {
    if disposition == ChangeDisposition::NotFound {
        return "没有找到该订阅".into();
    }
    match refresh {
        Some(InitialRefreshOutcome::Queued) => "订阅已重新启用，正在刷新".into(),
        Some(InitialRefreshOutcome::Deferred) => "订阅已重新启用，将在资料维护结束后刷新".into(),
        Some(InitialRefreshOutcome::Succeeded { .. }) => "订阅已重新启用并刷新成功".into(),
        Some(InitialRefreshOutcome::Degraded { .. }) => {
            "订阅已重新启用，但刷新失败，请查看错误详情".into()
        }
        None => "订阅已重新启用".into(),
    }
}

fn discard_guard_controls(ui: &mut egui::Ui, visible: bool) -> Option<InteractionIntent> {
    if !visible {
        return None;
    }
    let mut intent = None;
    ui.separator();
    ui.colored_label(egui::Color32::from_rgb(190, 86, 86), "有尚未保存的修改");
    ui.weak("继续刚才的操作会丢弃这些修改。");
    ui.horizontal(|ui| {
        if ui.button("继续编辑").clicked() {
            intent = Some(InteractionIntent::KeepEditing);
        }
        if ui.button("放弃修改").clicked() {
            intent = Some(InteractionIntent::ConfirmDiscard);
        }
    });
    intent
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_draft_dirty_state_only_tracks_user_input() {
        let mut draft = AddDraft::default();
        assert!(!draft.is_dirty());
        draft.url = "  https://example.com/feed.xml  ".into();
        assert!(draft.is_dirty());
    }

    #[test]
    fn settings_draft_dirty_state_tracks_only_editable_values() {
        let feed = Feed {
            id: 7,
            url: "https://example.com/feed.xml".into(),
            title: Some("Example".into()),
            interval_secs: Some(1800),
            last_fetch: None,
            next_fetch: 0,
            last_error: None,
            fail_count: 0,
            disabled: false,
        };
        let mut draft = SettingsDraft::from_feed(&feed);
        assert_eq!(draft.feed_id(), 7);
        assert!(!draft.is_dirty());
        draft.disabled = true;
        assert!(draft.is_dirty());
    }

    #[test]
    fn settings_interval_uses_the_shared_duration_parser() {
        assert_eq!(parse_duration("30m").unwrap(), 1800);
        assert_eq!(parse_duration("0s").unwrap(), 0);
    }

    #[test]
    fn disabled_feed_status_explains_failure_state_and_manual_pause() {
        assert_eq!(disabled_feed_status(true, 10), "已暂停（连续失败 10 次）");
        assert_eq!(disabled_feed_status(true, 0), "已暂停");
        assert!(disabled_feed_status(false, 10).is_empty());
    }

    #[test]
    fn retry_notice_explains_refresh_lifecycle() {
        assert_eq!(
            retry_notice(
                ChangeDisposition::Changed,
                Some(InitialRefreshOutcome::Queued)
            ),
            "订阅已重新启用，正在刷新"
        );
        assert_eq!(
            retry_notice(
                ChangeDisposition::Changed,
                Some(InitialRefreshOutcome::Deferred)
            ),
            "订阅已重新启用，将在资料维护结束后刷新"
        );
        assert_eq!(
            retry_notice(
                ChangeDisposition::Changed,
                Some(InitialRefreshOutcome::Degraded {
                    technical_detail: Some("timeout".into()),
                })
            ),
            "订阅已重新启用，但刷新失败，请查看错误详情"
        );
    }

    #[test]
    fn gui_root_delegates_feed_lifecycle_changes_to_this_adapter() {
        let gui_root = include_str!("../gui.rs");
        for delegated_change in [
            "SubscriptionChange::Add",
            "SubscriptionChange::Delete",
            "SubscriptionChange::Enable",
            "SubscriptionChange::Disable",
            "SubscriptionChange::SetInterval",
        ] {
            assert!(
                !gui_root.contains(delegated_change),
                "GUI root still owns delegated subscription change: {delegated_change}"
            );
        }
    }
}
