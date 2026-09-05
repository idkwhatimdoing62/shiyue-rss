//! Private desktop adapter for Resource maintenance interactions.
//!
//! Drafts remain owned by the single Desktop Interaction state in `gui.rs`.
//! This module renders and validates those drafts, invokes Resource Library
//! Lifecycle, and returns typed outcomes for the GUI root to adopt.

use std::collections::HashSet;

use eframe::egui;

use crate::db::Db;
use crate::gui_modal::{self, InitialFocus, ModalHostAction};
use crate::gui_state::ModalKind;
use crate::knowledge_workflow::{
    TaskSnapshot, TaskStage as KnowledgeTaskStage, TaskStatus as KnowledgeTaskStatus,
};
use crate::resource_library_lifecycle::{
    Clock, CompleteManualEdit, CreateResource, FailureKind, HandoffDisposition, ImportCandidate,
    LifecycleFailure, ProcessingHandoff, ProjectionScope, Resource, ResourceCollection,
    ResourceCurationState, ResourceDetail, ResourceKind, ResourceLibraryLifecycle,
    ResourceLibraryProjection, ResourceLifecycleChange, ResourcePrivacy, ResourceSource,
};

pub(super) struct Dependencies<'a> {
    pub(super) db: &'a Db,
    pub(super) processing_handoff: &'a dyn ProcessingHandoff,
    pub(super) clock: &'a dyn Clock,
}

#[derive(Debug)]
pub(super) struct AddDraft {
    url: String,
    note: String,
    private: bool,
    error: Option<String>,
    focus_input: bool,
}

impl Default for AddDraft {
    fn default() -> Self {
        Self {
            url: String::new(),
            note: String::new(),
            private: false,
            error: None,
            focus_input: true,
        }
    }
}

impl AddDraft {
    pub(super) fn is_dirty(&self) -> bool {
        !self.url.trim().is_empty() || !self.note.trim().is_empty() || self.private
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct EditorDraft {
    id: i64,
    hydrated: bool,
    title: String,
    purpose_zh: String,
    note: String,
    private: bool,
    rating: i64,
    original: EditorValues,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct EditorValues {
    title: String,
    purpose_zh: String,
    note: String,
    private: bool,
    rating: i64,
}

impl EditorValues {
    fn from_resource(resource: &Resource) -> Self {
        Self {
            title: resource.title.clone().unwrap_or_default(),
            purpose_zh: resource.purpose_zh.clone().unwrap_or_default(),
            note: resource.private_note.clone().unwrap_or_default(),
            private: resource.privacy == ResourcePrivacy::Private,
            rating: resource.manual_rating.unwrap_or(0),
        }
    }
}

impl EditorDraft {
    pub(super) fn open(resource_id: i64, resource: Option<&Resource>) -> Self {
        let values = resource
            .map(EditorValues::from_resource)
            .unwrap_or_default();
        Self {
            id: resource_id,
            hydrated: resource.is_some(),
            title: values.title.clone(),
            purpose_zh: values.purpose_zh.clone(),
            note: values.note.clone(),
            private: values.private,
            rating: values.rating,
            original: values,
        }
    }

    pub(super) fn id(&self) -> i64 {
        self.id
    }

    pub(super) fn is_dirty(&self) -> bool {
        self.title != self.original.title
            || self.purpose_zh != self.original.purpose_zh
            || self.note != self.original.note
            || self.private != self.original.private
            || self.rating != self.original.rating
    }

    fn reconcile(&mut self, resource: &Resource) {
        if self.hydrated {
            return;
        }
        let authoritative = EditorValues::from_resource(resource);
        if self.title == self.original.title {
            self.title.clone_from(&authoritative.title);
        }
        if self.purpose_zh == self.original.purpose_zh {
            self.purpose_zh.clone_from(&authoritative.purpose_zh);
        }
        if self.note == self.original.note {
            self.note.clone_from(&authoritative.note);
        }
        if self.private == self.original.private {
            self.private = authoritative.private;
        }
        if self.rating == self.original.rating {
            self.rating = authoritative.rating;
        }
        self.original = authoritative;
        self.hydrated = true;
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

#[derive(Debug)]
pub(super) struct ImportDraft {
    candidates: Vec<ImportCandidate>,
    selected: HashSet<i64>,
    initial_selected: HashSet<i64>,
}

impl ImportDraft {
    pub(super) fn prepare(dependencies: &Dependencies<'_>) -> Result<Self, LifecycleFailure> {
        let candidates = lifecycle(dependencies).preview_web_clipping_import()?;
        let selected = candidates
            .iter()
            .filter(|item| !item.already_imported)
            .map(|item| item.article_id)
            .collect::<HashSet<_>>();
        Ok(Self {
            initial_selected: selected.clone(),
            selected,
            candidates,
        })
    }

    pub(super) fn is_dirty(&self) -> bool {
        self.selected != self.initial_selected
    }
}

pub(super) enum ModalDraft<'a> {
    Add(&'a mut AddDraft),
    Delete(&'a mut DeleteDraft),
    Import(&'a mut ImportDraft),
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
    pub(super) projection: Option<ResourceLibraryProjection>,
    pub(super) notice: Option<String>,
    pub(super) retry_resource_id: Option<i64>,
}

impl Outcome {
    fn idle(modal_action: ModalHostAction) -> Self {
        Self {
            modal_action,
            interaction: InteractionIntent::None,
            projection: None,
            notice: None,
            retry_resource_id: None,
        }
    }

    fn lifecycle_success(
        interaction: InteractionIntent,
        projection: ResourceLibraryProjection,
        notice: impl Into<String>,
    ) -> Self {
        Self {
            modal_action: ModalHostAction::None,
            interaction,
            projection: Some(projection),
            notice: Some(notice.into()),
            retry_resource_id: None,
        }
    }

    fn notice(message: impl Into<String>) -> Self {
        Self {
            modal_action: ModalHostAction::None,
            interaction: InteractionIntent::None,
            projection: None,
            notice: Some(message.into()),
            retry_resource_id: None,
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
        ModalDraft::Import(draft) => show_import_modal(context, draft, show_discard, dependencies),
    }
}

pub(super) fn show_panel(
    ui: &mut egui::Ui,
    draft: &mut EditorDraft,
    detail: Option<&ResourceDetail>,
    task: Option<&TaskSnapshot>,
    show_discard: bool,
    dependencies: &Dependencies<'_>,
) -> Outcome {
    if let Some(detail) = detail {
        draft.reconcile(&detail.resource);
    }
    let resource_id = draft.id;
    let mut save = false;
    let mut close = false;
    let mut enrich = false;
    ui.horizontal(|ui| {
        ui.heading("编辑资源");
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.button("关闭").clicked() {
                close = true;
            }
        });
    });
    ui.separator();
    render_processing_state(ui, task, &mut enrich);
    if let Some(detail) = detail {
        render_enrichment(ui, detail, &mut enrich);
    }
    ui.label(egui::RichText::new("可手动修改").strong());
    ui.label("标题");
    ui.add(egui::TextEdit::singleline(&mut draft.title).desired_width(f32::INFINITY));
    ui.label("用途");
    ui.add(
        egui::TextEdit::multiline(&mut draft.purpose_zh)
            .desired_rows(6)
            .desired_width(f32::INFINITY),
    );
    ui.label("私人备注");
    ui.add(
        egui::TextEdit::multiline(&mut draft.note)
            .desired_rows(6)
            .desired_width(f32::INFINITY),
    );
    ui.checkbox(&mut draft.private, "私密资源");
    ui.horizontal(|ui| {
        ui.label("评分");
        ui.add(
            egui::Slider::new(&mut draft.rating, 0..=5).custom_formatter(|value, _| {
                if value == 0.0 {
                    "未设置".into()
                } else {
                    format!("{value:.0}/5")
                }
            }),
        );
    });
    ui.add_space(8.0);
    if ui
        .add_enabled(detail.is_some(), egui::Button::new("保存修改"))
        .clicked()
    {
        save = true;
    }

    if let Some(intent) = discard_guard_controls(ui, show_discard) {
        return Outcome {
            interaction: intent,
            ..Outcome::idle(ModalHostAction::None)
        };
    }
    if close {
        return Outcome {
            interaction: InteractionIntent::ClosePanel,
            ..Outcome::idle(ModalHostAction::None)
        };
    }

    let mut outcome = if save {
        save_editor(draft, detail, dependencies)
    } else {
        Outcome::idle(ModalHostAction::None)
    };
    if enrich {
        outcome.retry_resource_id = Some(resource_id);
    }
    outcome
}

pub(super) fn transition_curation(
    resource_id: i64,
    target: ResourceCurationState,
    collection: ResourceCollection,
    dependencies: &Dependencies<'_>,
) -> Outcome {
    match lifecycle(dependencies).apply(
        ResourceLifecycleChange::SetCurationState {
            resource_id,
            target,
        },
        ProjectionScope::collection(collection),
    ) {
        Ok(result) => Outcome::lifecycle_success(
            InteractionIntent::None,
            result.projection,
            resource_saved_notice("资源状态已更新", &result.handoffs),
        ),
        Err(error) => Outcome::notice(format!("操作失败：{error}")),
    }
}

fn lifecycle<'a>(dependencies: &'a Dependencies<'a>) -> ResourceLibraryLifecycle<'a, 'a> {
    ResourceLibraryLifecycle::new(
        dependencies.db,
        dependencies.processing_handoff,
        dependencies.clock,
    )
}

fn show_add_modal(
    context: &egui::Context,
    draft: &mut AddDraft,
    show_discard: bool,
    dependencies: &Dependencies<'_>,
) -> Outcome {
    let response = gui_modal::show(
        context,
        ModalKind::AddResource,
        show_discard,
        |ui, focus| {
            ui.label("网址（唯一必填项）");
            let input = ui.add(
                egui::TextEdit::singleline(&mut draft.url)
                    .desired_width(f32::INFINITY)
                    .hint_text("https://koboyo.com/icons?q=app+icon"),
            );
            if focus == InitialFocus::PrimaryField && draft.focus_input {
                input.request_focus();
                draft.focus_input = false;
            }
            ui.label("私人备注（可选）");
            ui.add(
                egui::TextEdit::multiline(&mut draft.note)
                    .desired_rows(3)
                    .desired_width(f32::INFINITY),
            );
            ui.checkbox(&mut draft.private, "私密资源（永不发送到云端 AI）");
            if let Some(error) = &draft.error {
                ui.colored_label(egui::Color32::RED, error);
            }
            ui.button("立即保存").clicked()
        },
    );
    let mut outcome = Outcome::idle(response.action);
    if response.inner != Some(true) {
        return outcome;
    }
    match create_resource_with_handoff(draft, dependencies) {
        Ok((projection, handoffs)) => {
            outcome.interaction = InteractionIntent::CompleteModal;
            outcome.projection = Some(projection);
            outcome.notice = Some(resource_saved_notice(
                "资源网址已保存；断网也不会丢失",
                &handoffs,
            ));
        }
        Err(error) => draft.error = Some(error.to_string()),
    }
    outcome
}

#[cfg(test)]
fn create_resource(
    draft: &AddDraft,
    dependencies: &Dependencies<'_>,
) -> Result<ResourceLibraryProjection, LifecycleFailure> {
    Ok(create_resource_with_handoff(draft, dependencies)?.0)
}

fn create_resource_with_handoff(
    draft: &AddDraft,
    dependencies: &Dependencies<'_>,
) -> Result<
    (
        ResourceLibraryProjection,
        Vec<crate::resource_library_lifecycle::ResourceHandoff>,
    ),
    LifecycleFailure,
> {
    let outcome = lifecycle(dependencies).apply(
        ResourceLifecycleChange::Create(CreateResource {
            url: draft.url.clone(),
            parent_resource_id: None,
            linked_article_id: None,
            kind: ResourceKind::Page,
            title: None,
            private_note: non_empty_owned(&draft.note),
            privacy: if draft.private {
                ResourcePrivacy::Private
            } else {
                ResourcePrivacy::Public
            },
            source: ResourceSource::Gui,
            manual_rating: None,
        }),
        ProjectionScope::collection(ResourceCollection::Active),
    )?;
    Ok((outcome.projection, outcome.handoffs))
}

fn show_delete_modal(
    context: &egui::Context,
    draft: &DeleteDraft,
    dependencies: &Dependencies<'_>,
) -> Outcome {
    let response = gui_modal::show(context, ModalKind::DeleteResource, false, |ui, _| {
        ui.label(format!("确定永久删除「{}」吗？", draft.title));
        ui.weak("资源快照、分类、标签和整理记录会一起删除；关联的博客文章不会删除。");
        let mut confirm = false;
        let mut cancel = false;
        ui.horizontal(|ui| {
            if ui.button("确认永久删除").clicked() {
                confirm = true;
            }
            if ui.button("取消").clicked() {
                cancel = true;
            }
        });
        (confirm, cancel)
    });
    let mut outcome = Outcome::idle(response.action);
    let Some((confirm, cancel)) = response.inner else {
        return outcome;
    };
    if cancel {
        outcome.interaction = InteractionIntent::CompleteModal;
    } else if confirm {
        outcome.interaction = InteractionIntent::CompleteModal;
        match delete_resource(draft.id, dependencies) {
            Ok(projection) => {
                outcome.projection = Some(projection);
                outcome.notice = Some("资源已永久删除".into());
            }
            Err(error) => outcome.notice = Some(format!("删除失败：{error}")),
        }
    }
    outcome
}

fn delete_resource(
    resource_id: i64,
    dependencies: &Dependencies<'_>,
) -> Result<ResourceLibraryProjection, LifecycleFailure> {
    Ok(lifecycle(dependencies)
        .apply(
            ResourceLifecycleChange::Delete { resource_id },
            ProjectionScope::collection(ResourceCollection::Archived),
        )?
        .projection)
}

fn show_import_modal(
    context: &egui::Context,
    draft: &mut ImportDraft,
    show_discard: bool,
    dependencies: &Dependencies<'_>,
) -> Outcome {
    let response = gui_modal::show(
        context,
        ModalKind::ImportResources,
        show_discard,
        |ui, _| {
            ui.label(
                "只创建 Resource 与原 Article 的关联，不复制正文，也不改变原文章、标签或收藏状态。",
            );
            ui.separator();
            egui::ScrollArea::vertical().show(ui, |ui| {
                for candidate in &draft.candidates {
                    let mut checked = draft.selected.contains(&candidate.article_id);
                    ui.horizontal(|ui| {
                        let response = ui.add_enabled(
                            !candidate.already_imported,
                            egui::Checkbox::new(&mut checked, ""),
                        );
                        if response.changed() {
                            if checked {
                                draft.selected.insert(candidate.article_id);
                            } else {
                                draft.selected.remove(&candidate.article_id);
                            }
                        }
                        ui.vertical(|ui| {
                            ui.label(candidate.title.as_deref().unwrap_or(&candidate.url));
                            ui.weak(if candidate.already_imported {
                                format!("{} · 已导入", candidate.url)
                            } else {
                                candidate.url.clone()
                            });
                        });
                    });
                    ui.separator();
                }
            });
            ui.add_enabled(
                !draft.selected.is_empty(),
                egui::Button::new(format!("导入选中的 {} 项", draft.selected.len())),
            )
            .clicked()
        },
    );
    let mut outcome = Outcome::idle(response.action);
    if response.inner != Some(true) {
        return outcome;
    }
    match import_resources_with_handoff(draft, dependencies) {
        Ok((projection, count, handoffs)) => {
            outcome.interaction = InteractionIntent::CompleteModal;
            outcome.projection = Some(projection);
            outcome.notice = Some(resource_saved_notice(
                &format!("已导入 {count} 个资源，正在后台补全描述"),
                &handoffs,
            ));
        }
        Err(error) => outcome.notice = Some(format!("导入失败：{error}")),
    }
    outcome
}

#[cfg(test)]
fn import_resources(
    draft: &ImportDraft,
    dependencies: &Dependencies<'_>,
) -> Result<(ResourceLibraryProjection, usize), LifecycleFailure> {
    let (projection, count, _) = import_resources_with_handoff(draft, dependencies)?;
    Ok((projection, count))
}

fn import_resources_with_handoff(
    draft: &ImportDraft,
    dependencies: &Dependencies<'_>,
) -> Result<
    (
        ResourceLibraryProjection,
        usize,
        Vec<crate::resource_library_lifecycle::ResourceHandoff>,
    ),
    LifecycleFailure,
> {
    let outcome = lifecycle(dependencies).apply(
        ResourceLifecycleChange::ImportWebClippings {
            article_ids: draft.selected.iter().copied().collect(),
        },
        ProjectionScope::collection(ResourceCollection::Active),
    )?;
    let count = outcome.affected_resource_ids.len();
    Ok((outcome.projection, count, outcome.handoffs))
}

fn resource_saved_notice(
    base: &str,
    handoffs: &[crate::resource_library_lifecycle::ResourceHandoff],
) -> String {
    let deferred = handoffs
        .iter()
        .find_map(|handoff| match &handoff.disposition {
            HandoffDisposition::Deferred {
                user_message,
                technical_detail,
            } => Some(format!(
                "{user_message}（技术详情：{}）",
                technical_detail.chars().take(240).collect::<String>()
            )),
            _ => None,
        });
    deferred.map_or_else(|| base.to_owned(), |message| format!("{base}；{message}"))
}

fn save_editor(
    draft: &EditorDraft,
    detail: Option<&ResourceDetail>,
    dependencies: &Dependencies<'_>,
) -> Outcome {
    let result = detail
        .ok_or_else(|| LifecycleFailure {
            kind: FailureKind::Storage,
            user_message: "资源详情读取失败".into(),
            technical_detail: format!("RESOURCE_DETAIL_MISSING: {}", draft.id),
        })
        .and_then(|detail| {
            lifecycle(dependencies)
                .apply(
                    ResourceLifecycleChange::CompleteManualEdit(CompleteManualEdit {
                        resource_id: draft.id,
                        title: non_empty_owned(&draft.title),
                        purpose_zh: non_empty_owned(&draft.purpose_zh),
                        use_when_zh: detail.resource.use_when_zh.clone(),
                        private_note: non_empty_owned(&draft.note),
                        privacy: if draft.private {
                            ResourcePrivacy::Private
                        } else {
                            ResourcePrivacy::Public
                        },
                        manual_rating: (draft.rating > 0).then_some(draft.rating),
                        categories: detail.categories.clone(),
                        tags: detail.tags.clone(),
                    }),
                    ProjectionScope::Resource(draft.id),
                )
                .map(|outcome| outcome.projection)
        });
    match result {
        Ok(projection) => {
            Outcome::lifecycle_success(InteractionIntent::FinishPanel, projection, "资源已更新")
        }
        Err(error) => Outcome::notice(format!("保存失败：{error}")),
    }
}

fn render_processing_state(ui: &mut egui::Ui, task: Option<&TaskSnapshot>, enrich: &mut bool) {
    let Some(task) = task else { return };
    let stage = match task.current_stage {
        Some(KnowledgeTaskStage::Fetching) => "正在抓取网页",
        Some(KnowledgeTaskStage::Organizing) => "正在整理描述",
        Some(KnowledgeTaskStage::Summarizing) => "正在总结文章",
        None => "等待后台处理",
    };
    if matches!(
        task.status,
        KnowledgeTaskStatus::Queued | KnowledgeTaskStatus::Running
    ) {
        ui.label(egui::RichText::new(stage).strong());
        ui.spinner();
    } else if matches!(
        task.status,
        KnowledgeTaskStatus::Failed | KnowledgeTaskStatus::Interrupted
    ) {
        ui.colored_label(egui::Color32::RED, "上次处理失败，可以重试");
        ui.collapsing("技术详情", |ui| {
            ui.monospace(task.technical_detail.as_deref().unwrap_or("未知错误"));
        });
        if ui.button("重试处理").clicked() {
            *enrich = true;
        }
    } else {
        ui.label("AI 信息已更新");
    }
    ui.separator();
}

fn render_enrichment(ui: &mut egui::Ui, detail: &ResourceDetail, enrich: &mut bool) {
    let resource = &detail.resource;
    ui.label(egui::RichText::new("AI 补全信息").strong());
    let has_ai_details = resource.purpose_zh.is_some()
        || resource.use_when_zh.is_some()
        || !resource.capabilities.is_empty()
        || !resource.limitations.is_empty()
        || !detail.categories.is_empty()
        || !detail.tags.is_empty()
        || resource.pricing.is_some()
        || resource.requires_login.is_some()
        || !resource.languages.is_empty();
    if !has_ai_details {
        ui.weak("这条资源还没有成功生成 AI 描述。网页或文章内容已经保存，可以重新补全。");
        if resource.privacy == ResourcePrivacy::Private {
            ui.weak("私密资源不会发送给 AI；取消“私密资源”并保存后才能补全。");
        } else if ui.button("立即补全描述").clicked() {
            *enrich = true;
        }
    }
    if let Some(value) = &resource.purpose_zh {
        ui.label("用途描述");
        ui.label(value);
    }
    if let Some(value) = &resource.use_when_zh {
        ui.label("适合什么时候使用");
        ui.label(value);
    }
    if !resource.capabilities.is_empty() {
        ui.label(format!("主要能力：{}", resource.capabilities.join("、")));
    }
    if !resource.limitations.is_empty() {
        ui.label(format!("限制：{}", resource.limitations.join("、")));
    }
    if !detail.categories.is_empty() {
        let values = detail
            .categories
            .iter()
            .map(|category| match category {
                crate::resource_library_lifecycle::Category::Tool => "工具",
                crate::resource_library_lifecycle::Category::AssetLibrary => "素材库",
                crate::resource_library_lifecycle::Category::Docs => "文档",
                crate::resource_library_lifecycle::Category::Blog => "博客",
                crate::resource_library_lifecycle::Category::Inspiration => "灵感",
                crate::resource_library_lifecycle::Category::Service => "服务",
                crate::resource_library_lifecycle::Category::Repository => "代码仓库",
                crate::resource_library_lifecycle::Category::Other => "其他",
            })
            .collect::<Vec<_>>();
        ui.label(format!("分类：{}", values.join("、")));
    }
    if !detail.tags.is_empty() {
        ui.label(format!(
            "标签：{}",
            detail
                .tags
                .iter()
                .map(|tag| tag.name.as_str())
                .collect::<Vec<_>>()
                .join("、")
        ));
    }
    let pricing = resource.pricing.map(|pricing| match pricing {
        crate::resource_library_lifecycle::Pricing::Free => "免费",
        crate::resource_library_lifecycle::Pricing::Freemium => "部分免费",
        crate::resource_library_lifecycle::Pricing::Paid => "付费",
        crate::resource_library_lifecycle::Pricing::Unknown => "未知",
    });
    let login = resource
        .requires_login
        .map(|value| if value { "需要" } else { "不需要" });
    if pricing.is_some() || login.is_some() || !resource.languages.is_empty() {
        ui.label(format!(
            "价格：{}　登录：{}　语言：{}",
            pricing.unwrap_or("未判断"),
            login.unwrap_or("未判断"),
            if resource.languages.is_empty() {
                "未判断".to_owned()
            } else {
                resource.languages.join("、")
            }
        ));
    }
    ui.separator();
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

fn non_empty_owned(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resource_library_lifecycle::{
        Category, NoProcessingHandoff, ResourceCurationState, ResourceTag, TagLanguage, TagSource,
    };

    #[derive(Debug)]
    struct FixedClock(i64);

    impl Clock for FixedClock {
        fn now(&self) -> i64 {
            self.0
        }
    }

    fn test_db() -> Db {
        let connection = rusqlite::Connection::open_in_memory().unwrap();
        connection
            .pragma_update(None, "foreign_keys", "ON")
            .unwrap();
        crate::schema_evolution::evolve(&connection).unwrap();
        Db {
            conn: connection,
            path: None,
            _maintenance_fence: None,
        }
    }

    fn dependencies<'a>(db: &'a Db, clock: &'a FixedClock) -> Dependencies<'a> {
        Dependencies {
            db,
            processing_handoff: &NoProcessingHandoff,
            clock,
        }
    }

    #[test]
    fn editor_draft_tracks_changes_against_its_authoritative_snapshot() {
        let db = test_db();
        let clock = FixedClock(100);
        let dependencies = dependencies(&db, &clock);
        let mut add = AddDraft {
            url: "https://example.com/tool".into(),
            note: "opening note".into(),
            private: false,
            error: None,
            focus_input: false,
        };
        let projection = create_resource(&add, &dependencies).unwrap();
        let resource = projection.resources.first().unwrap();
        let mut draft = EditorDraft::open(resource.id, Some(resource));

        assert!(!draft.is_dirty());
        draft.note = "edited note".into();
        assert!(draft.is_dirty());

        add.note = "opening note".into();
        assert!(add.is_dirty());
    }

    #[test]
    fn editor_reconciliation_keeps_local_edits_and_hydrates_untouched_fields() {
        let db = test_db();
        let clock = FixedClock(100);
        let dependencies = dependencies(&db, &clock);
        let add = AddDraft {
            url: "https://example.com/reconcile".into(),
            note: "authoritative note".into(),
            private: true,
            error: None,
            focus_input: false,
        };
        let projection = create_resource(&add, &dependencies).unwrap();
        let resource = projection.resources.first().unwrap();
        let mut draft = EditorDraft::open(resource.id, None);
        draft.title = "local title".into();

        draft.reconcile(resource);

        assert_eq!(draft.title, "local title");
        assert_eq!(draft.note, "authoritative note");
        assert!(draft.private);
        assert!(draft.is_dirty());
    }

    #[test]
    fn editor_save_preserves_authoritative_categories_tags_and_use_when() {
        let db = test_db();
        let clock = FixedClock(100);
        let dependencies = dependencies(&db, &clock);
        let add = AddDraft {
            url: "https://example.com/preserved".into(),
            note: String::new(),
            private: false,
            error: None,
            focus_input: false,
        };
        let created = create_resource(&add, &dependencies).unwrap();
        let id = created.resources[0].id;
        let prepared = lifecycle(&dependencies)
            .apply(
                ResourceLifecycleChange::CompleteManualEdit(CompleteManualEdit {
                    resource_id: id,
                    title: Some("Original".into()),
                    purpose_zh: Some("Original purpose".into()),
                    use_when_zh: Some("需要架构资料时".into()),
                    private_note: None,
                    privacy: ResourcePrivacy::Public,
                    manual_rating: Some(3),
                    categories: vec![Category::Docs],
                    tags: vec![ResourceTag {
                        name: "architecture".into(),
                        language: TagLanguage::En,
                        source: TagSource::Manual,
                    }],
                }),
                ProjectionScope::Resource(id),
            )
            .unwrap();
        let detail = prepared.projection.detail.unwrap();
        let mut draft = EditorDraft::open(id, Some(&detail.resource));
        draft.title = "Edited".into();
        draft.purpose_zh = "Edited purpose".into();

        let outcome = save_editor(&draft, Some(&detail), &dependencies);
        let saved = outcome.projection.unwrap().detail.unwrap();

        assert_eq!(outcome.interaction, InteractionIntent::FinishPanel);
        assert_eq!(saved.resource.title.as_deref(), Some("Edited"));
        assert_eq!(
            saved.resource.use_when_zh.as_deref(),
            Some("需要架构资料时")
        );
        assert_eq!(saved.categories, vec![Category::Docs]);
        assert_eq!(saved.tags, detail.tags);
    }

    #[test]
    fn import_and_delete_helpers_return_adoptable_projections() {
        let db = test_db();
        let clock = FixedClock(100);
        let dependencies = dependencies(&db, &clock);
        let article_id = db
            .save_web_clipping(
                Some("https://example.com/clipping"),
                Some("Clipping"),
                "<main>saved</main>",
                50,
            )
            .unwrap();
        let import = ImportDraft::prepare(&dependencies).unwrap();

        assert_eq!(import.selected, HashSet::from([article_id]));
        let (projection, count) = import_resources(&import, &dependencies).unwrap();
        assert_eq!(count, 1);
        let resource_id = projection.resources[0].id;
        let transitioned = transition_curation(
            resource_id,
            ResourceCurationState::Archived,
            ResourceCollection::Archived,
            &dependencies,
        );
        assert!(transitioned.projection.is_some());
        assert_eq!(transitioned.notice.as_deref(), Some("资源状态已更新"));

        let deleted = delete_resource(resource_id, &dependencies).unwrap();
        assert!(deleted.resources.is_empty());
        assert_eq!(deleted.counts.active, 0);
    }

    #[test]
    fn deferred_processing_handoff_is_visible_in_save_notice() {
        let message = resource_saved_notice(
            "资源网址已保存；断网也不会丢失",
            &[crate::resource_library_lifecycle::ResourceHandoff {
                resource_id: 7,
                disposition: HandoffDisposition::Deferred {
                    user_message: "资源已保存，后台整理暂未启动，可稍后重试".into(),
                    technical_detail: "KNOWLEDGE_PROCESSING_NOT_CONNECTED".into(),
                },
            }],
        );
        assert!(message.contains("后台整理暂未启动"));
        assert!(message.contains("KNOWLEDGE_PROCESSING_NOT_CONNECTED"));
    }

    #[test]
    fn gui_root_delegates_resource_maintenance_changes_to_this_adapter() {
        let gui_root = include_str!("../gui.rs");
        for delegated_change in [
            "ResourceLifecycleChange::Create(",
            "ResourceLifecycleChange::CompleteManualEdit",
            "ResourceLifecycleChange::ImportWebClippings",
            "ResourceLifecycleChange::Delete {",
            "ResourceLifecycleChange::SetCurationState",
        ] {
            assert!(
                !gui_root.contains(delegated_change),
                "GUI root still owns delegated resource change: {delegated_change}"
            );
        }
    }
}
