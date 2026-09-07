//! 桌面阅读器（egui/eframe，ADR-13）+ 内置抓取调度（ADR-14）+ 关窗到托盘（ADR-15）。
//! 三栏：源 | 文章 | 正文。正文按原文顺序穿插 文字/图片（ADR-16），图片原生纹理渲染。
//!
//! 进程模型：UI 在主线程；RSS Refresh 与 Knowledge Processing 各自通过窄 facade
//! 表达意图和 snapshot。后台模块只在短事务期间打开数据库，并通过 notice 请求 repaint。

mod excerpt_thought_feature;
mod feed_subscription_feature;
mod knowledge_feature;
mod library_search_feature;
mod resource_feature;
mod web_clipping_feature;

use anyhow::Result;
use eframe::egui;
use std::collections::{HashMap, HashSet};
use std::ops::{Deref, DerefMut, Range};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::article_document_presentation::{
    ArticleDocumentPresenter, ArticleDocumentSource, PresentOutcome, PresentRequest,
    PresentationIntent, RestoreSelection, SelectedQuote, article_visible_text,
};
use crate::article_library_lifecycle::{
    ArticleBatchAction, ArticleLibraryLifecycle, ArticleLibraryProjection, ArticleLifecycleChange,
    ChangeDisposition as ArticleChangeDisposition, LifecycleFailure, ProjectionScope,
};
use crate::backup::{BackupEntry, BackupProtection, BackupStore, DEFAULT_BACKUP_KEEP};
use crate::config::{Config, NetworkMode};
use crate::db::Db;
use crate::desktop_library_projection::{
    DesktopLibraryProjection, DesktopProjectionDemand, DesktopProjectionFact,
    DesktopProjectionFrame, ProjectionFreshness, ResourceProjectionDemand,
};
use crate::desktop_runtime::{DesktopIntent, DesktopSession, Paths, SettingsChange};
use crate::excerpt_thought_lifecycle::{
    ArticleOrigin as ExcerptArticleOrigin, ExcerptCapture, ExcerptIdentityKind,
    ExcerptThoughtProjection, ExcerptView, ProjectionScope as ExcerptProjectionScope,
};
use crate::feed_subscription::FeedSubscriptions;
use crate::gui_icons::{NavigationButton, RemixIcon};
use crate::gui_modal::{self, InitialFocus, ModalHostAction};
use crate::gui_state::{
    ArticleCollection, DiscardOwner, ModalKind, ModalPayload, PanelPayload, Route, UiAction,
    UiEffect, UiState,
};
use crate::gui_theme::ReaderTheme;
use crate::image_store::{CacheStats, DEFAULT_LIMIT_BYTES, ImageStore};
use crate::knowledge_workflow::{
    ConnectionState, KnowledgeEngine, TaskKey, TaskKind as KnowledgeTaskKind, TaskSnapshot,
    TaskStatus as KnowledgeTaskStatus,
};
use crate::library_search::{LibrarySearchResult, PrimaryIdentity};
use crate::local_data_maintenance::{
    MaintenanceEngine, MaintenanceNotice, MaintenanceParticipant, MaintenanceRequest,
    MaintenanceSnapshot, MaintenanceStage, MaintenanceStatus,
};
use crate::model::{Article, ArticleSelection, Feed, TextAnchor};
use crate::rss_refresh_workflow::{
    RefreshNotice, RefreshRunStatus, RefreshWorkflowStatus, RssRefreshWorkflow, RunId,
    format_refresh_error_for_display,
};
use crate::web_clipping_lifecycle::{
    CaptureFailureKind, CaptureId, CaptureSnapshot, CaptureState, WebClippingLifecycle,
};

const FEED_PANEL_WIDTH: f32 = 240.0;
const ARTICLE_PANEL_WIDTH: f32 = 340.0;
const ARTICLE_MAX_WIDTH: f32 = 820.0;
const RESOURCE_CARD_HEIGHT: f32 = 216.0;
const ARTICLE_ROW_HEIGHT: f32 = 68.0;
const FEED_ROW_HEIGHT: f32 = 38.0;

// ---------- App ----------

pub(crate) struct GuiApp {
    db_path: PathBuf,
    rss_refresh: RssRefreshWorkflow,
    rss_last_terminal_notice: Option<RunId>,
    feeds: Vec<Feed>,
    batch_mode: bool,
    batch_selection: HashSet<i64>,
    // 选中态存 id 而非下标，后台刷新重排后也不跳（ADR-14）。
    sel_article_id: Option<i64>,
    pending_article_focus: Option<i64>,
    pending_feed_focus: Option<i64>,
    last_opened_article_id: Option<i64>,
    article_route_memory: HashMap<ArticleCollection, ArticleRouteMemory>,
    current_body_scroll: f32,
    body_article_id: Option<i64>,
    reading_positions: HashMap<i64, f32>,
    delayed_read_marking: DelayedReadMarking,
    article_document: ArticleDocumentPresenter,
    image_store: Arc<ImageStore>,
    backup_store: BackupStore,
    maintenance_engine: MaintenanceEngine,
    maintenance_snapshot: Option<MaintenanceSnapshot>,
    data_dir: PathBuf,
    log_file: PathBuf,
    ui_state: InteractionState,
    route_storage_key: Option<String>,
    storage_overview: Option<StorageOverview>,
    storage_message: Option<String>,
    pending_selection_anchor: Option<ArticleSelection>,
    pending_excerpt_selection_id: Option<(i64, i64)>,
    pending_body_scroll: Option<(i64, f32)>,
    /// 快捷操作浮层中当前等待处理的选区。
    selection_popup_geometry: Option<SelectionPopupGeometry>,
    /// 每次新选区使用不同的浮层 id，避免旧浮层的点击关闭事件误伤新浮层。
    selection_popup_generation: u64,
    /// 跨标题、正文、列表和图片的文章级拖选状态。
    web_clipping_lifecycle: WebClippingLifecycle,
    consumed_web_clipping_terminal: Option<(CaptureId, u64)>,
    search_feature: library_search_feature::SearchFeature,
    resource_filter: ResourceFilter,
    desktop_projection: DesktopLibraryProjection,
    desktop_projection_frame: DesktopProjectionFrame,
    knowledge_engine: KnowledgeEngine,
    knowledge_feature: knowledge_feature::KnowledgeFeature,
    /// Declared after every background workflow so their Drop implementations
    /// stop accepting work before the long-lived UI database handle closes.
    db: DbSlot,
    /// Declared last so native desktop resources are released after the
    /// workflow engines and their database participants.
    desktop: DesktopSession,
}

struct DbSlot(Option<Db>);

impl DbSlot {
    fn close(&mut self) {
        self.0.take();
    }

    fn reopen(&mut self, path: &std::path::Path) -> Result<()> {
        self.0 = Some(Db::open(path)?);
        Ok(())
    }

    fn is_open(&self) -> bool {
        self.0.is_some()
    }
}

impl Deref for DbSlot {
    type Target = Db;

    fn deref(&self) -> &Self::Target {
        self.0.as_ref().expect("database is closed for maintenance")
    }
}

impl DerefMut for DbSlot {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.0.as_mut().expect("database is closed for maintenance")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResourceFilter {
    Active,
    PendingReview,
    Broken,
    Archived,
}

fn projection_scope_for_route(route: Route) -> Option<ProjectionScope> {
    match route {
        Route::Dashboard => None,
        Route::Articles(ArticleCollection::Saved) => Some(ProjectionScope::ArticleBookmarks),
        Route::Articles(ArticleCollection::ReadLater) => Some(ProjectionScope::ReadLater),
        Route::Articles(ArticleCollection::SearchResult(id)) => Some(ProjectionScope::Article(id)),
        Route::Articles(ArticleCollection::Feed(Some(id))) => Some(ProjectionScope::Feed(id)),
        Route::Archive => Some(ProjectionScope::Archive),
        Route::Articles(ArticleCollection::Feed(None)) => Some(ProjectionScope::All),
        Route::Resources | Route::Excerpts | Route::Storage => None,
    }
}

fn reconcile_article_selection(
    selected: Option<i64>,
    remembered: Option<i64>,
    articles: &[Article],
) -> Option<i64> {
    let present = |id| articles.iter().any(|article| article.id == id);
    selected
        .filter(|id| present(*id))
        .or_else(|| remembered.filter(|id| present(*id)))
}

fn article_navigation_target(articles: &[Article], current_id: i64, delta: isize) -> Option<i64> {
    let index = articles
        .iter()
        .position(|article| article.id == current_id)?;
    let target = index.checked_add_signed(delta)?;
    articles.get(target).map(|article| article.id)
}

fn article_scroll_offset(articles: &[Article], focused_id: Option<i64>) -> Option<f32> {
    focused_id.and_then(|id| {
        articles
            .iter()
            .position(|article| article.id == id)
            .map(|index| index as f32 * ARTICLE_ROW_HEIGHT)
    })
}

fn feed_navigation_target(feeds: &[Feed], current_id: i64, delta: isize) -> Option<i64> {
    let index = feeds.iter().position(|feed| feed.id == current_id)?;
    let target = index.checked_add_signed(delta)?;
    feeds.get(target).map(|feed| feed.id)
}

fn feed_unread_index(entries: &[(i64, usize)]) -> HashMap<i64, usize> {
    entries.iter().copied().collect()
}

fn dashboard_metric(
    ui: &mut egui::Ui,
    label: &str,
    value: usize,
    color: egui::Color32,
    ready: bool,
    route: Option<Route>,
    action: &mut Option<DashboardAction>,
) {
    let theme = ReaderTheme::sspai();
    let mut button = egui::Button::new(egui::RichText::new(label).size(15.0).color(theme.text))
        .min_size(egui::vec2(132.0, 72.0))
        .fill(theme.panel)
        .stroke(egui::Stroke::new(1.0, theme.border))
        .corner_radius(egui::CornerRadius::same(8));
    if !ready {
        button = button.sense(egui::Sense::hover());
    }
    let response = ui.add_enabled(ready, button);
    if response.clicked()
        && let Some(route) = route
    {
        *action = Some(DashboardAction::Navigate(route));
    }
    ui.painter().text(
        response.rect.left_top() + egui::vec2(12.0, 9.0),
        egui::Align2::LEFT_TOP,
        if ready {
            value.to_string()
        } else {
            "加载中…".to_owned()
        },
        egui::FontId::proportional(18.0),
        if ready { color } else { theme.muted },
    );
}

enum DashboardAction {
    Navigate(Route),
    OpenArticle {
        article_id: i64,
        feed_id: i64,
        saved: bool,
    },
    RefreshAll,
    RetryFeeds(Vec<i64>),
}

#[derive(Debug, Clone, Copy, Default)]
struct ArticleRouteMemory {
    selected_article_id: Option<i64>,
    body_scroll: f32,
}

const AUTO_READ_DELAY: Duration = Duration::from_secs(10);

/// Tracks the continuous time an article has been successfully visible.
///
/// The clock deliberately stops whenever the caller reports that the article
/// is not eligible (for example while the window is unfocused or a modal is
/// open). Changing articles starts a fresh interval.
#[derive(Debug, Default)]
struct DelayedReadMarking {
    article_id: Option<i64>,
    elapsed: Duration,
    last_tick: Option<Instant>,
}

impl DelayedReadMarking {
    fn reset_for(&mut self, article_id: Option<i64>) {
        self.article_id = article_id;
        self.elapsed = Duration::ZERO;
        self.last_tick = None;
    }

    fn observe(&mut self, article_id: Option<i64>, eligible: bool, now: Instant) -> bool {
        if self.article_id != article_id {
            self.reset_for(article_id);
        }

        let Some(_) = article_id else {
            self.last_tick = None;
            return false;
        };
        if !eligible {
            self.last_tick = None;
            return false;
        }

        let elapsed = self
            .last_tick
            .replace(now)
            .map(|last| now.saturating_duration_since(last))
            .unwrap_or(Duration::ZERO);
        self.elapsed = self.elapsed.saturating_add(elapsed);
        if self.elapsed >= AUTO_READ_DELAY {
            self.elapsed = Duration::ZERO;
            self.last_tick = None;
            true
        } else {
            false
        }
    }
}

#[derive(Debug, Clone)]
struct TagDialog {
    article_id: i64,
    draft: String,
    original: String,
    focus_input: bool,
}

impl SelectedQuote {
    fn capture(&self) -> ExcerptCapture {
        ExcerptCapture {
            article_id: self.article_id,
            selected_text: self.text.clone(),
            anchor: TextAnchor {
                start_offset: self.start_offset,
                end_offset: self.end_offset,
                prefix: self.anchor_prefix.clone(),
                suffix: self.anchor_suffix.clone(),
            },
        }
    }

    fn from_excerpt(excerpt: &ExcerptView) -> Self {
        Self {
            article_id: excerpt.article_id,
            text: excerpt.selected_text.clone(),
            start_offset: excerpt.anchor.start_offset,
            end_offset: excerpt.anchor.end_offset,
            anchor_prefix: excerpt.anchor.prefix.clone(),
            anchor_suffix: excerpt.anchor.suffix.clone(),
        }
    }
}

enum ModalState {
    AddFeed(feed_subscription_feature::AddDraft),
    DeleteFeed(feed_subscription_feature::DeleteDraft),
    Search(library_search_feature::SearchDialog),
    EditTags(TagDialog),
    WriteThought(excerpt_thought_feature::CommentDialog),
    DeleteExcerpt(excerpt_thought_feature::DeleteExcerptDialog),
    SaveWebPage(web_clipping_feature::WebClipDialog),
    DeleteWebPage(web_clipping_feature::DeleteWebClipDialog),
    AddResource(resource_feature::AddDraft),
    DeleteResource(resource_feature::DeleteDraft),
    ImportResources(resource_feature::ImportDraft),
    RestoreBackup(BackupEntry),
    ClearImages,
}

impl ModalPayload for ModalState {
    fn kind(&self) -> ModalKind {
        match self {
            Self::AddFeed(_) => ModalKind::AddFeed,
            Self::DeleteFeed(_) => ModalKind::DeleteFeed,
            Self::Search(_) => ModalKind::Search,
            Self::EditTags(_) => ModalKind::EditTags,
            Self::WriteThought(_) => ModalKind::WriteThought,
            Self::DeleteExcerpt(_) => ModalKind::DeleteExcerpt,
            Self::SaveWebPage(_) => ModalKind::SaveWebPage,
            Self::DeleteWebPage(_) => ModalKind::DeleteWebPage,
            Self::AddResource(_) => ModalKind::AddResource,
            Self::DeleteResource(_) => ModalKind::DeleteResource,
            Self::ImportResources(_) => ModalKind::ImportResources,
            Self::RestoreBackup(_) => ModalKind::RestoreBackup,
            Self::ClearImages => ModalKind::ClearImages,
        }
    }

    fn is_dirty(&self) -> bool {
        match self {
            Self::AddFeed(dialog) => dialog.is_dirty(),
            Self::Search(_) => false,
            Self::EditTags(dialog) => dialog.draft != dialog.original,
            Self::WriteThought(dialog) => dialog.is_dirty(),
            Self::SaveWebPage(dialog) => dialog.is_dirty(),
            Self::AddResource(dialog) => dialog.is_dirty(),
            Self::ImportResources(dialog) => dialog.is_dirty(),
            Self::DeleteFeed(_)
            | Self::DeleteExcerpt(_)
            | Self::DeleteWebPage(_)
            | Self::DeleteResource(_)
            | Self::RestoreBackup(_)
            | Self::ClearImages => false,
        }
    }
}

enum PanelState {
    ResourceEditor(resource_feature::EditorDraft),
    FeedSettings(feed_subscription_feature::SettingsDraft),
}

impl PanelPayload for PanelState {
    fn is_dirty(&self) -> bool {
        match self {
            Self::ResourceEditor(dialog) => dialog.is_dirty(),
            Self::FeedSettings(panel) => panel.is_dirty(),
        }
    }

    fn is_compatible(&self, route: Route) -> bool {
        match self {
            Self::ResourceEditor(_) => route == Route::Resources,
            Self::FeedSettings(panel) => {
                route == Route::Articles(ArticleCollection::Feed(Some(panel.feed_id())))
            }
        }
    }
}

#[derive(Debug, Clone)]
struct SelectionPopoverState {
    quote: SelectedQuote,
    generation: u64,
}

type InteractionState = UiState<ModalState, PanelState, SelectionPopoverState>;

#[derive(Debug, Clone)]
struct SelectionPopupGeometry {
    /// 选中文字第一行在全局坐标中的矩形。
    anchor_rect: egui::Rect,
    source_layer: egui::LayerId,
    /// 浮层打开时的正文滚动位置；正文一滚动就关闭，避免浮层漂离选区。
    scroll_offset: egui::Vec2,
    viewport_rect: egui::Rect,
    generation: u64,
}

#[derive(Debug, Clone)]
struct SelectionPopupRequest {
    quote: SelectedQuote,
    anchor_rect: egui::Rect,
    source_layer: egui::LayerId,
}

#[derive(Debug, Clone, Copy)]
enum SelectionAction {
    Copy,
    Favorite,
    Comment,
}

#[derive(Debug, Clone)]
struct StorageOverview {
    database_bytes: u64,
    log_bytes: u64,
    image_cache: CacheStats,
    backup_bytes: u64,
    backups: Vec<BackupEntry>,
}

enum StorageAction {
    Check,
    Backup(BackupProtection),
    Compact,
    PruneImages,
    ClearImages,
    PruneBackups,
    OpenFolder,
    RequestRestore(BackupEntry),
    ConfirmRestore(BackupEntry),
}

impl GuiApp {
    fn has_modal_dialog(&self) -> bool {
        self.ui_state.has_modal()
    }

    fn update_delayed_read_marking(
        &mut self,
        ctx: &egui::Context,
        article_id: Option<i64>,
        body_rendered: bool,
    ) {
        let focused = ctx.input(|input| input.viewport().focused.unwrap_or(true));
        let eligible = body_rendered && focused && !self.has_modal_dialog();
        if self
            .delayed_read_marking
            .observe(article_id, eligible, Instant::now())
        {
            if let Some(article_id) = article_id
                && let Err(error) =
                    self.apply_article_library_change(ArticleLifecycleChange::SetRead {
                        article_id,
                        target: true,
                    })
            {
                self.report_article_library_failure("自动标记已读", error);
            }
        } else if eligible {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
    }

    fn apply_ui_effects(&mut self, effects: Vec<UiEffect>) {
        for effect in effects {
            match effect {
                UiEffect::RouteChanged { from, to } => {
                    if let Some(collection) = from.article_collection() {
                        self.article_route_memory.insert(
                            collection,
                            ArticleRouteMemory {
                                selected_article_id: self.sel_article_id,
                                body_scroll: self.current_body_scroll,
                            },
                        );
                    }
                    self.sel_article_id = None;
                    self.body_article_id = None;
                    self.current_body_scroll = 0.0;
                    self.delayed_read_marking.reset_for(None);
                    self.clear_selection_popover();
                    if to == Route::Storage {
                        self.refresh_storage_overview();
                    }
                }
                UiEffect::PersistRoute(key) => self.route_storage_key = Some(key),
                UiEffect::InvalidTransition(message) => self.notice(message),
            }
        }
    }

    fn navigate(&mut self, route: Route) -> bool {
        let effects = self.ui_state.reduce(UiAction::Navigate(route));
        self.apply_ui_effects(effects);
        self.ui_state.route() == route
    }

    fn open_modal(&mut self, modal: ModalState) {
        let effects = self.ui_state.reduce(UiAction::OpenModal(modal));
        self.apply_ui_effects(effects);
        if self.ui_state.has_modal() {
            self.selection_popup_geometry = None;
        }
    }

    fn close_modal(&mut self) {
        let effects = self.ui_state.reduce(UiAction::CloseModal);
        self.apply_ui_effects(effects);
    }

    fn complete_modal(&mut self) {
        let effects = self.ui_state.reduce(UiAction::CompleteModal);
        self.apply_ui_effects(effects);
    }

    fn set_feed_settings_panel(&mut self, feed: Option<Feed>) {
        let panel = feed.map(|feed| {
            PanelState::FeedSettings(feed_subscription_feature::SettingsDraft::from_feed(&feed))
        });
        let effects = self.ui_state.reduce(UiAction::SetPanel(panel));
        self.apply_ui_effects(effects);
    }

    fn feed_settings_panel(&self) -> Option<&feed_subscription_feature::SettingsDraft> {
        match self.ui_state.panel() {
            Some(PanelState::FeedSettings(panel)) => Some(panel),
            Some(PanelState::ResourceEditor(_)) | None => None,
        }
    }

    fn notice(&mut self, message: impl Into<String>) {
        self.ui_state.show_notice(message, Instant::now());
    }

    fn apply_modal_host_action(&mut self, action: ModalHostAction) {
        let effects = match action {
            ModalHostAction::None => Vec::new(),
            ModalHostAction::RequestClose => self.ui_state.reduce(UiAction::CloseModal),
            ModalHostAction::KeepEditing => self.ui_state.reduce(UiAction::KeepEditing),
            ModalHostAction::ConfirmDiscard => self.ui_state.reduce(UiAction::ConfirmDiscard),
        };
        self.apply_ui_effects(effects);
    }

    fn apply_resource_feature_outcome(
        &mut self,
        outcome: resource_feature::Outcome,
        context: &egui::Context,
    ) {
        self.apply_modal_host_action(outcome.modal_action);
        if let Some(projection) = outcome.projection {
            self.desktop_projection
                .accept(DesktopProjectionFact::adopt_resource(projection));
        }
        match outcome.interaction {
            resource_feature::InteractionIntent::None => {}
            resource_feature::InteractionIntent::CompleteModal => self.complete_modal(),
            resource_feature::InteractionIntent::ClosePanel => {
                let effects = self.ui_state.reduce(UiAction::SetPanel(None));
                self.apply_ui_effects(effects);
            }
            resource_feature::InteractionIntent::FinishPanel => self.ui_state.finish_panel(),
            resource_feature::InteractionIntent::KeepEditing => {
                let effects = self.ui_state.reduce(UiAction::KeepEditing);
                self.apply_ui_effects(effects);
            }
            resource_feature::InteractionIntent::ConfirmDiscard => {
                let effects = self.ui_state.reduce(UiAction::ConfirmDiscard);
                self.apply_ui_effects(effects);
            }
        }
        if let Some(message) = outcome.notice {
            self.notice(message);
        }
        if let Some(resource_id) = outcome.retry_resource_id {
            match self.knowledge_feature.request_resource_completion(
                resource_id,
                &self.knowledge_engine,
                context,
            ) {
                Ok(()) => self.notice("已重新加入后台处理队列"),
                Err(error) => self.notice(format!("无法重试后台任务：{error:#}")),
            }
        }
    }

    fn apply_feed_feature_outcome(&mut self, outcome: feed_subscription_feature::Outcome) {
        self.apply_modal_host_action(outcome.modal_action);
        match outcome.interaction {
            feed_subscription_feature::InteractionIntent::None => {}
            feed_subscription_feature::InteractionIntent::CompleteModal => self.complete_modal(),
            feed_subscription_feature::InteractionIntent::ClosePanel => {
                let effects = self.ui_state.reduce(UiAction::SetPanel(None));
                self.apply_ui_effects(effects);
            }
            feed_subscription_feature::InteractionIntent::FinishPanel => {
                self.ui_state.finish_panel()
            }
            feed_subscription_feature::InteractionIntent::KeepEditing => {
                let effects = self.ui_state.reduce(UiAction::KeepEditing);
                self.apply_ui_effects(effects);
            }
            feed_subscription_feature::InteractionIntent::ConfirmDiscard => {
                let effects = self.ui_state.reduce(UiAction::ConfirmDiscard);
                self.apply_ui_effects(effects);
            }
        }
        if outcome.reload {
            self.reload();
        }
        if let Some(feed_id) = outcome.select_feed_id {
            self.select_feed(feed_id);
        }
        if let Some(message) = outcome.notice {
            self.notice(message);
        }
    }

    fn clear_selection_popover(&mut self) {
        self.ui_state.reduce(UiAction::SetPopover(None));
        self.selection_popup_geometry = None;
    }

    fn search_dialog(&self) -> Option<&library_search_feature::SearchDialog> {
        match self.ui_state.modal() {
            Some(ModalState::Search(dialog)) => Some(dialog),
            _ => None,
        }
    }

    fn tag_dialog_mut(&mut self) -> Option<&mut TagDialog> {
        match self.ui_state.modal_mut() {
            Some(ModalState::EditTags(dialog)) => Some(dialog),
            _ => None,
        }
    }

    fn web_clip_dialog(&self) -> Option<&web_clipping_feature::WebClipDialog> {
        match self.ui_state.modal() {
            Some(ModalState::SaveWebPage(dialog)) => Some(dialog),
            _ => None,
        }
    }

    fn web_clip_dialog_mut(&mut self) -> Option<&mut web_clipping_feature::WebClipDialog> {
        match self.ui_state.modal_mut() {
            Some(ModalState::SaveWebPage(dialog)) => Some(dialog),
            _ => None,
        }
    }

    pub(crate) fn new(
        cc: &eframe::CreationContext,
        paths: &Paths,
        cfg: Config,
        desktop: DesktopSession,
    ) -> Result<Self> {
        let backup_store = BackupStore::open(&paths.backup_dir)?;
        // Crash recovery must run before this process opens any long-lived
        // database connection, otherwise its own shared writer lease would
        // prevent recovery from acquiring the exclusive lock.
        drop(MaintenanceEngine::start(
            paths.db_file.clone(),
            backup_store.clone(),
            Vec::new(),
        )?);
        let db = Db::open(&paths.db_file)?;
        let resource_enrichment_config = cfg.resource_enrichment.clone();
        let knowledge_engine = KnowledgeEngine::start_with_network_mode(
            paths.db_file.clone(),
            resource_enrichment_config.clone(),
            cfg.network_mode,
        )?;
        let desktop_projection = DesktopLibraryProjection::start(
            paths.db_file.clone(),
            knowledge_engine.projection_observer(),
            cc.egui_ctx.clone(),
        )?;
        let repaint = cc.egui_ctx.clone();
        let rss_refresh =
            RssRefreshWorkflow::start_scheduled(paths.db_file.clone(), cfg.clone(), move || {
                repaint.request_repaint()
            })?;

        let image_store = Arc::new(ImageStore::open(&paths.image_cache_dir)?);
        let _ = image_store.prune_to(DEFAULT_LIMIT_BYTES);
        let web_clipping_lifecycle =
            WebClippingLifecycle::start_with_mode(paths.db_file.clone(), cfg.network_mode);
        let participants: Vec<Arc<dyn MaintenanceParticipant>> = vec![
            rss_refresh.maintenance_participant(),
            knowledge_engine.maintenance_participant(),
            desktop_projection.maintenance_participant(),
            web_clipping_lifecycle.maintenance_participant(),
        ];
        let maintenance_engine =
            MaintenanceEngine::start(paths.db_file.clone(), backup_store.clone(), participants)?;
        let restored_route = cc
            .storage
            .and_then(|storage| storage.get_string("shiyue.desktop.route"))
            .and_then(|value| Route::from_stable_key(&value))
            .unwrap_or_default();
        let reading_positions = cc
            .storage
            .and_then(|storage| storage.get_string("shiyue.desktop.reading_positions"))
            .and_then(|value| serde_json::from_str::<HashMap<i64, f32>>(&value).ok())
            .map(|positions| {
                positions
                    .into_iter()
                    .filter(|(_, offset)| offset.is_finite() && *offset >= 0.0)
                    .collect()
            })
            .unwrap_or_default();
        let last_opened_article_id = cc
            .storage
            .and_then(|storage| storage.get_string("shiyue.desktop.last_opened_article"))
            .and_then(|value| value.parse::<i64>().ok())
            .filter(|id| *id > 0);
        let mut ui_state = InteractionState::default();
        ui_state.initialize_route(restored_route);
        let mut app = GuiApp {
            db_path: paths.db_file.clone(),
            rss_refresh,
            rss_last_terminal_notice: None,
            feeds: Vec::new(),
            batch_mode: false,
            batch_selection: HashSet::new(),
            sel_article_id: None,
            pending_article_focus: None,
            pending_feed_focus: None,
            last_opened_article_id,
            article_route_memory: HashMap::new(),
            current_body_scroll: 0.0,
            body_article_id: None,
            reading_positions,
            delayed_read_marking: DelayedReadMarking::default(),
            article_document: ArticleDocumentPresenter::new(image_store.clone(), cfg.network_mode)?,
            image_store,
            backup_store,
            maintenance_engine,
            maintenance_snapshot: None,
            data_dir: paths.data_dir.clone(),
            log_file: paths.log_file.clone(),
            ui_state,
            route_storage_key: restored_route.stable_key(),
            storage_overview: None,
            storage_message: None,
            pending_selection_anchor: None,
            pending_excerpt_selection_id: None,
            pending_body_scroll: None,
            selection_popup_geometry: None,
            selection_popup_generation: 0,
            web_clipping_lifecycle,
            consumed_web_clipping_terminal: None,
            search_feature: library_search_feature::SearchFeature::new(),
            resource_filter: ResourceFilter::Active,
            desktop_projection,
            desktop_projection_frame: DesktopProjectionFrame::default(),
            knowledge_engine,
            knowledge_feature: knowledge_feature::KnowledgeFeature::new(),
            db: DbSlot(Some(db)),
            desktop,
        };
        app.reload();
        Ok(app)
    }

    fn current_excerpt_projection_scope(&self) -> ExcerptProjectionScope {
        match self.ui_state.route() {
            Route::Excerpts => ExcerptProjectionScope::Library,
            Route::Articles(_) => self
                .sel_article_id
                .map(ExcerptProjectionScope::Article)
                .unwrap_or(ExcerptProjectionScope::Library),
            _ => ExcerptProjectionScope::Library,
        }
    }

    fn desktop_article_projection_scope(&self) -> ProjectionScope {
        self.current_article_projection_scope()
            .unwrap_or(ProjectionScope::ArticleBookmarks)
    }

    fn article_projection(&self, scope: ProjectionScope) -> Option<Arc<ArticleLibraryProjection>> {
        self.desktop_projection_frame
            .article(scope)
            .filter(|view| !matches!(view.freshness, ProjectionFreshness::Maintenance))
            .and_then(|view| view.data.clone())
    }

    fn current_article_projection(&self) -> Option<Arc<ArticleLibraryProjection>> {
        self.article_projection(self.desktop_article_projection_scope())
    }

    fn current_article_freshness(&self) -> Option<ProjectionFreshness> {
        self.desktop_projection_frame
            .article(self.desktop_article_projection_scope())
            .map(|view| view.freshness.clone())
    }

    fn excerpt_projection(
        &self,
        scope: ExcerptProjectionScope,
    ) -> Option<Arc<ExcerptThoughtProjection>> {
        self.desktop_projection_frame
            .excerpt(scope)
            .filter(|view| !matches!(view.freshness, ProjectionFreshness::Maintenance))
            .and_then(|view| view.data.clone())
    }

    fn current_excerpt_projection(&self) -> Option<Arc<ExcerptThoughtProjection>> {
        self.excerpt_projection(self.current_excerpt_projection_scope())
    }

    fn excerpt_count(&self) -> usize {
        self.current_excerpt_projection()
            .as_deref()
            .map_or(0, |projection| projection.counts.library_excerpts)
    }

    fn current_article_projection_scope(&self) -> Option<ProjectionScope> {
        projection_scope_for_route(self.ui_state.route())
    }

    fn accept_article_projection(&mut self, projection: ArticleLibraryProjection) {
        self.desktop_projection
            .accept(DesktopProjectionFact::adopt_article(projection));
    }

    fn accept_excerpt_projection(&mut self, projection: ExcerptThoughtProjection) {
        self.desktop_projection
            .accept(DesktopProjectionFact::adopt_excerpt(projection));
    }

    fn report_article_library_failure(&mut self, action: &str, error: LifecycleFailure) {
        tracing::warn!(
            action,
            kind = ?error.kind,
            operation = ?error.operation,
            detail = %error.technical_detail,
            missing = ?error.missing_article_ids,
            "article library lifecycle failed"
        );
        self.notice(format!("{action}失败：{}", error.user_message));
    }

    fn apply_article_library_change(
        &mut self,
        change: ArticleLifecycleChange,
    ) -> Result<ArticleChangeDisposition, LifecycleFailure> {
        let scope = self.current_article_projection_scope().ok_or_else(|| {
            LifecycleFailure::input("当前页面没有文章资料投影", "ARTICLE_SCOPE_UNAVAILABLE")
        })?;
        let outcome = ArticleLibraryLifecycle::new(&self.db).apply(change, scope)?;
        let disposition = outcome.disposition;
        self.accept_article_projection(outcome.projection);
        Ok(disposition)
    }

    fn refresh_storage_overview(&mut self) {
        let result = (|| -> Result<StorageOverview> {
            let backups = self.backup_store.list()?;
            Ok(StorageOverview {
                database_bytes: self.db.disk_bytes(),
                log_bytes: std::fs::metadata(&self.log_file)
                    .map(|meta| meta.len())
                    .unwrap_or_default(),
                image_cache: self.image_store.stats()?,
                backup_bytes: backups.iter().map(|entry| entry.size).sum(),
                backups,
            })
        })();
        match result {
            Ok(overview) => self.storage_overview = Some(overview),
            Err(error) => self.storage_message = Some(format!("读取资料库占用失败：{error:#}")),
        }
    }

    fn begin_database_maintenance(&mut self, request: MaintenanceRequest) -> Result<()> {
        if !self.db.is_open() {
            anyhow::bail!("MAINTENANCE_IN_PROGRESS: 资料维护已经在进行中");
        }
        self.desktop_projection
            .accept(DesktopProjectionFact::MaintenanceStarted);
        self.db.close();
        match self.maintenance_engine.request(request) {
            Ok(snapshot) => {
                self.maintenance_snapshot = Some(snapshot);
                self.storage_message = Some("资料维护已开始，正在等待后台写入停止".into());
                Ok(())
            }
            Err(error) => {
                let reopen = self.db.reopen(&self.db_path);
                if let Err(reopen_error) = reopen {
                    return Err(anyhow::anyhow!(
                        "{error:#}; 重新打开资料库失败: {reopen_error:#}"
                    ));
                }
                // The request failed before maintenance became active. Roll
                // back the projection transition so the UI can schedule
                // normal loads again instead of remaining blank forever.
                self.desktop_projection
                    .accept(DesktopProjectionFact::MaintenanceEnded);
                Err(error)
            }
        }
    }

    fn receive_maintenance_updates(&mut self, ctx: &egui::Context) {
        let notices = self.maintenance_engine.try_notices();
        for notice in notices {
            match notice {
                MaintenanceNotice::Changed(snapshot) => {
                    let terminal = snapshot.status != MaintenanceStatus::Running;
                    self.maintenance_snapshot = Some(snapshot.clone());
                    if terminal {
                        self.storage_message = snapshot.user_message.clone();
                    }
                }
                MaintenanceNotice::ModuleFault {
                    user_message,
                    technical_detail,
                } => {
                    tracing::error!("maintenance module: {technical_detail}");
                    self.storage_message = Some(user_message);
                }
            }
        }

        let active = crate::local_data_maintenance::MaintenanceFence::observe(&self.db_path)
            .map(|availability| availability.is_active())
            .unwrap_or(true);
        if active
            && self
                .maintenance_snapshot
                .as_ref()
                .is_none_or(|value| !value.active)
        {
            self.maintenance_snapshot = self.maintenance_engine.snapshot().ok().flatten();
        }
        if active && self.db.is_open() {
            self.db.close();
        } else if !active && !self.db.is_open() {
            match self.db.reopen(&self.db_path) {
                Ok(()) => {
                    self.reload();
                    self.storage_overview = None;
                }
                Err(error) => {
                    self.storage_message = Some(format!("重新打开资料库失败：{error:#}"));
                }
            }
        }
        if active
            || self
                .maintenance_snapshot
                .as_ref()
                .is_some_and(|snapshot| snapshot.status == MaintenanceStatus::Running)
        {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
    }

    fn receive_rss_refresh_updates(&mut self) {
        let notices = self.rss_refresh.try_notices().collect::<Vec<_>>();
        for notice in notices {
            match notice {
                RefreshNotice::Changed(run_id) => {
                    let workflow = self.rss_refresh.snapshot();
                    let Some(completed) =
                        workflow.last_completed.filter(|run| run.run_id == run_id)
                    else {
                        continue;
                    };
                    if self.rss_last_terminal_notice == Some(run_id) {
                        continue;
                    }
                    self.rss_last_terminal_notice = Some(run_id);
                    if self.db.is_open() {
                        self.reload();
                    }
                    let feeds_with_new = completed.feeds_with_new_articles();
                    if completed.new_article_count > 0 {
                        self.desktop
                            .notify_new_articles(feeds_with_new, completed.new_article_count);
                    }
                    match completed.status {
                        RefreshRunStatus::Degraded => self.notice(format!(
                            "订阅刷新完成，但有 {} 个源失败",
                            completed.failed_feed_count
                        )),
                        RefreshRunStatus::Failed => {
                            let detail = completed
                                .module_failure
                                .as_ref()
                                .map(|failure| failure.technical_detail.as_str())
                                .or_else(|| {
                                    completed.feeds.iter().find_map(|feed| {
                                        feed.failure
                                            .as_ref()
                                            .map(|failure| failure.technical_detail.as_str())
                                    })
                                })
                                .unwrap_or("没有可用的技术详情");
                            tracing::warn!("RSS refresh run failed: {detail}");
                            self.notice(format!(
                                "订阅刷新失败（{} 个源）",
                                completed.failed_feed_count
                            ));
                        }
                        RefreshRunStatus::Succeeded
                        | RefreshRunStatus::Interrupted
                        | RefreshRunStatus::Fetching
                        | RefreshRunStatus::Committing => {}
                    }
                }
                RefreshNotice::ModuleFault {
                    user_message,
                    technical_detail,
                } => {
                    tracing::error!("RSS refresh workflow: {technical_detail}");
                    self.notice(user_message);
                }
            }
        }
    }

    fn show_maintenance_page(&self, ui: &mut egui::Ui) {
        let snapshot = self.maintenance_snapshot.as_ref();
        egui::CentralPanel::default().show(ui, |ui| {
            ui.vertical_centered(|ui| {
                ui.add_space(120.0);
                ui.heading("资料维护中");
                let stage = snapshot.map(|value| match value.stage {
                    MaintenanceStage::WaitingForWriters => "正在等待后台写入停止",
                    MaintenanceStage::CreatingSafetyBackup => "正在创建恢复前安全副本",
                    MaintenanceStage::Executing => "正在执行资料库操作",
                    MaintenanceStage::Validating => "正在检查资料库完整性",
                    MaintenanceStage::Reopening => "正在重新打开资料库",
                    MaintenanceStage::ResumingParticipants => "正在恢复后台任务",
                    MaintenanceStage::Finished => "资料维护已结束",
                });
                ui.label(stage.unwrap_or("正在同步其他窗口的资料维护状态"));
                ui.add_space(8.0);
                ui.label("此窗口仍可响应；维护结束后会自动恢复。期间不会排队写入操作。");
                ui.spinner();
            });
        });
    }

    fn execute_storage_action(&mut self, action: StorageAction) {
        let result: Result<String> = match action {
            StorageAction::Check => self.db.integrity_check().map(|check| check.details),
            StorageAction::Backup(protection) => self
                .backup_store
                .create(&self.db, protection)
                .map(|entry| format!("备份已创建：{}", entry.path.display())),
            StorageAction::Compact => {
                let result = self.begin_database_maintenance(MaintenanceRequest::Compact);
                self.storage_message = Some(match result {
                    Ok(()) => "数据库压缩已进入后台维护流程".into(),
                    Err(error) => format!("操作失败：{error:#}"),
                });
                return;
            }
            StorageAction::PruneImages => self
                .image_store
                .prune_to(512 * 1024 * 1024)
                .map(|bytes| format!("图片缓存已释放 {}", format_bytes(bytes))),
            StorageAction::ClearImages => self
                .image_store
                .clear()
                .map(|bytes| format!("图片缓存已清空，释放 {}", format_bytes(bytes))),
            StorageAction::PruneBackups => self
                .backup_store
                .prune_keep(DEFAULT_BACKUP_KEEP)
                .map(|bytes| format!("旧备份已清理，释放 {}", format_bytes(bytes))),
            StorageAction::OpenFolder => open::that(&self.data_dir)
                .map(|_| "已打开资料库目录".to_owned())
                .map_err(Into::into),
            StorageAction::RequestRestore(entry) => {
                self.open_modal(ModalState::RestoreBackup(entry));
                return;
            }
            StorageAction::ConfirmRestore(entry) => {
                self.complete_modal();
                let result = self.begin_database_maintenance(MaintenanceRequest::Restore(entry));
                self.storage_message = Some(match result {
                    Ok(()) => "资料库恢复已进入后台维护流程".into(),
                    Err(error) => format!("操作失败：{error:#}"),
                });
                return;
            }
        };
        self.storage_message = Some(match result {
            Ok(message) => message,
            Err(error) => format!("操作失败：{error:#}"),
        });
        self.refresh_storage_overview();
    }

    fn show_storage_page(&mut self, root_ui: &mut egui::Ui) {
        if self.ui_state.route() != Route::Storage {
            return;
        }
        let ctx = root_ui.ctx().clone();
        if self.storage_overview.is_none() {
            self.refresh_storage_overview();
        }
        let overview = self.storage_overview.clone();
        let connection_task = self.knowledge_feature.connection_state().cloned();
        let mut action = None;
        let theme = ReaderTheme::sspai();
        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(theme.canvas)
                    .stroke(egui::Stroke::new(1.0, theme.border))
                    .inner_margin(egui::Margin::symmetric(24, 18)),
            )
            .show(root_ui, |ui| {
                egui::ScrollArea::vertical()
                    .id_salt("storage-page-scroll")
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                ui.heading("资料库与离线缓存");
                ui.label("资料默认保存在本机；图片缓存和备份均可独立清理。所有恢复都会先创建安全副本。");
                ui.add_space(8.0);
                ui.label(egui::RichText::new("界面显示").strong());
                let previous_scale = self.desktop.settings().ui_scale_percent;
                let mut selected_scale = previous_scale;
                ui.horizontal(|ui| {
                    ui.label("界面缩放");
                    egui::ComboBox::from_id_salt("ui-scale-percent")
                        .selected_text(format!("{}%", selected_scale))
                        .show_ui(ui, |ui| {
                            for &percent in self.desktop.ui_scale_options() {
                                ui.selectable_value(
                                    &mut selected_scale,
                                    percent,
                                    format!("{percent}%"),
                                );
                            }
                        });
                    ui.label("字号和控件会立即按比例调整");
                });
                if selected_scale != previous_scale {
                    self.storage_message = Some(match self
                        .desktop
                        .apply(SettingsChange::UiScale(selected_scale), &ctx)
                    {
                        Ok(()) => format!("界面缩放已设为 {selected_scale}%"),
                        Err(error) => format!("界面缩放未修改：{error:#}"),
                    });
                }
                let previous_network_mode = self.desktop.settings().network_mode;
                let mut selected_network_mode = previous_network_mode;
                ui.horizontal(|ui| {
                    ui.label("网络访问");
                    egui::ComboBox::from_id_salt("network-mode")
                        .selected_text(match selected_network_mode {
                            NetworkMode::Strict => "严格公网（默认）",
                            NetworkMode::TunCompatible => "兼容 TUN/代理 DNS",
                        })
                        .show_ui(ui, |ui| {
                            ui.selectable_value(
                                &mut selected_network_mode,
                                NetworkMode::Strict,
                                "严格公网（默认）",
                            );
                            ui.selectable_value(
                                &mut selected_network_mode,
                                NetworkMode::TunCompatible,
                                "兼容 TUN/代理 DNS",
                            );
                        });
                    ui.label("允许代理合成地址；明确内网地址仍会拦截");
                    ui.colored_label(theme.muted, "重启后生效");
                });
                if selected_network_mode != previous_network_mode {
                    self.storage_message = Some(match self
                        .desktop
                        .apply(SettingsChange::NetworkMode(selected_network_mode), &ctx)
                    {
                        Ok(()) => "网络模式已保存，重启后对后台请求生效".into(),
                        Err(error) => format!("网络模式未修改：{error:#}"),
                    });
                }
                ui.separator();
                ui.add_space(8.0);
                if let Some(overview) = &overview {
                    egui::Grid::new("storage-usage").num_columns(2).show(ui, |ui| {
                        ui.label("数据库（含 WAL）");
                        ui.label(format_bytes(overview.database_bytes));
                        ui.end_row();
                        ui.label("图片内容寻址缓存");
                        ui.label(format!(
                            "{} · {} 个对象 / {} 个网址",
                            format_bytes(overview.image_cache.bytes),
                            overview.image_cache.objects,
                            overview.image_cache.references
                        ));
                        ui.end_row();
                        ui.label("数据库备份");
                        ui.label(format!(
                            "{} · {} 份",
                            format_bytes(overview.backup_bytes),
                            overview.backups.len()
                        ));
                        ui.end_row();
                        ui.label("日志");
                        ui.label(format_bytes(overview.log_bytes));
                        ui.end_row();
                    });
                }
                ui.add_space(8.0);
                ui.horizontal_wrapped(|ui| {
                    if ui.button("检查数据库").clicked() {
                        action = Some(StorageAction::Check);
                    }
                    if ui.button("压缩数据库").clicked() {
                        action = Some(StorageAction::Compact);
                    }
                    if ui.button("普通备份").clicked() {
                        action = Some(StorageAction::Backup(BackupProtection::Plain));
                    }
                    if ui
                        .add_enabled(cfg!(windows), egui::Button::new("Windows 用户加密备份"))
                        .on_hover_text("只能由当前 Windows 用户账户解密；适合保存到移动盘或云盘")
                        .clicked()
                    {
                        action = Some(StorageAction::Backup(BackupProtection::WindowsUser));
                    }
                    if ui.button("打开资料目录").clicked() {
                        action = Some(StorageAction::OpenFolder);
                    }
                });
                ui.separator();
                ui.label(egui::RichText::new("DeepSeek AI").strong());
                ui.label("API Key 安全保存在 Windows 凭据管理器，用于资源补全、RSS 总结和中文翻译。");
                ui.horizontal(|ui| {
                    ui.add(
                            egui::TextEdit::singleline(self.knowledge_feature.api_key_draft_mut())
                            .password(true)
                            .hint_text("sk-…")
                            .desired_width(320.0),
                    );
                    if ui.button("保存 Key").clicked() {
                        self.knowledge_feature.save_api_key();
                    }
                    let connection_busy = self.knowledge_feature.connection_busy();
                    if ui
                        .add_enabled(!connection_busy, egui::Button::new("测试连接"))
                        .clicked()
                    {
                        self.knowledge_feature
                            .begin_connection_test(&self.knowledge_engine, &ctx);
                    }
                    if ui.button("删除 Key").clicked() {
                        self.knowledge_feature.delete_api_key();
                    }
                });
                if let Some(message) = self.knowledge_feature.settings_message() {
                    ui.label(message);
                }
                if let Some(state) = &connection_task {
                    match state {
                        ConnectionState::Running => {
                            ui.horizontal(|ui| {
                                ui.spinner();
                                ui.label("正在测试 DeepSeek 连接");
                            });
                        }
                        ConnectionState::Failed { detail } => {
                            ui.colored_label(egui::Color32::RED, "连接测试失败");
                            ui.collapsing("技术详情", |ui| {
                                ui.monospace(detail);
                            });
                        }
                        ConnectionState::Succeeded(_) => {
                            ui.label("连接测试成功");
                        }
                    }
                }
                ui.separator();
                ui.label(egui::RichText::new("清理策略").strong());
                ui.label("图片按内容 SHA-256 去重，使用时更新最近访问时间；超过 1 GB 自动淘汰。备份自动保留最近 10 份。数据库只在手动操作时 VACUUM。");
                ui.horizontal(|ui| {
                    if ui.button("图片缓存收缩到 512 MB").clicked() {
                        action = Some(StorageAction::PruneImages);
                    }
                    if ui.button("清空图片缓存…").clicked() {
                        self.open_modal(ModalState::ClearImages);
                    }
                    if ui.button("清理第 10 份之前的备份").clicked() {
                        action = Some(StorageAction::PruneBackups);
                    }
                });
                if let Some(message) = &self.storage_message {
                    ui.add_space(8.0);
                    ui.label(message);
                }
                if let Some(snapshot) = &self.maintenance_snapshot
                    && snapshot.status != MaintenanceStatus::Running
                {
                    let status = match snapshot.status {
                        MaintenanceStatus::Succeeded => "成功",
                        MaintenanceStatus::Degraded => "部分恢复失败",
                        MaintenanceStatus::Failed => "失败",
                        MaintenanceStatus::Running => "进行中",
                    };
                    ui.label(format!("最近一次资料维护：{status}"));
                    if let Some(detail) = &snapshot.technical_detail {
                        ui.collapsing("技术详情", |ui| {
                            ui.monospace(detail);
                        });
                    }
                    if !snapshot.failed_participants.is_empty() {
                        ui.label(format!(
                            "未恢复的后台模块：{}",
                            snapshot.failed_participants.join("、")
                        ));
                    }
                }
                ui.separator();
                ui.label(egui::RichText::new("可恢复备份").strong());
                egui::ScrollArea::vertical().max_height(210.0).show(ui, |ui| {
                    if let Some(overview) = &overview {
                        for entry in &overview.backups {
                            ui.horizontal(|ui| {
                                let modified: chrono::DateTime<chrono::Local> = entry.modified.into();
                                ui.label(format!(
                                    "{}  {}  {}",
                                    modified.format("%Y-%m-%d %H:%M:%S"),
                                    format_bytes(entry.size),
                                    if entry.protected { "Windows 加密" } else { "普通" }
                                ));
                                if ui.button("恢复…").clicked() {
                                    action = Some(StorageAction::RequestRestore(entry.clone()));
                                }
                            });
                        }
                    }
                });
                    });
            });
        if let Some(action) = action {
            self.execute_storage_action(action);
        }
    }

    fn show_storage_modal(&mut self, ctx: &egui::Context) {
        let kind = self.ui_state.modal_kind();
        match kind {
            Some(ModalKind::ClearImages) => {
                let response = gui_modal::show(ctx, ModalKind::ClearImages, false, |ui, _| {
                    let mut confirm = false;
                    let mut cancel = false;
                    ui.label("只删除可重新下载的图片，不删除文章、摘录或想法。离线图片将在下次阅读时重新下载。");
                    ui.horizontal(|ui| {
                        if ui.button("确认清空").clicked() {
                            confirm = true;
                        }
                        if ui.button("取消").clicked() {
                            cancel = true;
                        }
                    });
                    (confirm, cancel)
                });
                self.apply_modal_host_action(response.action);
                if let Some((confirm, cancel)) = response.inner {
                    if confirm {
                        self.complete_modal();
                        self.execute_storage_action(StorageAction::ClearImages);
                    } else if cancel {
                        self.complete_modal();
                    }
                }
            }
            Some(ModalKind::RestoreBackup) => {
                let entry = match self.ui_state.modal() {
                    Some(ModalState::RestoreBackup(entry)) => entry.clone(),
                    _ => return,
                };
                let response = gui_modal::show(ctx, ModalKind::RestoreBackup, false, |ui, _| {
                    let mut confirm = false;
                    let mut cancel = false;
                    ui.label("当前资料库会先生成安全副本，再恢复所选备份。恢复期间请勿关闭程序。");
                    ui.label(entry.path.display().to_string());
                    ui.horizontal(|ui| {
                        if ui.button("确认恢复").clicked() {
                            confirm = true;
                        }
                        if ui.button("取消").clicked() {
                            cancel = true;
                        }
                    });
                    (confirm, cancel)
                });
                self.apply_modal_host_action(response.action);
                if let Some((confirm, cancel)) = response.inner {
                    if confirm {
                        self.complete_modal();
                        self.execute_storage_action(StorageAction::ConfirmRestore(entry));
                    } else if cancel {
                        self.complete_modal();
                    }
                }
            }
            _ => {}
        }
    }

    fn reload(&mut self) {
        match FeedSubscriptions::session(self.db_path.clone(), &self.rss_refresh).list() {
            Ok(feeds) => self.feeds = feeds.into_iter().map(|(feed, _)| feed).collect(),
            Err(error) => {
                tracing::warn!(detail = %error.technical_detail, "reload subscriptions failed");
                self.notice(error.user_message);
            }
        }
        if let Some(ArticleCollection::Feed(selected)) = self.ui_state.route().article_collection()
        {
            let selected_is_valid =
                selected.is_some_and(|id| self.feeds.iter().any(|feed| feed.id == id));
            if !selected_is_valid {
                let route = Route::Articles(ArticleCollection::Feed(
                    self.feeds.first().map(|feed| feed.id),
                ));
                let effects = self
                    .ui_state
                    .reduce(UiAction::ReplaceUnavailableRoute(route));
                self.apply_ui_effects(effects);
            }
        }
    }

    fn show_feed_dialogs(&mut self, ctx: &egui::Context) {
        let show_discard = self.ui_state.discard_owner() == Some(DiscardOwner::Modal);
        let dependencies = feed_subscription_feature::Dependencies {
            database: self.db_path.as_path(),
            refresh: &self.rss_refresh,
        };
        let outcome = match self.ui_state.modal_mut() {
            Some(ModalState::AddFeed(draft)) => feed_subscription_feature::show_modal(
                ctx,
                feed_subscription_feature::ModalDraft::Add(draft),
                show_discard,
                &dependencies,
            ),
            Some(ModalState::DeleteFeed(draft)) => feed_subscription_feature::show_modal(
                ctx,
                feed_subscription_feature::ModalDraft::Delete(draft),
                show_discard,
                &dependencies,
            ),
            _ => return,
        };
        self.apply_feed_feature_outcome(outcome);
    }
    fn select_feed(&mut self, id: i64) {
        let route = Route::Articles(ArticleCollection::Feed(Some(id)));
        if self.ui_state.route() != route {
            self.navigate(route);
        }
    }

    fn select_saved_articles(&mut self) {
        let route = Route::Articles(ArticleCollection::Saved);
        if self.ui_state.route() != route {
            self.navigate(route);
        }
    }

    fn select_read_later(&mut self) {
        let route = Route::Articles(ArticleCollection::ReadLater);
        if self.ui_state.route() != route {
            self.navigate(route);
        }
    }

    fn open_search(&mut self) {
        if self.search_dialog().is_none() {
            self.open_modal(ModalState::Search(self.search_feature.new_dialog(&self.db)));
        }
        let db = &self.db;
        let Some(dialog) = (match self.ui_state.modal_mut() {
            Some(ModalState::Search(dialog)) => Some(dialog),
            _ => None,
        }) else {
            return;
        };
        library_search_feature::prepare_dialog(dialog, db);
        self.clear_selection_popover();
    }

    fn receive_search_events(&mut self, _ctx: &egui::Context) {
        let dialog = match self.ui_state.modal_mut() {
            Some(ModalState::Search(dialog)) => Some(dialog),
            _ => None,
        };
        for notice in self.search_feature.receive_events(&self.db, dialog) {
            self.notice(notice);
        }
    }

    fn open_search_result(&mut self, hit: &LibrarySearchResult) {
        if let PrimaryIdentity::Resource(resource_id) = hit.primary {
            self.navigate(Route::Resources);
            self.open_resource_panel(resource_id);
            self.complete_modal();
            self.notice("已打开资源详情");
            return;
        }
        let Some(target) = hit.article_targets.first() else {
            self.notice("搜索结果没有可打开的文章位置");
            return;
        };
        self.pending_excerpt_selection_id = target
            .selection_id
            .map(|selection_id| (target.article_id, selection_id));
        if target.archived {
            self.navigate(Route::Articles(ArticleCollection::SearchResult(
                target.article_id,
            )));
            self.sel_article_id = Some(target.article_id);
            self.body_article_id = None;
            self.clear_selection_popover();
            self.complete_modal();
            self.notice("正在查看已归档文章（未恢复）");
            return;
        }

        if target.web_clipping {
            self.select_saved_articles();
        } else {
            self.select_feed(target.feed_id);
        }
        self.select_article(target.article_id);
        self.complete_modal();
        self.notice("已打开搜索结果");
    }

    fn open_resource_panel(&mut self, resource_id: i64) {
        let resource = self
            .desktop_projection_frame
            .resource(ResourceProjectionDemand::Detail(resource_id))
            .and_then(|view| view.data.as_ref())
            .and_then(|projection| projection.detail.as_ref())
            .map(|detail| &detail.resource);
        let panel =
            PanelState::ResourceEditor(resource_feature::EditorDraft::open(resource_id, resource));
        let effects = self.ui_state.reduce(UiAction::SetPanel(Some(panel)));
        self.apply_ui_effects(effects);
    }

    /// 仅切换当前文章；已读由正文成功显示后的延迟阅读计时器负责。
    fn select_article(&mut self, id: i64) {
        if self.sel_article_id != Some(id) {
            self.body_article_id = None;
            self.current_body_scroll = 0.0;
            self.pending_body_scroll = self
                .reading_positions
                .get(&id)
                .copied()
                .map(|offset| (id, offset));
            self.delayed_read_marking.reset_for(Some(id));
            self.clear_selection_popover();
        }
        self.sel_article_id = Some(id);
        self.last_opened_article_id = Some(id);
    }

    fn mark_unread(&mut self, id: i64) {
        let projection = self.current_article_projection();
        if projection
            .as_deref()
            .and_then(|projection| projection.articles.iter().find(|article| article.id == id))
            .is_none_or(|article| !article.is_read)
        {
            return;
        }
        if let Err(error) = self.apply_article_library_change(ArticleLifecycleChange::SetRead {
            article_id: id,
            target: false,
        }) {
            self.report_article_library_failure("标记未读", error);
        }
    }

    fn toggle_star(&mut self, id: i64) {
        let projection = self.current_article_projection();
        let Some(was_starred) = projection
            .as_deref()
            .and_then(|projection| projection.articles.iter().find(|article| article.id == id))
            .map(|article| article.starred)
        else {
            return;
        };

        match self.apply_article_library_change(ArticleLifecycleChange::SetBookmark {
            article_id: id,
            target: !was_starred,
        }) {
            Ok(_) => {
                self.notice(if was_starred {
                    "已取消文章收藏"
                } else {
                    "已收藏文章，可在左侧「文章收藏」查看"
                });
            }
            Err(error) => self.report_article_library_failure(
                if was_starred {
                    "取消文章收藏"
                } else {
                    "收藏文章"
                },
                error,
            ),
        }
    }

    fn toggle_read_later(&mut self, id: i64) {
        let projection = self.current_article_projection();
        let current = projection
            .as_deref()
            .and_then(|projection| projection.articles.iter().find(|article| article.id == id))
            .map(|article| article.read_later);
        let Some(current) = current else {
            return;
        };
        match self.apply_article_library_change(ArticleLifecycleChange::SetReadLater {
            article_id: id,
            target: !current,
        }) {
            Ok(_) => {
                self.notice(if current {
                    "已移出稍后读"
                } else {
                    "已加入稍后读"
                });
            }
            Err(error) => self.report_article_library_failure("更新稍后读", error),
        }
    }

    fn apply_batch_action(&mut self, action: ArticleBatchAction) {
        let ids = self.batch_selection.iter().copied().collect::<Vec<_>>();
        if ids.is_empty() {
            self.notice("请先勾选文章");
            return;
        }
        match self.apply_article_library_change(ArticleLifecycleChange::Batch {
            article_ids: ids,
            action,
        }) {
            Ok(disposition) => {
                self.batch_selection.clear();
                let action_name = match action {
                    ArticleBatchAction::Archive => "归档",
                    ArticleBatchAction::Bookmark => "收藏",
                    ArticleBatchAction::ReadLater => "加入稍后读",
                };
                let changed = disposition.changed_articles();
                self.notice(format!("已批量{action_name} {changed} 篇文章"));
            }
            Err(error) => self.report_article_library_failure("批量操作", error),
        }
    }

    fn open_tag_dialog(&mut self, article_id: i64) {
        let projection = self.current_article_projection();
        let tags = projection
            .as_deref()
            .and_then(|projection| projection.tags.get(&article_id))
            .cloned()
            .unwrap_or_default();
        let draft = tags.join(", ");
        self.open_modal(ModalState::EditTags(TagDialog {
            article_id,
            original: draft.clone(),
            draft,
            focus_input: true,
        }));
    }

    fn show_tag_dialog(&mut self, ctx: &egui::Context) {
        if self.ui_state.modal_kind() != Some(ModalKind::EditTags) {
            return;
        }
        let show_discard = self.ui_state.discard_owner() == Some(DiscardOwner::Modal);
        let Some(dialog) = self.tag_dialog_mut() else {
            return;
        };
        let mut save = false;
        let response = gui_modal::show(ctx, ModalKind::EditTags, show_discard, |ui, focus| {
            ui.label("用逗号或换行分隔多个标签：");
            ui.add_space(6.0);
            let input = ui.add_sized(
                egui::vec2(ui.available_width(), 88.0),
                egui::TextEdit::multiline(&mut dialog.draft).hint_text("架构, Rust, 稍后整理"),
            );
            if focus == InitialFocus::PrimaryField && dialog.focus_input {
                input.request_focus();
                dialog.focus_input = false;
            }
            ui.add_space(8.0);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("保存").clicked() {
                    save = true;
                }
            });
        });
        let save_input = save.then(|| {
            let names = dialog
                .draft
                .split([',', '，', '\n'])
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .map(str::to_owned)
                .collect::<Vec<_>>();
            (dialog.article_id, names)
        });
        self.apply_modal_host_action(response.action);
        if let Some((article_id, names)) = save_input {
            match self.apply_article_library_change(ArticleLifecycleChange::ReplaceTags {
                article_id,
                names,
            }) {
                Ok(_) => {
                    self.complete_modal();
                    self.notice("标签已保存");
                }
                Err(error) => self.report_article_library_failure("保存标签", error),
            }
        }
    }

    fn archive_article(&mut self, id: i64) {
        match self.apply_article_library_change(ArticleLifecycleChange::SetArchived {
            article_id: id,
            target: true,
        }) {
            Ok(_) => {
                self.notice("文章已归档，后续刷新不会重新出现");
            }
            Err(error) => self.report_article_library_failure("归档文章", error),
        }
    }

    fn open_web_clip_dialog(&mut self) {
        if self.ui_state.modal_kind() != Some(ModalKind::SaveWebPage) {
            self.open_modal(ModalState::SaveWebPage(
                web_clipping_feature::WebClipDialog::default(),
            ));
        }
        self.clear_selection_popover();
    }

    fn receive_web_clipping_updates(&mut self, ctx: &egui::Context) {
        let modal_snapshot = self
            .web_clip_dialog()
            .and_then(web_clipping_feature::WebClipDialog::capture_snapshot);
        if modal_snapshot
            .as_ref()
            .is_some_and(|snapshot| !snapshot.state.is_terminal())
        {
            ctx.request_repaint_after(Duration::from_millis(80));
        }
        let terminal = modal_snapshot
            .filter(|snapshot| snapshot.state.is_terminal())
            .or_else(|| self.web_clipping_lifecycle.recent_terminal());
        let Some(snapshot) = terminal else {
            return;
        };
        let key = (snapshot.id, snapshot.revision);
        if self.consumed_web_clipping_terminal == Some(key) {
            return;
        }
        self.consumed_web_clipping_terminal = Some(key);
        self.apply_web_clipping_terminal(snapshot);
        ctx.request_repaint();
    }

    fn apply_web_clipping_terminal(&mut self, snapshot: CaptureSnapshot) {
        let modal_owns_capture = matches!(
            self.ui_state.modal(),
            Some(ModalState::SaveWebPage(dialog)) if dialog.owns_capture(snapshot.id)
        );
        match snapshot.state {
            CaptureState::Succeeded(success) => {
                if modal_owns_capture {
                    self.complete_modal();
                }
                let article_id = success.clipping.article_id;
                self.navigate(Route::Articles(ArticleCollection::Saved));
                self.accept_article_projection(success.projection);
                self.select_article(article_id);
                tracing::info!(
                    capture_id = snapshot.id.value(),
                    article_id,
                    provenance = ?success.provenance,
                    "web clipping capture succeeded"
                );
                self.notice("正文快照已保存到本机；网页图片仍需联网加载");
            }
            CaptureState::Failed(failure) => {
                tracing::warn!(
                    capture_id = snapshot.id.value(),
                    kind = ?failure.kind,
                    detail = %failure.technical_detail,
                    "web clipping capture failed"
                );
                if modal_owns_capture {
                    if let Some(dialog) = self.web_clip_dialog_mut() {
                        dialog.fail_capture(failure.user_message);
                    }
                } else {
                    self.notice(failure.user_message);
                }
            }
            CaptureState::Cancelled(failure) => {
                tracing::info!(
                    capture_id = snapshot.id.value(),
                    kind = ?failure.kind,
                    detail = %failure.technical_detail,
                    "web clipping capture cancelled"
                );
                if modal_owns_capture {
                    if failure.kind == CaptureFailureKind::Maintenance {
                        if let Some(dialog) = self.web_clip_dialog_mut() {
                            dialog.fail_capture(failure.user_message);
                        }
                    } else {
                        self.complete_modal();
                    }
                } else if failure.kind == CaptureFailureKind::Maintenance {
                    self.notice(failure.user_message);
                }
            }
            CaptureState::Fetching | CaptureState::Preparing | CaptureState::Committing => {}
        }
    }

    fn receive_knowledge_updates(&mut self, ctx: &egui::Context) {
        let notices = self
            .knowledge_feature
            .receive_updates(&self.knowledge_engine, ctx);
        for notice in notices {
            self.notice(notice);
        }
    }

    fn publish_pending_knowledge_notices(&mut self) {
        let notices = self
            .knowledge_feature
            .publish_pending_notices(&self.desktop_projection_frame);
        for notice in notices {
            self.notice(notice);
        }
    }

    fn knowledge_task(&self, kind: KnowledgeTaskKind, target_id: i64) -> Option<TaskSnapshot> {
        let key = TaskKey::new(kind, target_id);
        self.desktop_projection_frame
            .knowledge(key)
            .and_then(|view| {
                (!matches!(view.freshness, ProjectionFreshness::Maintenance))
                    .then(|| view.data.clone())
                    .flatten()
            })
    }

    fn refresh_desktop_projection_frame(&mut self) {
        use crate::resource_library_lifecycle::ResourceCollection;
        let mut demand = DesktopProjectionDemand::default();
        let article_scope = self.desktop_article_projection_scope();
        demand.articles.push(article_scope);
        if self.ui_state.route() == Route::Dashboard {
            demand.articles.push(ProjectionScope::Unread);
            if let Some(article_id) = self.last_opened_article_id {
                demand.articles.push(ProjectionScope::Article(article_id));
            }
        }
        let excerpt_scope = self.current_excerpt_projection_scope();
        demand.excerpts.push(excerpt_scope);
        if self.ui_state.route() == Route::Dashboard {
            demand.excerpts.push(ExcerptProjectionScope::Library);
            demand.resources.push(ResourceProjectionDemand::Collection(
                ResourceCollection::Active,
            ));
        }
        if self.ui_state.route() == Route::Resources {
            let collection = match self.resource_filter {
                ResourceFilter::Active => ResourceCollection::Active,
                ResourceFilter::PendingReview => ResourceCollection::PendingReview,
                ResourceFilter::Broken => ResourceCollection::Broken,
                ResourceFilter::Archived => ResourceCollection::Archived,
            };
            demand
                .resources
                .push(ResourceProjectionDemand::Collection(collection));
            if let Some(resource_id) = self.ui_state.panel().and_then(|panel| match panel {
                PanelState::ResourceEditor(draft) => Some(draft.id()),
                PanelState::FeedSettings(_) => None,
            }) {
                demand
                    .resources
                    .push(ResourceProjectionDemand::Detail(resource_id));
                demand.knowledge.push(TaskKey::new(
                    KnowledgeTaskKind::ResourceCompletion,
                    resource_id,
                ));
            }
        }
        if self.ui_state.route().article_collection().is_some()
            && let Some(article_id) = self.sel_article_id
        {
            demand
                .knowledge
                .push(TaskKey::new(KnowledgeTaskKind::ArticleSummary, article_id));
        }
        demand
            .knowledge
            .extend(self.knowledge_feature.watched_keys());
        self.desktop_projection_frame = self.desktop_projection.frame(demand);

        let article_projection = self.article_projection(article_scope);
        if let Some(projection) = article_projection {
            let remembered = self
                .ui_state
                .route()
                .article_collection()
                .and_then(|collection| self.article_route_memory.get(&collection))
                .and_then(|memory| memory.selected_article_id);
            let reconciled =
                reconcile_article_selection(self.sel_article_id, remembered, &projection.articles);
            if self.sel_article_id.is_some() && reconciled != self.sel_article_id {
                self.sel_article_id = None;
                self.body_article_id = None;
                self.clear_selection_popover();
            }
            if self.sel_article_id.is_none()
                && let Some(restored) = reconciled
                && let Some(collection) = self.ui_state.route().article_collection()
            {
                let offset = self
                    .article_route_memory
                    .get(&collection)
                    .map(|memory| memory.body_scroll)
                    .or_else(|| self.reading_positions.get(&restored).copied())
                    .unwrap_or(0.0);
                self.sel_article_id = Some(restored);
                self.pending_body_scroll = Some((restored, offset));
            }
        }

        if let Some(projection) = self.excerpt_projection(excerpt_scope)
            && let Some((article_id, excerpt_id)) = self.pending_excerpt_selection_id
            && projection.scope == ExcerptProjectionScope::Article(article_id)
        {
            self.pending_selection_anchor = projection
                .excerpt(excerpt_id)
                .map(ExcerptView::as_article_selection);
            self.pending_excerpt_selection_id = None;
        }
    }

    fn current_article_counts(&self) -> (usize, usize, usize) {
        self.current_article_projection()
            .as_deref()
            .map(|projection| {
                (
                    projection.counts.bookmarks,
                    projection.counts.read_later,
                    projection.counts.archived,
                )
            })
            .unwrap_or_default()
    }

    fn remove_saved_article(&mut self, id: i64) {
        let projection = self.current_article_projection();
        let is_web_clipping = projection
            .as_deref()
            .is_some_and(|projection| projection.fixed_bookmark_ids.contains(&id));
        if is_web_clipping {
            let title = projection
                .as_deref()
                .and_then(|projection| projection.articles.iter().find(|article| article.id == id))
                .and_then(|article| article.title.clone())
                .unwrap_or_else(|| "未命名网页".to_owned());
            self.open_modal(ModalState::DeleteWebPage(
                web_clipping_feature::DeleteWebClipDialog::new(id, title),
            ));
        } else {
            self.toggle_star(id);
        }
    }

    fn save_favorite_quote(&mut self, quote: SelectedQuote) {
        let article_id = quote.article_id;
        let outcome = excerpt_thought_feature::ensure_excerpt(
            quote.capture(),
            ExcerptProjectionScope::Article(article_id),
            &excerpt_thought_feature::Dependencies {
                db: &self.db,
                clock: &crate::excerpt_thought_lifecycle::SYSTEM_CLOCK,
            },
        );
        self.apply_excerpt_thought_feature_outcome(outcome);
    }

    fn begin_comment(&mut self, quote: SelectedQuote) {
        let projection = self.current_excerpt_projection();
        self.open_modal(ModalState::WriteThought(
            excerpt_thought_feature::new_comment_dialog(quote, projection.as_deref()),
        ));
    }

    fn begin_edit_thought(&mut self, excerpt: &ExcerptView) {
        self.open_modal(ModalState::WriteThought(
            excerpt_thought_feature::new_edit_dialog(excerpt),
        ));
    }

    fn remove_thought(&mut self, excerpt_id: i64, scope: ExcerptProjectionScope) {
        let outcome = excerpt_thought_feature::remove_thought(
            excerpt_id,
            scope,
            &excerpt_thought_feature::Dependencies {
                db: &self.db,
                clock: &crate::excerpt_thought_lifecycle::SYSTEM_CLOCK,
            },
        );
        self.apply_excerpt_thought_feature_outcome(outcome);
    }

    fn request_delete_excerpt(&mut self, excerpt: &ExcerptView, scope: ExcerptProjectionScope) {
        if excerpt.thought.is_some() {
            self.open_modal(ModalState::DeleteExcerpt(
                excerpt_thought_feature::new_delete_dialog(excerpt, scope),
            ));
        } else {
            let outcome = excerpt_thought_feature::delete_excerpt(
                excerpt.id,
                scope,
                &excerpt_thought_feature::Dependencies {
                    db: &self.db,
                    clock: &crate::excerpt_thought_lifecycle::SYSTEM_CLOCK,
                },
            );
            self.apply_excerpt_thought_feature_outcome(outcome);
        }
    }

    fn show_excerpt_thought_dialogs(&mut self, ctx: &egui::Context) {
        let show_discard = self.ui_state.discard_owner() == Some(DiscardOwner::Modal);
        let dependencies = excerpt_thought_feature::Dependencies {
            db: &self.db,
            clock: &crate::excerpt_thought_lifecycle::SYSTEM_CLOCK,
        };
        let outcome = match self.ui_state.modal_mut() {
            Some(ModalState::WriteThought(draft)) => excerpt_thought_feature::show_modal(
                ctx,
                excerpt_thought_feature::ModalDraft::Thought(draft),
                show_discard,
                &dependencies,
            ),
            Some(ModalState::DeleteExcerpt(draft)) => excerpt_thought_feature::show_modal(
                ctx,
                excerpt_thought_feature::ModalDraft::Delete(draft),
                false,
                &dependencies,
            ),
            _ => return,
        };
        self.apply_excerpt_thought_feature_outcome(outcome);
    }

    fn apply_excerpt_thought_feature_outcome(&mut self, outcome: excerpt_thought_feature::Outcome) {
        self.apply_modal_host_action(outcome.modal_action);
        match outcome.interaction {
            excerpt_thought_feature::InteractionIntent::None => {}
            excerpt_thought_feature::InteractionIntent::CloseModal => self.close_modal(),
            excerpt_thought_feature::InteractionIntent::CompleteModal => self.complete_modal(),
        }
        if let Some(projection) = outcome.projection {
            self.accept_excerpt_projection(projection);
        }
        if let Some(message) = outcome.notice {
            self.notice(message);
        }
        if let Some(request) = outcome.pending_delete {
            let follow_up = excerpt_thought_feature::execute_delete(
                request,
                &excerpt_thought_feature::Dependencies {
                    db: &self.db,
                    clock: &crate::excerpt_thought_lifecycle::SYSTEM_CLOCK,
                },
            );
            self.apply_excerpt_thought_feature_outcome(follow_up);
        }
    }

    fn show_web_clipping_dialogs(&mut self, ctx: &egui::Context) {
        let show_discard = self.ui_state.discard_owner() == Some(DiscardOwner::Modal);
        let dependencies = web_clipping_feature::Dependencies {
            lifecycle: &self.web_clipping_lifecycle,
            delete_scope: self
                .current_article_projection_scope()
                .unwrap_or(ProjectionScope::ArticleBookmarks),
        };
        let outcome = match self.ui_state.modal_mut() {
            Some(ModalState::SaveWebPage(draft)) => web_clipping_feature::show_modal(
                ctx,
                web_clipping_feature::ModalDraft::Save(draft),
                show_discard,
                &dependencies,
            ),
            Some(ModalState::DeleteWebPage(draft)) => web_clipping_feature::show_modal(
                ctx,
                web_clipping_feature::ModalDraft::Delete(draft),
                false,
                &dependencies,
            ),
            _ => return,
        };
        self.apply_web_clipping_feature_outcome(outcome);
    }

    fn apply_web_clipping_feature_outcome(&mut self, outcome: web_clipping_feature::Outcome) {
        self.apply_modal_host_action(outcome.modal_action);
        if let Some(projection) = outcome.projection {
            self.accept_article_projection(projection);
        }
        if let Some(article_id) = outcome.deleted_article_id
            && self.sel_article_id == Some(article_id)
        {
            self.sel_article_id = None;
            self.body_article_id = None;
        }
        if matches!(
            outcome.interaction,
            web_clipping_feature::InteractionIntent::CompleteModal
        ) {
            self.complete_modal();
        }
        if let Some(message) = outcome.notice {
            self.notice(message);
        }
    }

    fn show_resource_library_page(&mut self, root_ui: &mut egui::Ui) {
        if self.ui_state.route() != Route::Resources {
            return;
        }
        let ctx = root_ui.ctx().clone();
        use crate::resource_library_lifecycle::{
            ResourceCollection, ResourceCurationState, ResourceHealth, ResourcePrivacy, SystemClock,
        };
        let collection = match self.resource_filter {
            ResourceFilter::Active => ResourceCollection::Active,
            ResourceFilter::PendingReview => ResourceCollection::PendingReview,
            ResourceFilter::Broken => ResourceCollection::Broken,
            ResourceFilter::Archived => ResourceCollection::Archived,
        };
        let demand = ResourceProjectionDemand::Collection(collection);
        let projection_view = self.desktop_projection_frame.resource(demand);
        let projection = projection_view.and_then(|view| view.data.clone());
        let rows = projection
            .as_deref()
            .map(|projection| projection.resources.as_slice())
            .unwrap_or_default();
        let counts = projection
            .as_deref()
            .map(|projection| projection.counts)
            .unwrap_or_default();
        let projection_error = projection_view.and_then(|view| match &view.freshness {
            ProjectionFreshness::Failed { technical_detail } => Some(technical_detail.clone()),
            _ => None,
        });
        let projection_loading = projection_view.is_none_or(|view| {
            view.data.is_none()
                && matches!(
                    view.freshness,
                    ProjectionFreshness::Loading | ProjectionFreshness::Refreshing
                )
        });
        let has_more = projection_view.is_some_and(|view| view.has_more);
        let loading_more = projection_view.is_some_and(|view| {
            view.data.is_some() && matches!(view.freshness, ProjectionFreshness::Refreshing)
        });
        enum Action {
            Open(String),
            Edit(Box<crate::resource_library_lifecycle::Resource>),
            Transition(i64, ResourceCurationState),
            Delete(i64, String),
            Retry(i64, String),
            LoadMore,
        }
        let mut action = None;
        let theme = ReaderTheme::sspai();
        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(theme.canvas)
                    .stroke(egui::Stroke::new(1.0, theme.border))
                    .inner_margin(egui::Margin::symmetric(24, 18)),
            )
            .show(root_ui, |ui| {
                if matches!(self.ui_state.panel(), Some(PanelState::ResourceEditor(_))) {
                    egui::Panel::right("resource-editor")
                        .resizable(true)
                        .default_size(480.0)
                        .size_range(360.0..=720.0)
                        .show(ui, |ui| {
                            ui.set_min_width(ui.available_width());
                            egui::ScrollArea::vertical()
                                .id_salt("resource-editor-scroll")
                                .auto_shrink([false, false])
                                .show(ui, |ui| {
                                    self.show_resource_editor(ui);
                                });
                        });
                }
                ui.label(
                    egui::RichText::new(format!("资源库 · {}", rows.len()))
                        .size(22.0)
                        .color(theme.text)
                        .family(egui::FontFamily::Name("cjk-bold".into())),
                );
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.button("＋ 添加网站").clicked() {
                        self.open_modal(ModalState::AddResource(
                            resource_feature::AddDraft::default(),
                        ));
                    }
                    if ui.button("导入网页收藏").clicked() {
                        let dependencies = resource_feature::Dependencies {
                            db: &self.db,
                            processing_handoff: &self.knowledge_engine,
                            clock: &SystemClock,
                        };
                        match resource_feature::ImportDraft::prepare(&dependencies) {
                            Ok(draft) => {
                                self.open_modal(ModalState::ImportResources(draft));
                            }
                            Err(error) => self.notice(format!("读取网页收藏失败：{error}")),
                        }
                    }
                    ui.add(
                        egui::TextEdit::singleline(self.search_feature.resource_query_mut())
                            .hint_text("搜索标题、URL、用途或备注")
                            .desired_width(280.0),
                    );
                    if ui.button("搜索资源和文章").clicked() {
                        let query = self.search_feature.resource_query().trim().to_owned();
                        self.search_feature
                            .start_resource_search(query, ui.ctx(), &self.db_path);
                    }
                });
                ui.horizontal_wrapped(|ui| {
                    for (filter, label) in [
                        (
                            ResourceFilter::Active,
                            format!("我的资源 {}", counts.active),
                        ),
                        (
                            ResourceFilter::PendingReview,
                            format!("等待确认 {}", counts.pending_review),
                        ),
                        (ResourceFilter::Broken, format!("失效 {}", counts.broken)),
                        (
                            ResourceFilter::Archived,
                            format!("归档 {}", counts.archived),
                        ),
                    ] {
                        if ui
                            .selectable_label(self.resource_filter == filter, label)
                            .clicked()
                        {
                            self.resource_filter = filter;
                        }
                    }
                });
                if let Some(error) = self.search_feature.resource_error() {
                    ui.colored_label(egui::Color32::RED, format!("搜索失败：{error}"));
                }
                if let Some(error) = &projection_error {
                    ui.colored_label(egui::Color32::RED, format!("读取资源失败：{error}"));
                    if ui.button("重试读取").clicked() {
                        self.desktop_projection
                            .accept(DesktopProjectionFact::RetryResource(demand));
                    }
                }
                if projection_loading {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.weak("正在读取资源…");
                    });
                }
                if !self.search_feature.resource_query().trim().is_empty() {
                    if self.search_feature.resource_searching() {
                        ui.horizontal(|ui| {
                            ui.add(egui::Spinner::new());
                            ui.label("正在搜索资料库…");
                        });
                    }
                    ui.weak(format!(
                        "与 CLI 相同的统一搜索结果：{} 条",
                        self.search_feature.resource_results().len()
                    ));
                    for result in self.search_feature.resource_results() {
                        let kind = match result.primary {
                            PrimaryIdentity::Resource(_) => "网站资源",
                            PrimaryIdentity::Article(_) => "收藏文章",
                        };
                        ui.horizontal_wrapped(|ui| {
                            ui.label(format!("[{kind}]"));
                            ui.strong(result.title.as_deref().unwrap_or("未命名"));
                            if let Some(url) = result.url.as_deref() {
                                ui.hyperlink_to("打开", url);
                            }
                            if let Some(evidence) = result.evidence.first() {
                                ui.weak(&evidence.text);
                            }
                        });
                    }
                    ui.separator();
                }
                ui.weak(match self.resource_filter {
                    ResourceFilter::Active => "这些网站已经确认，AI 搜索资源时会返回它们。",
                    ResourceFilter::PendingReview => {
                        "通过 CLI 添加的网站先放在这里。确认收藏后，AI 才能在默认搜索中找到。"
                    }
                    ResourceFilter::Broken => "抓取失败或网址失效的资源，可以重试或归档。",
                    ResourceFilter::Archived => "暂时不用的资源，不会出现在 AI 默认搜索结果中。",
                });
                ui.separator();
                egui::ScrollArea::vertical().show(ui, |ui| {
                    let columns = if ui.available_width() >= 1_040.0 {
                        3
                    } else if ui.available_width() >= 680.0 {
                        2
                    } else {
                        1
                    };
                    ui.columns(columns, |columns| {
                        for (index, resource) in rows.iter().enumerate() {
                            let column = &mut columns[index % columns.len()];
                            resource_card(column, |ui| {
                                let domain = resource_domain(&resource.url);
                                egui::Frame::new()
                                    .fill(theme.code_bg)
                                    .corner_radius(egui::CornerRadius::same(6))
                                    .inner_margin(egui::Margin::symmetric(8, 6))
                                    .show(ui, |ui| {
                                        ui.horizontal(|ui| {
                                            ui.add(RemixIcon::Resources.image(
                                                false,
                                                theme.muted,
                                                18.0,
                                            ));
                                            ui.add(
                                                egui::Label::new(
                                                    egui::RichText::new(&domain)
                                                        .size(12.5)
                                                        .color(theme.muted),
                                                )
                                                .truncate(),
                                            );
                                            ui.weak("·");
                                            ui.add(
                                                egui::Label::new(
                                                    egui::RichText::new(match resource.kind {
                                                        crate::resource_library_lifecycle::ResourceKind::Site => "网站",
                                                        crate::resource_library_lifecycle::ResourceKind::Page => "网页",
                                                        crate::resource_library_lifecycle::ResourceKind::Article => "文章",
                                                    })
                                                    .size(12.5)
                                                    .color(theme.subtle),
                                                )
                                                .truncate(),
                                            );
                                        });
                                    });
                                ui.add_space(6.0);
                                ui.add(
                                    egui::Label::new(
                                        egui::RichText::new(
                                            resource.title.as_deref().unwrap_or("尚未整理的网站"),
                                        )
                                        .size(15.5)
                                        .strong()
                                        .color(theme.text),
                                    )
                                    .truncate(),
                                );
                                ui.hyperlink_to(
                                    egui::RichText::new(&domain)
                                        .size(12.5)
                                        .color(theme.link),
                                    &resource.url,
                                );
                                if let Some(purpose) = &resource.purpose_zh {
                                    ui.add(
                                        egui::Label::new(purpose)
                                            .truncate()
                                            .sense(egui::Sense::hover()),
                                    );
                                }
                                if let Some(note) = &resource.private_note {
                                    ui.add(
                                        egui::Label::new(
                                            egui::RichText::new(format!("备注：{note}"))
                                                .color(theme.subtle),
                                        )
                                        .truncate()
                                        .sense(egui::Sense::hover()),
                                    );
                                }
                                ui.add_space(2.0);
                                ui.horizontal_wrapped(|ui| {
                                    let status = match (resource.curation_state, resource.health) {
                                        (ResourceCurationState::PendingReview, _) => "待确认",
                                        (ResourceCurationState::Archived, _) => "已归档",
                                        (_, ResourceHealth::Broken) => "源站失效",
                                        (_, ResourceHealth::Unknown) => "待检查",
                                        (_, ResourceHealth::Healthy) => "可供 AI 搜索",
                                    };
                                    ui.label(
                                        egui::RichText::new(status)
                                            .size(12.5)
                                            .color(theme.accent),
                                    );
                                    ui.weak("·");
                                    ui.label(
                                        egui::RichText::new(if resource.privacy
                                            == ResourcePrivacy::Private
                                        {
                                            "私密"
                                        } else {
                                            "公开"
                                        })
                                        .size(12.5)
                                        .color(theme.subtle),
                                    );
                                    if let Some(rating) = resource.manual_rating {
                                        ui.weak("·");
                                        ui.label(
                                            egui::RichText::new(format!("评分 {rating}/5"))
                                                .size(12.5)
                                                .color(theme.subtle),
                                        );
                                    }
                                });
                                ui.add_space(4.0);
                                ui.scope(|ui| {
                                    ui.spacing_mut().button_padding = egui::vec2(5.0, 3.0);
                                    ui.spacing_mut().item_spacing.x = 6.0;
                                    ui.horizontal_wrapped(|ui| {
                                    if ui.small_button("访问").clicked() {
                                        action = Some(Action::Open(resource.url.clone()));
                                    }
                                    if ui.small_button("编辑").clicked() {
                                        action = Some(Action::Edit(Box::new(resource.clone())));
                                    }
                                    match resource.curation_state {
                                        ResourceCurationState::PendingReview => {
                                            if ui.small_button("确认收藏").clicked() {
                                                action = Some(Action::Transition(
                                                    resource.id,
                                                    ResourceCurationState::Active,
                                                ));
                                            }
                                        }
                                        ResourceCurationState::Active => {
                                            if ui.small_button("归档").clicked() {
                                                action = Some(Action::Transition(
                                                    resource.id,
                                                    ResourceCurationState::Archived,
                                                ));
                                            }
                                        }
                                        ResourceCurationState::Archived => {
                                            if ui.small_button("恢复").clicked() {
                                                action = Some(Action::Transition(
                                                    resource.id,
                                                    ResourceCurationState::Active,
                                                ));
                                            }
                                        }
                                    }
                                    if resource.privacy == ResourcePrivacy::Public
                                        && resource.curation_state
                                            != ResourceCurationState::Archived
                                        && ui
                                            .small_button(if resource.purpose_zh.is_none() {
                                                "补全描述"
                                            } else {
                                                "重新补全"
                                            })
                                            .clicked()
                                    {
                                        action = Some(Action::Retry(
                                            resource.id,
                                            resource.url.clone(),
                                        ));
                                    }
                                    if matches!(
                                        resource.curation_state,
                                        ResourceCurationState::PendingReview
                                            | ResourceCurationState::Archived
                                    ) && ui.small_button("删除").clicked()
                                    {
                                        action = Some(Action::Delete(
                                            resource.id,
                                            resource.title.clone().unwrap_or_else(|| {
                                                format!("Resource #{}", resource.id)
                                            }),
                                        ));
                                    }
                                    });
                                });
                            });
                            column.add_space(10.0);
                        }
                    });
                    if has_more {
                        ui.horizontal(|ui| {
                            if loading_more {
                                ui.spinner();
                                ui.weak("正在加载更多资源…");
                            } else if ui.button("加载更多").clicked() {
                                action = Some(Action::LoadMore);
                            }
                        });
                    }
                    if rows.is_empty() && !projection_loading {
                        ui.weak(
                            "这里还没有资源。可以先添加常用网站，例如图标、设计素材或开发工具站。",
                        );
                    }
                });
            });
        match action {
            Some(Action::Open(url)) => {
                let _ = open::that(url);
            }
            Some(Action::Edit(resource)) => {
                let panel = PanelState::ResourceEditor(resource_feature::EditorDraft::open(
                    resource.id,
                    Some(&resource),
                ));
                let effects = self.ui_state.reduce(UiAction::SetPanel(Some(panel)));
                self.apply_ui_effects(effects);
            }
            Some(Action::Transition(id, status)) => {
                let dependencies = resource_feature::Dependencies {
                    db: &self.db,
                    processing_handoff: &self.knowledge_engine,
                    clock: &SystemClock,
                };
                let outcome =
                    resource_feature::transition_curation(id, status, collection, &dependencies);
                self.apply_resource_feature_outcome(outcome, &ctx);
            }
            Some(Action::Delete(id, title)) => {
                self.open_modal(ModalState::DeleteResource(
                    resource_feature::DeleteDraft::new(id, title),
                ));
            }
            Some(Action::Retry(id, url)) => {
                let _ = url;
                match self.knowledge_feature.request_resource_completion(
                    id,
                    &self.knowledge_engine,
                    &ctx,
                ) {
                    Ok(()) => self.notice("已重新加入后台处理队列"),
                    Err(error) => self.notice(format!("无法重试后台任务：{error:#}")),
                }
            }
            Some(Action::LoadMore) => {
                self.desktop_projection
                    .accept(DesktopProjectionFact::LoadMoreResources(collection));
            }
            None => {}
        }
    }

    fn show_dashboard_page(&mut self, root_ui: &mut egui::Ui) {
        if self.ui_state.route() != Route::Dashboard {
            return;
        }

        use crate::resource_library_lifecycle::ResourceCollection;

        let theme = ReaderTheme::sspai();
        let article_projection = self.article_projection(ProjectionScope::ArticleBookmarks);
        let unread_projection = self.article_projection(ProjectionScope::Unread);
        let (bookmarks, read_later, unread) = article_projection
            .as_deref()
            .map(|projection| {
                let unread_by_feed = feed_unread_index(&projection.feed_unread);
                let unread = self
                    .feeds
                    .iter()
                    .filter(|feed| !feed.disabled)
                    .filter_map(|feed| unread_by_feed.get(&feed.id))
                    .sum();
                (
                    projection.counts.bookmarks,
                    projection.counts.read_later,
                    unread,
                )
            })
            .unwrap_or_default();
        let unread_articles = unread_projection
            .as_deref()
            .map(|projection| projection.articles.as_slice())
            .unwrap_or_default();
        let excerpts = self
            .excerpt_projection(ExcerptProjectionScope::Library)
            .as_deref()
            .map_or(0, |projection| projection.counts.library_excerpts);
        let resources = self
            .desktop_projection_frame
            .resource(ResourceProjectionDemand::Collection(
                ResourceCollection::Active,
            ))
            .and_then(|view| view.data.as_deref())
            .map_or(0, |projection| projection.counts.active);
        let failed_feeds = self
            .feeds
            .iter()
            .filter(|feed| feed.last_error.is_some())
            .count();
        // The dashboard metric aggregates every enabled feed, so it must open
        // the all-articles collection rather than silently narrowing to the
        // first source in the sidebar.
        let unread_route = Some(Route::Articles(ArticleCollection::Feed(None)));
        let failed_route = self
            .feeds
            .iter()
            .find(|feed| feed.last_error.is_some())
            .map(|feed| Route::Articles(ArticleCollection::Feed(Some(feed.id))))
            .or(Some(Route::default()));

        let article_ready = article_projection.is_some();
        let unread_ready = unread_projection.is_some();
        let resources_ready = self
            .desktop_projection_frame
            .resource(ResourceProjectionDemand::Collection(
                ResourceCollection::Active,
            ))
            .is_some_and(|view| view.data.is_some());
        let excerpts_ready = self
            .excerpt_projection(ExcerptProjectionScope::Library)
            .is_some();
        let refresh_snapshot = self.rss_refresh.snapshot();
        let refresh_busy = refresh_snapshot.current.is_some();
        let continue_article = self
            .last_opened_article_id
            .and_then(|id| self.article_projection(ProjectionScope::Article(id)))
            .and_then(|projection| projection.articles.first().cloned())
            .or_else(|| unread_articles.first().cloned());
        let mut action: Option<DashboardAction> = None;

        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(theme.canvas)
                    .stroke(egui::Stroke::new(1.0, theme.border))
                    .inner_margin(egui::Margin::symmetric(30, 24)),
            )
            .show(root_ui, |ui| {
                ui.label(
                    egui::RichText::new("总览")
                        .size(26.0)
                        .color(theme.text)
                        .family(egui::FontFamily::Name("cjk-bold".into())),
                );
                ui.add_space(5.0);
                ui.label(
                    egui::RichText::new("从这里继续阅读，或快速找回保存的资料。")
                        .size(14.0)
                        .color(theme.muted),
                );
                ui.horizontal(|ui| {
                    ui.add_space(1.0);
                    if ui
                        .add_enabled(
                            !refresh_busy,
                            egui::Button::new(if refresh_busy {
                                "刷新中…"
                            } else {
                                "刷新全部订阅"
                            }),
                        )
                        .clicked()
                    {
                        action = Some(DashboardAction::RefreshAll);
                    }
                    if refresh_busy {
                        if let Some(run) = &refresh_snapshot.current {
                            ui.label(
                                egui::RichText::new(format!(
                                    "正在刷新 {}/{} · 失败 {} · 新增 {}",
                                    run.completed_count,
                                    run.target_count,
                                    run.failed_feed_count,
                                    run.new_article_count
                                ))
                                .size(13.0)
                                .color(theme.muted),
                            );
                        }
                    } else if let Some(run) = &refresh_snapshot.last_completed {
                        let status = match run.status {
                            RefreshRunStatus::Succeeded => "上次刷新成功".to_owned(),
                            RefreshRunStatus::Degraded => {
                                format!("上次刷新有 {} 个订阅失败", run.failed_feed_count)
                            }
                            RefreshRunStatus::Failed => "上次刷新失败，请重试".to_owned(),
                            RefreshRunStatus::Interrupted => "上次刷新被中断".to_owned(),
                            RefreshRunStatus::Fetching | RefreshRunStatus::Committing => {
                                "刷新状态未知".to_owned()
                            }
                        };
                        ui.label(egui::RichText::new(status).size(13.0).color(theme.muted));
                        let failed_ids = run
                            .feeds
                            .iter()
                            .filter(|feed| feed.failure.is_some())
                            .map(|feed| feed.feed_id)
                            .collect::<Vec<_>>();
                        if !failed_ids.is_empty() && ui.small_button("重试失败订阅").clicked()
                        {
                            action = Some(DashboardAction::RetryFeeds(failed_ids));
                        }
                    }
                });
                ui.add_space(20.0);

                egui::Frame::new()
                    .fill(theme.panel)
                    .stroke(egui::Stroke::new(1.0, theme.border))
                    .corner_radius(egui::CornerRadius::same(10))
                    .inner_margin(egui::Margin::symmetric(16, 13))
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.label(egui::RichText::new("继续阅读").size(17.0).color(theme.text));
                            if let Some(article) = &continue_article {
                                let position = self
                                    .reading_positions
                                    .get(&article.id)
                                    .copied()
                                    .unwrap_or(0.0);
                                ui.label(
                                    egui::RichText::new(if position > 0.0 {
                                        format!("已保存阅读位置 · {:.0}px", position)
                                    } else {
                                        "从顶部开始".to_owned()
                                    })
                                    .size(13.0)
                                    .color(theme.muted),
                                );
                            }
                        });
                        if let Some(article) = &continue_article {
                            let title = article
                                .title
                                .as_deref()
                                .filter(|title| !title.trim().is_empty())
                                .unwrap_or("无标题文章");
                            let feed_title = self
                                .feeds
                                .iter()
                                .find(|feed| feed.id == article.feed_id)
                                .and_then(|feed| feed.title.as_deref())
                                .unwrap_or("订阅文章");
                            ui.label(egui::RichText::new(title).size(16.0).color(theme.text));
                            ui.label(
                                egui::RichText::new(feed_title)
                                    .size(13.0)
                                    .color(theme.muted),
                            );
                            let saved = article_projection.as_deref().is_some_and(|projection| {
                                projection.fixed_bookmark_ids.contains(&article.id)
                            });
                            if ui.button("继续阅读 →").clicked() {
                                action = Some(DashboardAction::OpenArticle {
                                    article_id: article.id,
                                    feed_id: article.feed_id,
                                    saved,
                                });
                            }
                        } else {
                            ui.label(
                                egui::RichText::new(
                                    "还没有阅读记录；打开一篇未读文章后，它会出现在这里。",
                                )
                                .size(14.0)
                                .color(theme.muted),
                            );
                        }
                    });

                ui.add_space(18.0);

                ui.horizontal_wrapped(|ui| {
                    dashboard_metric(
                        ui,
                        "未读文章",
                        unread,
                        theme.accent,
                        article_ready,
                        unread_route,
                        &mut action,
                    );
                    dashboard_metric(
                        ui,
                        "稍后读",
                        read_later,
                        theme.link,
                        article_ready,
                        Some(Route::Articles(ArticleCollection::ReadLater)),
                        &mut action,
                    );
                    dashboard_metric(
                        ui,
                        "文章收藏",
                        bookmarks,
                        theme.accent,
                        article_ready,
                        Some(Route::Articles(ArticleCollection::Saved)),
                        &mut action,
                    );
                    dashboard_metric(
                        ui,
                        "摘录与想法",
                        excerpts,
                        theme.accent,
                        excerpts_ready,
                        Some(Route::Excerpts),
                        &mut action,
                    );
                    dashboard_metric(
                        ui,
                        "资源",
                        resources,
                        theme.accent,
                        resources_ready,
                        Some(Route::Resources),
                        &mut action,
                    );
                    dashboard_metric(
                        ui,
                        "刷新失败",
                        failed_feeds,
                        theme.accent,
                        true,
                        failed_route,
                        &mut action,
                    );
                });

                ui.add_space(18.0);
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("最近未读").size(17.0).color(theme.text));
                    ui.label(
                        egui::RichText::new(format!("{} 篇", unread_articles.len()))
                            .size(13.0)
                            .color(theme.muted),
                    );
                    if ui.small_button("查看全部 →").clicked() {
                        action = Some(DashboardAction::Navigate(Route::Articles(
                            ArticleCollection::Feed(None),
                        )));
                    }
                });
                if !unread_ready {
                    ui.label(egui::RichText::new("正在加载未读文章…").color(theme.muted));
                } else if unread_articles.is_empty() {
                    ui.label(egui::RichText::new("已读完，暂时没有未读文章。").color(theme.muted));
                } else {
                    for article in unread_articles.iter().take(8) {
                        let title = article
                            .title
                            .as_deref()
                            .filter(|title| !title.trim().is_empty())
                            .unwrap_or("无标题文章");
                        let feed_title = self
                            .feeds
                            .iter()
                            .find(|feed| feed.id == article.feed_id)
                            .and_then(|feed| feed.title.as_deref())
                            .unwrap_or("订阅文章");
                        let response = ui.add(
                            egui::Button::new(
                                egui::RichText::new(format!("{title}  ·  {feed_title}"))
                                    .size(14.0)
                                    .color(theme.text),
                            )
                            .fill(egui::Color32::TRANSPARENT)
                            .stroke(egui::Stroke::NONE),
                        );
                        if response.clicked() {
                            action = Some(DashboardAction::OpenArticle {
                                article_id: article.id,
                                feed_id: article.feed_id,
                                saved: false,
                            });
                        }
                    }
                }

                ui.add_space(26.0);
                ui.separator();
                ui.add_space(18.0);
                ui.label(
                    egui::RichText::new("准备开始")
                        .size(17.0)
                        .color(theme.text)
                        .family(egui::FontFamily::Name("cjk-bold".into())),
                );
                ui.add_space(8.0);
                ui.label(
                    egui::RichText::new(
                        "打开左侧订阅源即可浏览文章；稍后读、收藏和资源库都可以从这里直接进入。",
                    )
                    .size(14.0)
                    .color(theme.muted),
                );
            });

        match action {
            Some(DashboardAction::Navigate(route)) => {
                self.navigate(route);
                self.clear_selection_popover();
            }
            Some(DashboardAction::OpenArticle {
                article_id,
                feed_id,
                saved,
            }) => {
                if saved {
                    self.select_saved_articles();
                } else {
                    self.select_feed(feed_id);
                }
                self.select_article(article_id);
            }
            Some(DashboardAction::RefreshAll) => match self.rss_refresh.request_all() {
                Ok(()) => self.notice("已开始刷新全部订阅"),
                Err(error) => self.notice(format!("无法启动订阅刷新：{error}")),
            },
            Some(DashboardAction::RetryFeeds(feed_ids)) => {
                let mut failed = 0;
                for feed_id in feed_ids {
                    if self.rss_refresh.request_feed(feed_id).is_err() {
                        failed += 1;
                    }
                }
                if failed == 0 {
                    self.notice("已重试失败订阅");
                } else {
                    self.notice(format!("有 {failed} 个订阅无法启动重试"));
                }
            }
            None => {}
        }
    }

    fn show_resource_modal(&mut self, ctx: &egui::Context) {
        let show_discard = self.ui_state.discard_owner() == Some(DiscardOwner::Modal);
        let dependencies = resource_feature::Dependencies {
            db: &self.db,
            processing_handoff: &self.knowledge_engine,
            clock: &crate::resource_library_lifecycle::SystemClock,
        };
        let outcome = match self.ui_state.modal_mut() {
            Some(ModalState::AddResource(draft)) => resource_feature::show_modal(
                ctx,
                resource_feature::ModalDraft::Add(draft),
                show_discard,
                &dependencies,
            ),
            Some(ModalState::DeleteResource(draft)) => resource_feature::show_modal(
                ctx,
                resource_feature::ModalDraft::Delete(draft),
                show_discard,
                &dependencies,
            ),
            Some(ModalState::ImportResources(draft)) => resource_feature::show_modal(
                ctx,
                resource_feature::ModalDraft::Import(draft),
                show_discard,
                &dependencies,
            ),
            _ => return,
        };
        self.apply_resource_feature_outcome(outcome, ctx);
    }

    fn show_resource_editor(&mut self, ui: &mut egui::Ui) {
        let Some(resource_id) = self.ui_state.panel().and_then(|panel| match panel {
            PanelState::ResourceEditor(draft) => Some(draft.id()),
            _ => None,
        }) else {
            return;
        };
        let show_discard = self.ui_state.discard_owner() == Some(DiscardOwner::Panel);
        let task = self.knowledge_task(KnowledgeTaskKind::ResourceCompletion, resource_id);
        let detail = self
            .desktop_projection_frame
            .resource(ResourceProjectionDemand::Detail(resource_id))
            .and_then(|view| view.data.as_ref())
            .and_then(|projection| projection.detail.clone());
        let dependencies = resource_feature::Dependencies {
            db: &self.db,
            processing_handoff: &self.knowledge_engine,
            clock: &crate::resource_library_lifecycle::SystemClock,
        };
        let outcome = match self.ui_state.panel_mut() {
            Some(PanelState::ResourceEditor(draft)) => resource_feature::show_panel(
                ui,
                draft,
                detail.as_ref(),
                task.as_ref(),
                show_discard,
                &dependencies,
            ),
            _ => return,
        };
        let context = ui.ctx().clone();
        self.apply_resource_feature_outcome(outcome, &context);
    }
    fn show_feed_settings_panel(&mut self, ui: &mut egui::Ui) {
        let show_discard = self.ui_state.discard_owner() == Some(DiscardOwner::Panel);
        let outcome = {
            let Some(PanelState::FeedSettings(draft)) = self.ui_state.panel_mut() else {
                return;
            };
            let dependencies = feed_subscription_feature::Dependencies {
                database: self.db_path.as_path(),
                refresh: &self.rss_refresh,
            };
            feed_subscription_feature::show_panel(ui, draft, show_discard, &dependencies)
        };
        self.apply_feed_feature_outcome(outcome);
    }
    fn show_saved_library_page(&mut self, root_ui: &mut egui::Ui) {
        if self.ui_state.route() != Route::Excerpts {
            return;
        }
        let theme = ReaderTheme::sspai();
        let projection = self.excerpt_projection(ExcerptProjectionScope::Library);
        let loading = projection.is_none()
            && self
                .desktop_projection_frame
                .excerpt(ExcerptProjectionScope::Library)
                .is_some_and(|view| {
                    matches!(
                        view.freshness,
                        ProjectionFreshness::Loading | ProjectionFreshness::Refreshing
                    )
                });
        let rows = projection
            .as_ref()
            .map(|projection| projection.excerpts.clone())
            .unwrap_or_default();
        let thought_count = projection
            .as_ref()
            .map_or(0, |projection| projection.counts.library_thoughts);
        let mut open_article: Option<ExcerptView> = None;
        let mut edit_thought: Option<ExcerptView> = None;
        let mut remove_thought = None;
        let mut delete_excerpt: Option<ExcerptView> = None;

        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(theme.canvas)
                    .stroke(egui::Stroke::new(1.0, theme.border))
                    .inner_margin(egui::Margin::symmetric(24, 18)),
            )
            .show(root_ui, |ui| {
                ui.label(
                    egui::RichText::new(format!("摘录与想法 · {}", rows.len()))
                        .size(22.0)
                        .color(theme.text)
                        .family(egui::FontFamily::Name("cjk-bold".into())),
                );
                ui.add_space(4.0);
                ui.label(
                    egui::RichText::new(format!(
                        "摘录用于保留原文片段；其中 {thought_count} 条附有个人想法。"
                    ))
                        .size(15.0)
                        .color(theme.muted),
                );
                ui.add_space(8.0);
                ui.separator();
                ui.add_space(6.0);

                if loading {
                    ui.vertical_centered(|ui| {
                        ui.add_space(80.0);
                        ui.label(egui::RichText::new("正在加载摘录与想法…").color(theme.muted));
                    });
                    return;
                }

                if rows.is_empty() {
                    ui.vertical_centered(|ui| {
                        ui.add_space(80.0);
                        ui.label(
                            egui::RichText::new("还没有摘录或想法")
                                .size(20.0)
                                .color(theme.text)
                                .family(egui::FontFamily::Name("cjk-bold".into())),
                        );
                        ui.add_space(8.0);
                        ui.label(
                            egui::RichText::new("在正文中选中文字，然后点击“摘录”或“写想法”。")
                                .size(15.0)
                                .color(theme.muted),
                        );
                    });
                    return;
                }

                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        for excerpt in &rows {
                            egui::Frame::new()
                                .fill(theme.code_bg)
                                .stroke(egui::Stroke::new(1.0, theme.border))
                                .corner_radius(egui::CornerRadius::same(7))
                                .inner_margin(egui::Margin::symmetric(14, 12))
                                .show(ui, |ui| {
                                    ui.set_width(ui.available_width());
                                    ui.horizontal(|ui| {
                                        ui.label(
                                            egui::RichText::new("★ 摘录")
                                                .size(13.0)
                                                .color(theme.accent),
                                        );
                                        if excerpt.thought.is_some() {
                                            ui.label(
                                                egui::RichText::new("✎ 想法")
                                                    .size(13.0)
                                                    .color(theme.link),
                                            );
                                        }
                                        if excerpt.identity_kind == ExcerptIdentityKind::Legacy {
                                            ui.label(
                                                egui::RichText::new("历史记录")
                                                    .size(13.0)
                                                    .color(theme.muted),
                                            );
                                        }
                                        if matches!(
                                            excerpt.resolution,
                                            crate::excerpt_thought_lifecycle::ExcerptResolution::Unresolved
                                        ) {
                                            ui.label(
                                                egui::RichText::new("未定位")
                                                    .size(13.0)
                                                    .color(theme.muted),
                                            );
                                        }
                                        ui.with_layout(
                                            egui::Layout::right_to_left(egui::Align::Center),
                                            |ui| {
                                                ui.label(
                                                    egui::RichText::new(format_timestamp(
                                                        excerpt.updated_at,
                                                    ))
                                                    .size(13.0)
                                                    .color(theme.muted),
                                                );
                                            },
                                        );
                                    });
                                    ui.add_space(7.0);
                                    ui.add(
                                        egui::Label::new(
                                            egui::RichText::new(format!(
                                                "“{}”",
                                                excerpt.selected_text
                                            ))
                                            .size(17.0)
                                            .color(theme.text),
                                        )
                                        .wrap(),
                                    );
                                    if let Some(thought) = &excerpt.thought {
                                        ui.add_space(9.0);
                                        egui::Frame::new()
                                            .fill(theme.selected_bg)
                                            .corner_radius(egui::CornerRadius::same(5))
                                            .inner_margin(egui::Margin::symmetric(10, 8))
                                            .show(ui, |ui| {
                                                ui.add(
                                                    egui::Label::new(
                                                        egui::RichText::new(&thought.content)
                                                            .size(15.0)
                                                            .color(theme.text),
                                                    )
                                                    .wrap(),
                                                );
                                            });
                                    }
                                    ui.add_space(9.0);
                                    ui.horizontal(|ui| {
                                        let title = excerpt
                                            .source
                                            .title
                                            .as_deref()
                                            .filter(|title| !title.trim().is_empty())
                                            .unwrap_or("未命名文章");
                                        ui.add(
                                            egui::Label::new(
                                                egui::RichText::new(title)
                                                    .size(13.0)
                                                    .color(theme.muted),
                                            )
                                            .truncate(),
                                        );
                                        ui.with_layout(
                                            egui::Layout::right_to_left(egui::Align::Center),
                                            |ui| {
                                                if ui
                                                    .add(
                                                        egui::Button::new(
                                                            egui::RichText::new("删除摘录")
                                                                .size(13.0)
                                                                .color(theme.muted),
                                                        )
                                                        .stroke(egui::Stroke::NONE),
                                                    )
                                                    .clicked()
                                                {
                                                    delete_excerpt = Some(excerpt.clone());
                                                }
                                                if excerpt.thought.is_some()
                                                    && ui
                                                        .add(
                                                            egui::Button::new(
                                                                egui::RichText::new("删除想法")
                                                                    .size(13.0)
                                                                    .color(theme.muted),
                                                            )
                                                            .stroke(egui::Stroke::NONE),
                                                        )
                                                        .clicked()
                                                {
                                                    remove_thought = Some(excerpt.id);
                                                }
                                                if ui
                                                    .add(
                                                        egui::Button::new(
                                                            egui::RichText::new(if excerpt.thought.is_some() {
                                                                "编辑想法"
                                                            } else {
                                                                "写想法"
                                                            })
                                                            .size(13.0)
                                                            .color(theme.link),
                                                        )
                                                        .stroke(egui::Stroke::NONE),
                                                    )
                                                    .clicked()
                                                {
                                                    edit_thought = Some(excerpt.clone());
                                                }
                                                if ui
                                                    .add(
                                                        egui::Button::new(
                                                            egui::RichText::new("打开文章 ↗")
                                                                .size(13.0)
                                                                .color(theme.link),
                                                        )
                                                        .stroke(egui::Stroke::NONE),
                                                    )
                                                    .clicked()
                                                {
                                                    open_article = Some(excerpt.clone());
                                                }
                                            },
                                        );
                                    });
                                });
                            ui.add_space(10.0);
                        }
                    });
            });

        if let Some(excerpt) = edit_thought {
            self.begin_edit_thought(&excerpt);
        } else if let Some(excerpt_id) = remove_thought {
            self.remove_thought(excerpt_id, ExcerptProjectionScope::Library);
        } else if let Some(excerpt) = delete_excerpt {
            self.request_delete_excerpt(&excerpt, ExcerptProjectionScope::Library);
        }
        if let Some(excerpt) = open_article {
            if excerpt.source.origin == ExcerptArticleOrigin::WebClipping {
                self.select_saved_articles();
            } else {
                self.select_feed(excerpt.source.feed_id);
            }
            self.select_article(excerpt.article_id);
            self.pending_selection_anchor = Some(excerpt.as_article_selection());
            self.notice("已打开原文章，正在定位摘录");
        }
    }

    fn show_archive_library_page(&mut self, root_ui: &mut egui::Ui) {
        if self.ui_state.route() != Route::Archive {
            return;
        }

        let theme = ReaderTheme::sspai();
        let projection = self.article_projection(ProjectionScope::Archive);
        let articles = projection
            .as_deref()
            .map(|projection| projection.articles.as_slice())
            .unwrap_or_default();
        let loading = projection.is_none()
            && matches!(
                self.current_article_freshness(),
                Some(ProjectionFreshness::Loading | ProjectionFreshness::Refreshing)
            );
        let mut restore_article = None;

        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(theme.canvas)
                    .stroke(egui::Stroke::new(1.0, theme.border))
                    .inner_margin(egui::Margin::symmetric(24, 18)),
            )
            .show(root_ui, |ui| {
                ui.label(
                    egui::RichText::new(format!("已归档文章 · {}", articles.len()))
                        .size(22.0)
                        .color(theme.text)
                        .family(egui::FontFamily::Name("cjk-bold".into())),
                );
                ui.add_space(4.0);
                ui.label(
                    egui::RichText::new(
                        "归档文章不会出现在订阅列表中，刷新同一订阅源也不会恢复它。",
                    )
                    .size(15.0)
                    .color(theme.muted),
                );
                ui.add_space(8.0);
                ui.separator();
                ui.add_space(6.0);

                if loading {
                    ui.vertical_centered(|ui| {
                        ui.add_space(80.0);
                        ui.label(egui::RichText::new("正在加载归档文章…").color(theme.muted));
                    });
                    return;
                }
                if articles.is_empty() {
                    ui.vertical_centered(|ui| {
                        ui.add_space(80.0);
                        ui.label(
                            egui::RichText::new("还没有归档文章")
                                .size(20.0)
                                .color(theme.text)
                                .family(egui::FontFamily::Name("cjk-bold".into())),
                        );
                        ui.add_space(8.0);
                        ui.label(
                            egui::RichText::new("在文章列表中右键一篇文章即可归档。")
                                .size(15.0)
                                .color(theme.muted),
                        );
                    });
                    return;
                }

                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        for article in articles {
                            egui::Frame::new()
                                .fill(theme.code_bg)
                                .stroke(egui::Stroke::new(1.0, theme.border))
                                .corner_radius(egui::CornerRadius::same(7))
                                .inner_margin(egui::Margin::symmetric(14, 11))
                                .show(ui, |ui| {
                                    ui.set_width(ui.available_width());
                                    ui.add(
                                        egui::Label::new(
                                            egui::RichText::new(
                                                article
                                                    .title
                                                    .as_deref()
                                                    .filter(|title| !title.trim().is_empty())
                                                    .unwrap_or("未命名文章"),
                                            )
                                            .size(17.0)
                                            .color(theme.text)
                                            .family(egui::FontFamily::Name("cjk-bold".into())),
                                        )
                                        .wrap(),
                                    );
                                    ui.add_space(6.0);
                                    ui.horizontal(|ui| {
                                        let date =
                                            article.published.map(format_timestamp).unwrap_or_else(
                                                || format_timestamp(article.fetched_at),
                                            );
                                        ui.label(
                                            egui::RichText::new(date).size(13.0).color(theme.muted),
                                        );
                                        ui.with_layout(
                                            egui::Layout::right_to_left(egui::Align::Center),
                                            |ui| {
                                                if ui
                                                    .add(
                                                        egui::Button::new(
                                                            egui::RichText::new("恢复并打开")
                                                                .size(13.0)
                                                                .color(theme.link),
                                                        )
                                                        .stroke(egui::Stroke::NONE),
                                                    )
                                                    .clicked()
                                                {
                                                    restore_article =
                                                        Some((article.feed_id, article.id));
                                                }
                                            },
                                        );
                                    });
                                });
                            ui.add_space(10.0);
                        }
                    });
            });

        if let Some((feed_id, article_id)) = restore_article {
            match self.apply_article_library_change(ArticleLifecycleChange::SetArchived {
                article_id,
                target: false,
            }) {
                Ok(_) => {
                    self.select_feed(feed_id);
                    self.select_article(article_id);
                    self.notice("文章已恢复并打开");
                }
                Err(error) => self.report_article_library_failure("恢复文章", error),
            }
        }
    }

    fn show_search_window(&mut self, ctx: &egui::Context) {
        let db_path = self.db_path.clone();
        let db = &self.db;
        let search_feature = &mut self.search_feature;
        let Some(dialog) = (match self.ui_state.modal_mut() {
            Some(ModalState::Search(dialog)) => Some(dialog),
            _ => None,
        }) else {
            return;
        };
        let outcome = search_feature.show_modal(ctx, dialog, db, &db_path);
        self.apply_modal_host_action(outcome.modal_action);
        for notice in outcome.notices {
            self.notice(notice);
        }
        if let Some(hit) = outcome.selected_hit {
            self.open_search_result(&hit);
        }
    }

    fn show_active_modal(&mut self, ctx: &egui::Context) {
        match self.ui_state.modal_kind() {
            Some(ModalKind::AddFeed | ModalKind::DeleteFeed) => self.show_feed_dialogs(ctx),
            Some(ModalKind::Search) => self.show_search_window(ctx),
            Some(ModalKind::EditTags) => self.show_tag_dialog(ctx),
            Some(ModalKind::WriteThought | ModalKind::DeleteExcerpt) => {
                self.show_excerpt_thought_dialogs(ctx)
            }
            Some(ModalKind::SaveWebPage | ModalKind::DeleteWebPage) => {
                self.show_web_clipping_dialogs(ctx)
            }
            Some(
                ModalKind::AddResource | ModalKind::DeleteResource | ModalKind::ImportResources,
            ) => self.show_resource_modal(ctx),
            Some(ModalKind::RestoreBackup | ModalKind::ClearImages) => self.show_storage_modal(ctx),
            None => {}
        }
    }

    fn show_selection_notice(&mut self, ctx: &egui::Context) {
        let Some(notice) = self.ui_state.notice() else {
            return;
        };
        let message = notice.message.clone();
        let theme = ReaderTheme::sspai();
        let failed = message.contains("失败") || message.contains("错误");
        let timeout = if failed {
            Duration::from_secs(12)
        } else {
            Duration::from_secs(4)
        };
        if notice.created.elapsed() > timeout {
            self.ui_state.reduce(UiAction::ClearNotice);
            return;
        }
        let mut dismiss = false;
        egui::Area::new(egui::Id::new("selection-notice"))
            .anchor(egui::Align2::CENTER_TOP, egui::vec2(0.0, 24.0))
            .order(egui::Order::Foreground)
            .show(ctx, |ui| {
                egui::Frame::new()
                    .fill(theme.canvas)
                    .stroke(egui::Stroke::new(1.0, theme.border))
                    .corner_radius(egui::CornerRadius::same(8))
                    .inner_margin(egui::Margin::symmetric(13, 9))
                    .shadow(egui::Shadow {
                        offset: [0, 3],
                        blur: 10,
                        spread: 0,
                        color: egui::Color32::from_black_alpha(35),
                    })
                    .show(ui, |ui| {
                        ui.set_max_width(720.0);
                        ui.horizontal(|ui| {
                            ui.label(
                                egui::RichText::new(if failed { "!" } else { "✓" })
                                    .size(16.0)
                                    .color(if failed { theme.accent } else { theme.link })
                                    .family(egui::FontFamily::Name("cjk-bold".into())),
                            );
                            ui.add(
                                egui::Label::new(
                                    egui::RichText::new(message).size(15.0).color(theme.text),
                                )
                                .wrap(),
                            );
                            if ui
                                .add(
                                    egui::Button::new(
                                        egui::RichText::new("×").size(16.0).color(theme.muted),
                                    )
                                    .stroke(egui::Stroke::NONE),
                                )
                                .on_hover_text("关闭提示")
                                .clicked()
                            {
                                dismiss = true;
                            }
                        });
                    });
            });
        if dismiss {
            self.ui_state.reduce(UiAction::ClearNotice);
        }
        ctx.request_repaint_after(Duration::from_millis(100));
    }

    fn show_selection_popup(&mut self, ctx: &egui::Context) {
        let Some(popover) = self.ui_state.popover().cloned() else {
            return;
        };
        let Some(popup) = self.selection_popup_geometry.clone() else {
            self.clear_selection_popover();
            return;
        };
        if popup.generation != popover.generation {
            self.clear_selection_popover();
            return;
        }
        let quote = popover.quote;
        let mut action: Option<SelectionAction> = None;
        let mut open = true;
        const ALTERNATIVE_POSITIONS: &[egui::RectAlign] = &[
            egui::RectAlign::TOP_START,
            egui::RectAlign::TOP_END,
            egui::RectAlign::BOTTOM,
            egui::RectAlign::BOTTOM_START,
            egui::RectAlign::BOTTOM_END,
        ];

        egui::Popup::new(
            egui::Id::new(("article-selection-toolbar", popup.generation)),
            ctx.clone(),
            egui::PopupAnchor::ParentRect(popup.anchor_rect),
            popup.source_layer,
        )
        .open_bool(&mut open)
        .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
        .align(egui::RectAlign::TOP)
        .align_alternatives(ALTERNATIVE_POSITIONS)
        .gap(7.0)
        .width(202.0)
        .frame(
            egui::Frame::new()
                .fill(ReaderTheme::sspai().canvas)
                .stroke(egui::Stroke::new(1.0, ReaderTheme::sspai().border))
                .corner_radius(egui::CornerRadius::same(9))
                .inner_margin(egui::Margin::symmetric(7, 5))
                .shadow(egui::Shadow {
                    offset: [0, 3],
                    blur: 10,
                    spread: 0,
                    color: egui::Color32::from_black_alpha(38),
                }),
        )
        .show(|ui| {
            ui.spacing_mut().item_spacing.x = 0.0;
            ui.horizontal(|ui| {
                if selection_toolbar_button(ui, "▣", "复制") {
                    action = Some(SelectionAction::Copy);
                }
                if selection_toolbar_button(ui, "★", "摘录") {
                    action = Some(SelectionAction::Favorite);
                }
                if selection_toolbar_button(ui, "✎", "写想法") {
                    action = Some(SelectionAction::Comment);
                }
            });
        });

        if !open || action.is_some() {
            self.clear_selection_popover();
        }
        if let Some(action) = action {
            match action {
                SelectionAction::Copy => {
                    ctx.copy_text(quote.text);
                    self.notice("已复制选中的文字");
                }
                SelectionAction::Favorite => self.save_favorite_quote(quote),
                SelectionAction::Comment => self.begin_comment(quote),
            }
        }
    }
}

impl eframe::App for GuiApp {
    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        let key = self
            .route_storage_key
            .clone()
            .unwrap_or_else(|| "articles".to_owned());
        storage.set_string("shiyue.desktop.route", key);
        if let Some(article_id) = self.last_opened_article_id {
            storage.set_string("shiyue.desktop.last_opened_article", article_id.to_string());
        }
        if let Ok(value) = serde_json::to_string(&self.reading_positions) {
            storage.set_string("shiyue.desktop.reading_positions", value);
        }
    }

    // eframe 0.35：App 入口是 ui(&mut Ui)，panel 在根 Ui 内 show。
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        for intent in self.desktop.poll(&ctx) {
            match intent {
                DesktopIntent::RefreshAllFeeds => {
                    if let Err(error) = self.rss_refresh.request_all() {
                        self.notice(format!("无法启动订阅刷新：{error}"));
                    }
                }
            }
        }
        self.receive_maintenance_updates(&ctx);
        self.receive_rss_refresh_updates();
        if !self.db.is_open() {
            self.show_maintenance_page(ui);
            return;
        }
        let modal_open = self.has_modal_dialog();
        if !modal_open
            && ctx.input_mut(|input| {
                input.consume_shortcut(&egui::KeyboardShortcut::new(
                    egui::Modifiers::CTRL,
                    egui::Key::F,
                ))
            })
        {
            self.open_search();
        }
        self.receive_web_clipping_updates(&ctx);
        self.receive_search_events(&ctx);
        self.receive_knowledge_updates(&ctx);
        self.refresh_desktop_projection_frame();
        self.publish_pending_knowledge_notices();
        let article_projection = self.current_article_projection();
        let article_freshness = self.current_article_freshness();
        let feed_unread_by_id = article_projection
            .as_deref()
            .map(|projection| feed_unread_index(&projection.feed_unread))
            .unwrap_or_default();
        let (saved_article_count, read_later_count, archived_article_count) =
            self.current_article_counts();

        let rss_snapshot = self.rss_refresh.snapshot();
        let busy = rss_snapshot.current.is_some();
        let theme = ReaderTheme::sspai();

        // 源栏
        let mut feed_click = None;
        let mut feed_settings_click = None;
        let mut retry_feed_id = None;
        egui::Panel::left("feeds")
            .default_size(FEED_PANEL_WIDTH)
            .size_range(200.0..=360.0)
            .resizable(true)
            .frame(
                egui::Frame::new()
                    .fill(theme.panel)
                    .stroke(egui::Stroke::new(1.0, theme.border))
                    .inner_margin(egui::Margin::symmetric(12, 10)),
            )
            .show(ui, |ui| {
                let selected_feed = self
                    .ui_state
                    .route()
                    .article_collection()
                    .and_then(|collection| match collection {
                        ArticleCollection::Feed(Some(id)) => Some(id),
                        _ => None,
                    })
                    .and_then(|id| self.feeds.iter().find(|feed| feed.id == id))
                    .cloned();
                let mut add_feed_clicked = false;
                let mut delete_feed_clicked = false;
                let mut settings_clicked = false;
                let mut refresh_clicked = false;

                ui.add_space(4.0);
                let navigation_width = ui.available_width();
                let dashboard_visible = self.ui_state.route() == Route::Dashboard;
                let dashboard_response = ui.add(
                    NavigationButton {
                        icon: RemixIcon::Dashboard,
                        selected: dashboard_visible,
                        label: "总览",
                        trailing: None,
                        color: if dashboard_visible {
                            theme.text
                        } else {
                            theme.muted
                        },
                        selected_fill: theme.selected_bg,
                        width: navigation_width,
                    }
                    .widget(),
                );
                if dashboard_response.clicked() {
                    self.navigate(Route::Dashboard);
                    self.clear_selection_popover();
                }
                ui.add_space(4.0);
                let search_selected = self.ui_state.modal_kind() == Some(ModalKind::Search);
                let search_response = ui.add_enabled(
                    !modal_open,
                    NavigationButton {
                        icon: RemixIcon::Search,
                        selected: search_selected,
                        label: "资料搜索",
                        trailing: Some("Ctrl+F".into()),
                        color: if search_selected {
                            theme.text
                        } else {
                            theme.muted
                        },
                        selected_fill: theme.selected_bg,
                        width: navigation_width,
                    }
                    .widget(),
                );
                if search_response.clicked() {
                    self.open_search();
                }
                ui.add_space(4.0);
                navigation_section_label(ui, "阅读", theme);
                let saved_articles_visible = matches!(
                    self.ui_state.route(),
                    Route::Articles(ArticleCollection::Saved)
                );
                let saved_articles_response = ui.add(
                    NavigationButton {
                        icon: RemixIcon::Star,
                        selected: saved_articles_visible,
                        label: "文章收藏",
                        trailing: Some(saved_article_count.to_string()),
                        color: if saved_articles_visible {
                            theme.text
                        } else {
                            theme.accent
                        },
                        selected_fill: theme.selected_bg,
                        width: navigation_width,
                    }
                    .widget(),
                );
                if saved_articles_response.clicked() {
                    self.select_saved_articles();
                    self.clear_selection_popover();
                }
                ui.add_space(4.0);
                navigation_section_label(ui, "资源", theme);
                let resources_visible = self.ui_state.route() == Route::Resources;
                let resources_response = ui.add(
                    NavigationButton {
                        icon: RemixIcon::Resources,
                        selected: resources_visible,
                        label: "资源库",
                        trailing: None,
                        color: if resources_visible {
                            theme.text
                        } else {
                            theme.accent
                        },
                        selected_fill: theme.selected_bg,
                        width: navigation_width,
                    }
                    .widget(),
                );
                if resources_response.clicked() {
                    self.navigate(Route::Resources);
                    self.clear_selection_popover();
                }
                ui.add_space(4.0);
                navigation_section_label(ui, "阅读队列", theme);
                let read_later_visible = matches!(
                    self.ui_state.route(),
                    Route::Articles(ArticleCollection::ReadLater)
                );
                let read_later_response = ui.add(
                    NavigationButton {
                        icon: RemixIcon::ReadLater,
                        selected: read_later_visible,
                        label: "稍后读",
                        trailing: Some(read_later_count.to_string()),
                        color: if read_later_visible {
                            theme.text
                        } else {
                            theme.muted
                        },
                        selected_fill: theme.selected_bg,
                        width: navigation_width,
                    }
                    .widget(),
                );
                if read_later_response.clicked() {
                    self.select_read_later();
                    self.clear_selection_popover();
                }
                ui.add_space(4.0);
                navigation_section_label(ui, "知识", theme);
                let excerpts_visible = self.ui_state.route() == Route::Excerpts;
                let library_response = ui.add(
                    NavigationButton {
                        icon: RemixIcon::Excerpts,
                        selected: excerpts_visible,
                        label: "摘录与想法",
                        trailing: Some(self.excerpt_count().to_string()),
                        color: if excerpts_visible {
                            theme.text
                        } else {
                            theme.accent
                        },
                        selected_fill: theme.selected_bg,
                        width: navigation_width,
                    }
                    .widget(),
                );
                if library_response.clicked() {
                    self.navigate(Route::Excerpts);
                    self.clear_selection_popover();
                }
                ui.add_space(4.0);
                let archive_visible = self.ui_state.route() == Route::Archive;
                let archive_response = ui.add(
                    NavigationButton {
                        icon: RemixIcon::Archive,
                        selected: archive_visible,
                        label: "已归档",
                        trailing: Some(archived_article_count.to_string()),
                        color: if archive_visible {
                            theme.text
                        } else {
                            theme.muted
                        },
                        selected_fill: theme.selected_bg,
                        width: navigation_width,
                    }
                    .widget(),
                );
                if archive_response.clicked() {
                    self.navigate(Route::Archive);
                    self.clear_selection_popover();
                }
                ui.add_space(4.0);
                navigation_section_label(ui, "系统", theme);
                let storage_visible = self.ui_state.route() == Route::Storage;
                let storage_response = ui.add(
                    NavigationButton {
                        icon: RemixIcon::Storage,
                        selected: storage_visible,
                        label: "资料库管理",
                        trailing: None,
                        color: if storage_visible {
                            theme.text
                        } else {
                            theme.muted
                        },
                        selected_fill: theme.selected_bg,
                        width: navigation_width,
                    }
                    .widget(),
                );
                if storage_response.clicked() {
                    self.navigate(Route::Storage);
                    if self.ui_state.route() == Route::Storage {
                        self.storage_message = None;
                        self.refresh_storage_overview();
                    }
                }
                if let Some(run) = &rss_snapshot.current {
                    ui.weak(format!(
                        "刷新 {}/{} · 失败 {} · 新增 {}",
                        run.completed_count,
                        run.target_count,
                        run.failed_feed_count,
                        run.new_article_count
                    ));
                    ui.add_space(3.0);
                } else if rss_snapshot.status == RefreshWorkflowStatus::PausedForMaintenance {
                    ui.weak("资料维护中，订阅刷新已暂停");
                    ui.add_space(3.0);
                } else if let Some(run) = &rss_snapshot.last_completed
                    && matches!(
                        run.status,
                        RefreshRunStatus::Degraded | RefreshRunStatus::Failed
                    )
                {
                    ui.weak(format!("上次刷新有 {} 个订阅失败", run.failed_feed_count));
                    ui.add_space(3.0);
                }
                ui.add_space(4.0);
                ui.separator();
                ui.add_space(4.0);
                ui.scope(|ui| {
                    // Subscription actions live with the feed list, keeping the global header
                    // focused on navigation and unread state.
                    ui.spacing_mut().button_padding = egui::vec2(2.0, 2.0);
                    ui.spacing_mut().item_spacing.x = 3.0;
                    ui.horizontal(|ui| {
                        ui.weak("订阅源");
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui
                                .add(egui::Button::image(
                                    RemixIcon::Add.image(false, theme.text, 18.0),
                                ))
                                .on_hover_text("添加订阅")
                                .clicked()
                            {
                                add_feed_clicked = true;
                            }
                            if ui
                                .add_enabled(
                                    selected_feed.is_some(),
                                    egui::Button::image(
                                        RemixIcon::Remove.image(false, theme.text, 18.0),
                                    ),
                                )
                                .on_hover_text("删除当前订阅")
                                .clicked()
                            {
                                delete_feed_clicked = true;
                            }
                            if ui
                                .add_enabled(
                                    selected_feed.is_some(),
                                    egui::Button::image(
                                        RemixIcon::Settings.image(false, theme.text, 18.0),
                                    ),
                                )
                                .on_hover_text("当前订阅设置")
                                .clicked()
                            {
                                settings_clicked = true;
                            }
                            let refresh_hint = if busy { "抓取中…" } else { "刷新订阅" };
                            if ui
                                .add_enabled(
                                    !busy,
                                    egui::Button::image(RemixIcon::Refresh.image(
                                        false,
                                        theme.muted,
                                        18.0,
                                    )),
                                )
                                .on_hover_text(refresh_hint)
                                .clicked()
                            {
                                refresh_clicked = true;
                            }
                        });
                    });
                });
                if add_feed_clicked {
                    self.open_modal(ModalState::AddFeed(
                        feed_subscription_feature::AddDraft::default(),
                    ));
                }
                if delete_feed_clicked && let Some(feed) = selected_feed.as_ref() {
                    self.open_modal(ModalState::DeleteFeed(
                        feed_subscription_feature::DeleteDraft::new(
                            feed.id,
                            feed.title.clone().unwrap_or_else(|| feed.url.clone()),
                        ),
                    ));
                }
                if settings_clicked {
                    feed_settings_click = selected_feed.clone();
                }
                if refresh_clicked && let Err(error) = self.rss_refresh.request_all() {
                    self.notice(format!("无法启动订阅刷新：{error}"));
                }
                ui.spacing_mut().item_spacing.y = 0.0;
                let focus_feed = self.pending_feed_focus.take();
                let mut feed_scroll = egui::ScrollArea::vertical().auto_shrink([false, false]);
                if let Some(feed_id) = focus_feed
                    && let Some(index) = self.feeds.iter().position(|feed| feed.id == feed_id)
                {
                    feed_scroll =
                        feed_scroll.scroll_offset(egui::vec2(0.0, index as f32 * FEED_ROW_HEIGHT));
                }
                feed_scroll.show_rows(ui, FEED_ROW_HEIGHT, self.feeds.len(), |ui, row_range| {
                    for index in row_range {
                        let fd = self.feeds[index].clone();
                        let unread = feed_unread_by_id.get(&fd.id).copied().unwrap_or_default();
                        let title = fd.title.clone().unwrap_or_else(|| fd.url.clone());
                        let mark = if fd.disabled {
                            "✗"
                        } else if fd.fail_count > 0 {
                            "⚠"
                        } else if unread > 0 {
                            "●"
                        } else {
                            " "
                        };
                        let sel = self.ui_state.route()
                            == Route::Articles(ArticleCollection::Feed(Some(fd.id)));
                        let fill = if sel {
                            theme.selected_bg
                        } else {
                            egui::Color32::TRANSPARENT
                        };
                        let inline_status = if fd.disabled {
                            Some(feed_subscription_feature::disabled_feed_status(
                                fd.disabled,
                                fd.fail_count,
                            ))
                        } else if fd.fail_count > 0 {
                            Some(format!("失败 {}", fd.fail_count))
                        } else {
                            None
                        };
                        let action_width = inline_status
                            .as_ref()
                            .map(|status| status.chars().count() as f32 * 7.0 + 56.0)
                            .unwrap_or(0.0);
                        let title_width = (ui.available_width() - action_width - 6.0).max(0.0);
                        let mut retry_clicked = false;
                        let mut row_layout = *ui.layout();
                        row_layout.main_align = egui::Align::Min;
                        let response = ui
                            .with_layout(row_layout, |ui| {
                                let mut response = ui.add(
                                    egui::Button::new(
                                        egui::RichText::new(format!("{mark} {title} ({unread})"))
                                            .size(15.0)
                                            .family(if sel || unread > 0 {
                                                egui::FontFamily::Name("cjk-bold".into())
                                            } else {
                                                egui::FontFamily::Proportional
                                            })
                                            .color(if sel || unread > 0 {
                                                theme.text
                                            } else {
                                                theme.muted
                                            }),
                                    )
                                    .fill(fill)
                                    .stroke(egui::Stroke::NONE)
                                    .corner_radius(egui::CornerRadius::same(4))
                                    .truncate()
                                    .min_size(egui::vec2(title_width, FEED_ROW_HEIGHT)),
                                );
                                if let Some(error) = &fd.last_error {
                                    response = response.on_hover_text(format!(
                                        "最近刷新失败：{}",
                                        format_refresh_error_for_display(error)
                                    ));
                                }
                                if let Some(status) = inline_status.as_deref() {
                                    ui.label(egui::RichText::new(status).size(11.0).color(
                                        if fd.disabled {
                                            theme.subtle
                                        } else {
                                            theme.accent
                                        },
                                    ));
                                    if fd.disabled
                                        && ui
                                            .small_button("重试")
                                            .on_hover_text("清除失败状态并立即抓取一次")
                                            .clicked()
                                    {
                                        retry_clicked = true;
                                    }
                                }
                                response
                            })
                            .inner;
                        if retry_clicked {
                            retry_feed_id = Some(fd.id);
                        }
                        if sel {
                            ui.painter().rect_filled(
                                egui::Rect::from_min_max(
                                    response.rect.left_top(),
                                    egui::pos2(response.rect.left() + 3.0, response.rect.bottom()),
                                ),
                                egui::CornerRadius::same(2),
                                theme.accent,
                            );
                        }
                        if focus_feed == Some(fd.id) {
                            response.request_focus();
                        }
                        if response.clicked() {
                            response.request_focus();
                            feed_click = Some(fd.id);
                        }
                        if response.has_focus() && !ui.ctx().egui_wants_keyboard_input() {
                            ui.input(|input| {
                                let target = if input.key_pressed(egui::Key::ArrowUp) {
                                    feed_navigation_target(&self.feeds, fd.id, -1)
                                } else if input.key_pressed(egui::Key::ArrowDown) {
                                    feed_navigation_target(&self.feeds, fd.id, 1)
                                } else if input.key_pressed(egui::Key::Home) {
                                    self.feeds.first().map(|feed| feed.id)
                                } else if input.key_pressed(egui::Key::End) {
                                    self.feeds.last().map(|feed| feed.id)
                                } else {
                                    None
                                };
                                if let Some(target) = target {
                                    feed_click = Some(target);
                                    self.pending_feed_focus = Some(target);
                                }
                                if input.key_pressed(egui::Key::Enter) {
                                    feed_click = Some(fd.id);
                                }
                            });
                        }
                    }
                });
            });
        if let Some(id) = feed_click {
            self.select_feed(id);
        }
        if let Some(feed) = feed_settings_click {
            self.set_feed_settings_panel(Some(feed));
        }
        if let Some(feed_id) = retry_feed_id {
            let outcome = {
                let dependencies = feed_subscription_feature::Dependencies {
                    database: self.db_path.as_path(),
                    refresh: &self.rss_refresh,
                };
                feed_subscription_feature::retry_disabled_feed(feed_id, &dependencies)
            };
            self.apply_feed_feature_outcome(outcome);
        }
        self.show_active_modal(&ctx);
        if self.ui_state.route() == Route::Dashboard {
            self.show_dashboard_page(ui);
            self.show_selection_notice(&ctx);
            return;
        }
        if self.ui_state.route() == Route::Storage {
            self.show_storage_page(ui);
            self.show_selection_notice(&ctx);
            return;
        }
        if self.ui_state.route() == Route::Resources {
            self.show_resource_library_page(ui);
            self.show_selection_notice(&ctx);
            return;
        }
        if self.ui_state.route() == Route::Excerpts {
            self.show_saved_library_page(ui);
            self.show_selection_notice(&ctx);
            return;
        }
        if self.ui_state.route() == Route::Archive {
            self.show_archive_library_page(ui);
            self.show_selection_notice(&ctx);
            return;
        }

        if self.feed_settings_panel().is_some() {
            egui::Panel::right("feed-settings")
                .resizable(true)
                .default_size(360.0)
                .size_range(320.0..=520.0)
                .frame(
                    egui::Frame::new()
                        .fill(theme.canvas)
                        .stroke(egui::Stroke::new(1.0, theme.border))
                        .inner_margin(egui::Margin::symmetric(18, 16)),
                )
                .show(ui, |ui| self.show_feed_settings_panel(ui));
        }

        // 文章栏
        let mut open_article = None;
        let mut unread_article = None;
        let mut star_article = None;
        let mut archive_article = None;
        let mut read_later_article = None;
        let mut tag_article = None;
        let mut batch_toggles = Vec::new();
        let mut batch_action = None;
        let article_collection = self.ui_state.route().article_collection();
        let saved_collection = article_collection == Some(ArticleCollection::Saved);
        let articles = article_projection
            .as_deref()
            .map(|projection| projection.articles.as_slice())
            .unwrap_or_default();
        let article_loading = article_projection.is_none()
            && matches!(
                article_freshness,
                Some(ProjectionFreshness::Loading | ProjectionFreshness::Refreshing)
            );
        egui::Panel::left("articles")
            .default_size(ARTICLE_PANEL_WIDTH)
            .size_range(280.0..=520.0)
            .resizable(true)
            .frame(
                egui::Frame::new()
                    .fill(theme.panel)
                    .stroke(egui::Stroke::new(1.0, theme.border))
                    .inner_margin(egui::Margin::symmetric(12, 10)),
            )
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new(match article_collection {
                            Some(ArticleCollection::Saved) => "文章收藏",
                            Some(ArticleCollection::ReadLater) => "稍后读",
                            Some(ArticleCollection::SearchResult(_)) => "搜索结果",
                            _ => "文章",
                        })
                        .size(20.0)
                        .family(egui::FontFamily::Name("cjk-bold".into())),
                    );
                    ui.label(
                        egui::RichText::new(format!("{} 篇", articles.len()))
                            .size(13.0)
                            .color(theme.muted),
                    );
                    if saved_collection {
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui
                                .add(
                                    egui::Button::new(
                                        egui::RichText::new("＋").size(20.0).color(theme.accent),
                                    )
                                    .stroke(egui::Stroke::NONE),
                                )
                                .on_hover_text("保存网页或粘贴 HTML")
                                .clicked()
                            {
                                self.open_web_clip_dialog();
                            }
                        });
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui
                            .add(
                                egui::Button::new(if self.batch_mode { "完成" } else { "批量" })
                                    .stroke(egui::Stroke::NONE),
                            )
                            .clicked()
                        {
                            self.batch_mode = !self.batch_mode;
                            if !self.batch_mode {
                                self.batch_selection.clear();
                            }
                        }
                    });
                });
                ui.separator();
                if self.batch_mode {
                    ui.horizontal_wrapped(|ui| {
                        ui.label(format!("已选 {} 篇", self.batch_selection.len()));
                        if ui.button("收藏").clicked() {
                            batch_action = Some(ArticleBatchAction::Bookmark);
                        }
                        if ui.button("稍后读").clicked() {
                            batch_action = Some(ArticleBatchAction::ReadLater);
                        }
                        if ui.button("归档").clicked() {
                            batch_action = Some(ArticleBatchAction::Archive);
                        }
                    });
                    ui.separator();
                }
                ui.add_space(4.0);
                if article_loading {
                    ui.add_space(26.0);
                    ui.vertical_centered(|ui| {
                        ui.label(egui::RichText::new("正在加载文章…").color(theme.muted));
                    });
                } else if saved_collection && articles.is_empty() {
                    ui.add_space(26.0);
                    ui.vertical_centered(|ui| {
                        ui.label(
                            egui::RichText::new("还没有文章收藏")
                                .size(17.0)
                                .color(theme.text)
                                .family(egui::FontFamily::Name("cjk-bold".into())),
                        );
                        ui.add_space(6.0);
                        ui.label(
                            egui::RichText::new(
                                "打开订阅文章，点击正文标题下方「收藏文章」；也可点右上角＋保存网页。",
                            )
                                .size(13.0)
                                .color(theme.muted),
                        );
                    });
                }
                ui.spacing_mut().item_spacing.y = 0.0;
                let focus_article = self.pending_article_focus.take();
                let mut article_scroll = egui::ScrollArea::vertical().auto_shrink([false, false]);
                if let Some(offset) = article_scroll_offset(articles, focus_article) {
                    article_scroll = article_scroll.scroll_offset(egui::vec2(0.0, offset));
                }
                article_scroll.show_rows(ui, ARTICLE_ROW_HEIGHT, articles.len(), |ui, row_range| {
                        for index in row_range {
                            let a = &articles[index];
                            let is_web_clip = article_projection
                                .as_deref()
                                .is_some_and(|projection| {
                                    projection.fixed_bookmark_ids.contains(&a.id)
                                });
                            let star = if is_web_clip {
                                "  ◫"
                            } else if a.starred {
                                " ★"
                            } else {
                                ""
                            };
                            let dot = if a.is_read { "" } else { "● " };
                            let title = a.title.clone().unwrap_or_default();
                            let sel = self.sel_article_id == Some(a.id);
                            let fill = if sel {
                                theme.selected_bg
                            } else {
                                egui::Color32::TRANSPARENT
                            };
                            let title_width = if self.batch_mode {
                                (ui.available_width() - 28.0).max(0.0)
                            } else {
                                ui.available_width()
                            };
                            let article_button = egui::Button::new(
                                    egui::RichText::new(format!("{dot}{title}{star}"))
                                        .size(15.5)
                                        .family(if a.is_read {
                                            egui::FontFamily::Proportional
                                        } else {
                                            egui::FontFamily::Name("cjk-bold".into())
                                        })
                                        .color(if a.is_read { theme.muted } else { theme.text }),
                                )
                                .fill(fill)
                                .stroke(egui::Stroke::NONE)
                                .corner_radius(egui::CornerRadius::same(4))
                                .truncate()
                                .min_size(egui::vec2(title_width, 38.0));
                            let resp = if self.batch_mode {
                                ui.horizontal(|ui| {
                                    let mut selected = self.batch_selection.contains(&a.id);
                                    if ui.checkbox(&mut selected, "").changed() {
                                        batch_toggles.push((a.id, selected));
                                    }
                                    ui.add(article_button)
                                })
                                .inner
                            } else {
                                ui.add(article_button)
                            };
                            if sel {
                                ui.painter().rect_filled(
                                    egui::Rect::from_min_max(
                                        resp.rect.left_top(),
                                        egui::pos2(resp.rect.left() + 3.0, resp.rect.bottom()),
                                    ),
                                    egui::CornerRadius::same(2),
                                    theme.accent,
                                );
                            }
                            if focus_article == Some(a.id) {
                                resp.request_focus();
                            }
                            if resp.clicked() {
                                resp.request_focus();
                                if self.batch_mode {
                                    batch_toggles.push((
                                        a.id,
                                        !self.batch_selection.contains(&a.id),
                                    ));
                                } else {
                                    open_article = Some(a.id);
                                }
                            }
                            if resp.has_focus() && !ui.ctx().egui_wants_keyboard_input() {
                                ui.input(|input| {
                                    if input.key_pressed(egui::Key::ArrowUp) {
                                        if let Some(id) = article_navigation_target(articles, a.id, -1) {
                                            open_article = Some(id);
                                            self.pending_article_focus = Some(id);
                                        }
                                    } else if input.key_pressed(egui::Key::ArrowDown) {
                                        if let Some(id) = article_navigation_target(articles, a.id, 1) {
                                            open_article = Some(id);
                                            self.pending_article_focus = Some(id);
                                        }
                                    } else if input.key_pressed(egui::Key::Enter) {
                                        open_article = Some(a.id);
                                    }
                                });
                            }
                            resp.context_menu(|ui| {
                                if saved_collection {
                                    let remove_label = if is_web_clip {
                                        "删除本地网页…"
                                    } else {
                                        "取消文章收藏"
                                    };
                                    if ui.button(remove_label).clicked() {
                                        star_article = Some(a.id);
                                        ui.close();
                                    }
                                } else {
                                    if ui.button("标为未读").clicked() {
                                        unread_article = Some(a.id);
                                    }
                                    let star_label = if a.starred {
                                        "取消文章收藏"
                                    } else {
                                        "收藏文章"
                                    };
                                    if ui.button(star_label).clicked() {
                                        star_article = Some(a.id);
                                    }
                                    let read_later_label = if a.read_later {
                                        "移出稍后读"
                                    } else {
                                        "加入稍后读"
                                    };
                                    if ui.button(read_later_label).clicked() {
                                        read_later_article = Some(a.id);
                                    }
                                    if ui.button("编辑标签…").clicked() {
                                        tag_article = Some(a.id);
                                    }
                                    ui.separator();
                                    if ui.button("归档文章").clicked() {
                                        archive_article = Some(a.id);
                                        ui.close();
                                    }
                                }
                            });
                            let meta = match (a.author.as_deref(), a.published) {
                                (Some(author), Some(ts)) => {
                                    format!("{author}  ·  {}", format_timestamp(ts))
                                }
                                (Some(author), None) => author.to_string(),
                                (None, Some(ts)) => format_timestamp(ts),
                                (None, None) => String::new(),
                            };
                            if !meta.is_empty() {
                                ui.add(
                                    egui::Label::new(
                                        egui::RichText::new(meta).size(12.5).color(theme.subtle),
                                    )
                                    .truncate(),
                                );
                            }
                            ui.add_space(ARTICLE_ROW_HEIGHT - 38.0 - 18.0);
                        }
                    });
            });
        for (article_id, selected) in batch_toggles {
            if selected {
                self.batch_selection.insert(article_id);
            } else {
                self.batch_selection.remove(&article_id);
            }
        }
        if let Some(action) = batch_action {
            self.apply_batch_action(action);
        }
        if let Some(id) = open_article {
            self.select_article(id);
        }
        if let Some(id) = unread_article {
            self.mark_unread(id);
        }
        if let Some(id) = star_article {
            if saved_collection {
                self.remove_saved_article(id);
            } else {
                self.toggle_star(id);
            }
        }
        if let Some(id) = archive_article {
            self.archive_article(id);
        }
        if let Some(id) = read_later_article {
            self.toggle_read_later(id);
        }
        if let Some(id) = tag_article {
            self.open_tag_dialog(id);
        }

        // 正文栏：Article/Excerpt 数据直接来自本帧权威 projection；GUI 只保留交互选择。
        let selected_article = self.sel_article_id.and_then(|article_id| {
            article_projection
                .as_deref()
                .and_then(|projection| {
                    projection
                        .articles
                        .iter()
                        .find(|article| article.id == article_id)
                })
                .cloned()
        });
        let selected_excerpt_projection = self.sel_article_id.and_then(|article_id| {
            self.excerpt_projection(ExcerptProjectionScope::Article(article_id))
        });
        let mut body_rendered = false;
        let mut rendered_article_id = None;
        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(theme.canvas)
                    .inner_margin(egui::Margin::ZERO),
            )
            .show(ui, |ui| {
                let Some(article_id) = self.sel_article_id else {
                    ui.centered_and_justified(|ui| ui.label("← 选一篇文章"));
                    return;
                };
                let reset_body_scroll = self.body_article_id != Some(article_id);
                self.body_article_id = Some(article_id);
                let Some(a) = selected_article.as_ref() else {
                    ui.centered_and_justified(|ui| ui.label("← 选一篇文章"));
                    return;
                };
                rendered_article_id = Some(article_id);
                let title = a.title.clone().unwrap_or_default();
                let date = a.published.map(format_timestamp).unwrap_or_default();
                let url = a.url.clone();
                let author = a.author.clone();
                let article_starred = a.starred;
                let article_read_later = a.read_later;
                let article_is_read = a.is_read;
                let article_archived = a.archived;
                let article_tags = article_projection
                    .as_deref()
                    .and_then(|projection| projection.tags.get(&article_id))
                    .cloned()
                    .unwrap_or_default();
                let is_web_clipping = article_projection
                    .as_deref()
                    .is_some_and(|projection| projection.fixed_bookmark_ids.contains(&article_id));
                let article_content = a.content.clone().unwrap_or_default();
                let article_base_url = a.url.clone();
                let article_ai = article_projection
                    .as_deref()
                    .and_then(|projection| projection.article_ai.get(&article_id))
                    .cloned();
                let article_ai_task =
                    self.knowledge_task(KnowledgeTaskKind::ArticleSummary, article_id);
                let article_ai_is_busy = article_ai_task.as_ref().is_some_and(|view| {
                    matches!(
                        view.status,
                        KnowledgeTaskStatus::Queued | KnowledgeTaskStatus::Running
                    )
                });
                let saved_excerpts = selected_excerpt_projection
                    .as_ref()
                    .map(|projection| projection.excerpts.clone())
                    .unwrap_or_default();
                let mut edit_thought: Option<ExcerptView> = None;
                let mut remove_thought = None;
                let mut delete_excerpt: Option<ExcerptView> = None;
                let mut toggle_article_star = false;
                let mut toggle_article_read_later = false;
                let mut toggle_article_read = false;
                let mut archive_selected_article = false;
                let mut edit_article_tags = false;
                let mut generate_article_ai = false;
                let restore_selection = self
                    .pending_selection_anchor
                    .as_ref()
                    .filter(|selection| selection.article_id == article_id)
                    .map(|selection| RestoreSelection {
                        selected_text: selection.selected_text.clone(),
                        anchor: TextAnchor {
                            start_offset: selection.start_offset,
                            end_offset: selection.end_offset,
                            prefix: selection.anchor_prefix.clone(),
                            suffix: selection.anchor_suffix.clone(),
                        },
                    });
                if restore_selection.is_some() {
                    self.pending_selection_anchor = None;
                }
                let mut presentation_outcome: Option<PresentOutcome> = None;
                let mut body_scroll = egui::ScrollArea::vertical()
                    .id_salt(("article-body-v2", article_id))
                    .hscroll(false);
                if let Some((pending_article_id, offset)) = self.pending_body_scroll.take() {
                    if pending_article_id == article_id {
                        body_scroll = body_scroll.scroll_offset(egui::vec2(0.0, offset.max(0.0)));
                    } else {
                        self.pending_body_scroll = Some((pending_article_id, offset));
                    }
                } else if reset_body_scroll {
                    body_scroll = body_scroll.scroll_offset(egui::Vec2::ZERO);
                }
                let mut body_scroll_output = body_scroll.show_viewport(ui, |ui, viewport| {
                    // Keep the scroll viewport full width while centering a
                    // readable 820 px column on the white reading canvas.
                    ui.painter().rect_filled(ui.max_rect(), 0.0, theme.canvas);
                    let available = ui.available_width();
                    let content_width = available.clamp(0.0, ARTICLE_MAX_WIDTH);
                    ui.set_min_width(available);
                    let side_margin = ((available - content_width) * 0.5).max(0.0);
                    ui.horizontal(|ui| {
                        ui.add_space(side_margin);
                        ui.allocate_ui_with_layout(
                            egui::vec2(content_width, 0.0),
                            egui::Layout::top_down(egui::Align::Min),
                            |ui| {
                                ui.set_width(content_width);
                                ui.add_space(26.0);
                                let presentation = self.article_document.show(
                                    ui,
                                    PresentRequest {
                                        source: ArticleDocumentSource {
                                            article_id,
                                            title: &title,
                                            html: &article_content,
                                            base_url: article_base_url.as_deref(),
                                        },
                                        viewport,
                                        restore_selection: restore_selection.clone(),
                                        scroll_title_into_view: reset_body_scroll,
                                    },
                                    |ui| {
                                        ui.horizontal_wrapped(|ui| {
                                            if let Some(author) = &author {
                                                ui.label(
                                                    egui::RichText::new(author)
                                                        .size(13.0)
                                                        .color(theme.subtle),
                                                );
                                                ui.label(
                                                    egui::RichText::new("·")
                                                        .size(13.0)
                                                        .color(theme.subtle),
                                                );
                                            }
                                            ui.label(
                                                egui::RichText::new(date)
                                                    .size(13.0)
                                                    .color(theme.subtle),
                                            );
                                            if let Some(u) = &url
                                                && ui
                                                    .add(
                                                        egui::Button::new(
                                                            egui::RichText::new("在浏览器中打开 ↗")
                                                                .size(13.0)
                                                                .color(theme.link),
                                                        )
                                                        .stroke(egui::Stroke::NONE),
                                                    )
                                                    .clicked()
                                            {
                                                open_in_browser(u);
                                            }
                                        });
                                        ui.add_space(10.0);
                                        ui.horizontal_wrapped(|ui| {
                                            if is_web_clipping {
                                                article_reader_action_button(
                                                    ui,
                                                    "已保存网页",
                                                    true,
                                                    false,
                                                )
                                                .on_disabled_hover_text(
                                                    "网页收藏是固定资料，可从文章收藏中删除",
                                                );
                                            } else {
                                                let label = if article_starred {
                                                    "已收藏"
                                                } else {
                                                    "收藏"
                                                };
                                                if article_reader_action_button(
                                                    ui,
                                                    label,
                                                    article_starred,
                                                    true,
                                                )
                                                .on_hover_text(if article_starred {
                                                    "取消文章收藏"
                                                } else {
                                                    "收藏文章"
                                                })
                                                .clicked()
                                                {
                                                    toggle_article_star = true;
                                                }
                                            }
                                            let later_label = if article_read_later {
                                                "已在稍后读"
                                            } else {
                                                "稍后读"
                                            };
                                            if article_reader_action_button(
                                                ui,
                                                later_label,
                                                article_read_later,
                                                true,
                                            )
                                            .on_hover_text(if article_read_later {
                                                "移出稍后读"
                                            } else {
                                                "加入稍后读"
                                            })
                                            .clicked()
                                            {
                                                toggle_article_read_later = true;
                                            }
                                            let read_label = if article_is_read {
                                                "标为未读"
                                            } else {
                                                "标为已读"
                                            };
                                            if article_reader_action_button(
                                                ui,
                                                read_label,
                                                article_is_read,
                                                true,
                                            )
                                            .clicked()
                                            {
                                                toggle_article_read = true;
                                            }
                                            let archive_label = if article_archived {
                                                "已归档"
                                            } else {
                                                "归档"
                                            };
                                            let archive_enabled =
                                                !article_archived && !is_web_clipping;
                                            let archive_response = article_reader_action_button(
                                                ui,
                                                archive_label,
                                                article_archived,
                                                archive_enabled,
                                            );
                                            let archive_response = if is_web_clipping {
                                                archive_response.on_disabled_hover_text(
                                                    "网页收藏是固定资料，不能归档",
                                                )
                                            } else {
                                                archive_response
                                            };
                                            if archive_response.clicked() {
                                                archive_selected_article = true;
                                            }
                                        });
                                        ui.add_space(6.0);
                                        ui.horizontal_wrapped(|ui| {
                                            if ui
                                                .add(
                                                    egui::Button::new(
                                                        egui::RichText::new("标签")
                                                            .size(13.0)
                                                            .color(theme.muted),
                                                    )
                                                    .stroke(egui::Stroke::new(1.0, theme.border)),
                                                )
                                                .clicked()
                                            {
                                                edit_article_tags = true;
                                            }
                                            if ui
                                                .add_enabled(
                                                    !article_ai_is_busy,
                                                    egui::Button::new(if article_ai_is_busy {
                                                        "AI 处理中…"
                                                    } else if article_ai.is_some() {
                                                        "重新总结与翻译"
                                                    } else {
                                                        "AI 总结与翻译"
                                                    }),
                                                )
                                                .clicked()
                                            {
                                                generate_article_ai = true;
                                            }
                                            for tag in &article_tags {
                                                ui.label(
                                                    egui::RichText::new(format!("#{tag}"))
                                                        .size(13.0)
                                                        .color(theme.link)
                                                        .background_color(theme.selected_bg),
                                                );
                                            }
                                        });
                                        ui.separator();
                                        ui.add_space(18.0);
                                        if let Some(task) = &article_ai_task {
                                            if matches!(
                                                task.status,
                                                KnowledgeTaskStatus::Queued
                                                    | KnowledgeTaskStatus::Running
                                            ) {
                                                ui.horizontal(|ui| {
                                                    ui.spinner();
                                                    ui.label("正在生成中文总结与翻译");
                                                });
                                                ui.add_space(12.0);
                                            } else if matches!(
                                                task.status,
                                                KnowledgeTaskStatus::Failed
                                                    | KnowledgeTaskStatus::Interrupted
                                            ) {
                                                egui::Frame::group(ui.style()).show(ui, |ui| {
                                                    ui.colored_label(
                                                        egui::Color32::RED,
                                                        "上次 AI 处理失败，可点击上方按钮重试",
                                                    );
                                                    ui.collapsing("技术详情", |ui| {
                                                        ui.monospace(
                                                            task.technical_detail
                                                                .as_deref()
                                                                .unwrap_or("未知错误"),
                                                        );
                                                    });
                                                });
                                                ui.add_space(12.0);
                                            }
                                        }
                                        if let Some(ai) = &article_ai {
                                            egui::Frame::group(ui.style()).show(ui, |ui| {
                                                ui.heading("AI 总结");
                                                ui.label(&ai.summary_zh);
                                                ui.add_space(10.0);
                                                ui.collapsing("查看中文翻译", |ui| {
                                                    ui.label(&ai.translation_zh);
                                                });
                                                ui.weak(format!(
                                                    "{} · {}",
                                                    ai.model,
                                                    format_timestamp(ai.updated_at)
                                                ));
                                            });
                                            ui.add_space(18.0);
                                        }
                                        if !saved_excerpts.is_empty() {
                                            ui.collapsing(
                                                format!("已保存摘录（{}）", saved_excerpts.len()),
                                                |ui| {
                                                    for excerpt in &saved_excerpts {
                                                        egui::Frame::group(ui.style()).show(
                                                            ui,
                                                            |ui| {
                                                                ui.label(&excerpt.selected_text);
                                                                ui.horizontal(|ui| {
                                                                    ui.label("★ 已摘录");
                                                                    if excerpt.identity_kind
                                                            == ExcerptIdentityKind::Legacy
                                                        {
                                                            ui.weak("历史记录");
                                                        }
                                                                    if let Some(thought) =
                                                                        &excerpt.thought
                                                                    {
                                                                        ui.weak(format!(
                                                                            "想法：{}",
                                                                            thought.content
                                                                        ));
                                                                        if ui
                                                                            .small_button(
                                                                                "编辑想法",
                                                                            )
                                                                            .clicked()
                                                                        {
                                                                            edit_thought = Some(
                                                                                excerpt.clone(),
                                                                            );
                                                                        }
                                                                        if ui
                                                                            .small_button(
                                                                                "删除想法",
                                                                            )
                                                                            .clicked()
                                                                        {
                                                                            remove_thought =
                                                                                Some(excerpt.id);
                                                                        }
                                                                    }
                                                                    if ui
                                                                        .small_button("删除摘录")
                                                                        .clicked()
                                                                    {
                                                                        delete_excerpt =
                                                                            Some(excerpt.clone());
                                                                    }
                                                                });
                                                            },
                                                        );
                                                        ui.add_space(4.0);
                                                    }
                                                },
                                            );
                                            ui.add_space(10.0);
                                        }
                                    },
                                );
                                body_rendered = presentation.body_rendered;
                                presentation_outcome = Some(presentation);
                            },
                        );
                        ui.add_space(side_margin);
                    });
                });
                let presentation_outcome = presentation_outcome.unwrap_or_default();
                if presentation_outcome.scroll_adjustment_y.abs() > 0.1 {
                    body_scroll_output.state.offset.y = (body_scroll_output.state.offset.y
                        + presentation_outcome.scroll_adjustment_y)
                        .max(0.0);
                    body_scroll_output.state.store(&ctx, body_scroll_output.id);
                    ctx.request_repaint();
                }
                if restore_selection.is_some() {
                    if let Some(span_top) = presentation_outcome.restored_span_top {
                        let offset = body_scroll_output.state.offset.y + span_top
                            - body_scroll_output.inner_rect.top()
                            - 28.0;
                        self.pending_body_scroll = Some((article_id, offset.max(0.0)));
                        self.notice("已定位到摘录原文");
                        ctx.request_repaint();
                    } else if presentation_outcome.restore_failed {
                        self.notice("正文已更新，暂时找不到这段摘录");
                    }
                }
                let mut selection_drag_started = false;
                let mut selection_popup_request = None;
                for intent in presentation_outcome.intents {
                    match intent {
                        PresentationIntent::SelectionStarted => selection_drag_started = true,
                        PresentationIntent::SelectedQuote {
                            quote,
                            anchor_rect,
                            source_layer,
                        } => {
                            selection_popup_request = Some(SelectionPopupRequest {
                                quote,
                                anchor_rect,
                                source_layer,
                            });
                        }
                        PresentationIntent::OpenUrl(url) => open_in_browser(&url),
                    }
                }
                let scroll_offset = body_scroll_output.state.offset;
                self.current_body_scroll = scroll_offset.y;
                if body_rendered && scroll_offset.y.is_finite() {
                    self.reading_positions
                        .insert(article_id, scroll_offset.y.max(0.0));
                }
                let popover_matches_article = self
                    .ui_state
                    .popover()
                    .is_some_and(|popover| popover.quote.article_id == article_id);
                let popup_moved_away_from_selection = popover_matches_article
                    && self.selection_popup_geometry.as_ref().is_some_and(|popup| {
                        (popup.scroll_offset - scroll_offset).length_sq() > 0.25
                    });
                let popup_layout_changed = popover_matches_article
                    && self.selection_popup_geometry.as_ref().is_some_and(|popup| {
                        // Image placeholders are replaced with their natural
                        // aspect ratio asynchronously, so the full article
                        // content size can legitimately change immediately
                        // after selection.  That must not dismiss the toolbar;
                        // only a real viewport geometry change invalidates it.
                        rect_changed(popup.viewport_rect, body_scroll_output.inner_rect)
                    });
                if selection_drag_started || popup_moved_away_from_selection || popup_layout_changed
                {
                    self.clear_selection_popover();
                }
                if let Some(request) = selection_popup_request {
                    self.selection_popup_generation =
                        self.selection_popup_generation.wrapping_add(1);
                    let generation = self.selection_popup_generation;
                    self.ui_state
                        .reduce(UiAction::SetPopover(Some(SelectionPopoverState {
                            quote: request.quote,
                            generation,
                        })));
                    self.selection_popup_geometry = Some(SelectionPopupGeometry {
                        anchor_rect: request.anchor_rect,
                        source_layer: request.source_layer,
                        scroll_offset,
                        viewport_rect: body_scroll_output.inner_rect,
                        generation,
                    });
                }
                if let Some(excerpt) = edit_thought {
                    self.begin_edit_thought(&excerpt);
                } else if let Some(excerpt_id) = remove_thought {
                    self.remove_thought(excerpt_id, ExcerptProjectionScope::Article(article_id));
                } else if let Some(excerpt) = delete_excerpt {
                    self.request_delete_excerpt(
                        &excerpt,
                        ExcerptProjectionScope::Article(article_id),
                    );
                }
                if toggle_article_star {
                    self.toggle_star(article_id);
                }
                if toggle_article_read_later {
                    self.toggle_read_later(article_id);
                }
                if toggle_article_read {
                    if article_is_read {
                        self.mark_unread(article_id);
                    } else if let Err(error) =
                        self.apply_article_library_change(ArticleLifecycleChange::SetRead {
                            article_id,
                            target: true,
                        })
                    {
                        self.report_article_library_failure("标记已读", error);
                    }
                }
                if archive_selected_article {
                    self.archive_article(article_id);
                }
                if edit_article_tags {
                    self.open_tag_dialog(article_id);
                }
                if generate_article_ai {
                    match self.knowledge_feature.request_article_summary(
                        article_id,
                        &self.knowledge_engine,
                        &ctx,
                    ) {
                        Ok(()) => {}
                        Err(error) => {
                            self.notice(format!("无法提交文章 AI 任务：{error:#}"));
                        }
                    }
                }
            });
        let article_unread = selected_article
            .as_ref()
            .is_some_and(|article| !article.is_read);
        self.update_delayed_read_marking(
            &ctx,
            rendered_article_id,
            body_rendered && article_unread,
        );
        self.show_selection_popup(&ctx);
        self.show_selection_notice(&ctx);
    }
}

fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn format_timestamp(timestamp: i64) -> String {
    chrono::DateTime::from_timestamp(timestamp, 0)
        .map(|date| date.format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_default()
}

fn search_preview(source: &str, query: &str, max_chars: usize) -> String {
    let text = article_visible_text(source, None);
    let normalized = if text.trim().is_empty() {
        source.split_whitespace().collect::<Vec<_>>().join(" ")
    } else {
        text.split_whitespace().collect::<Vec<_>>().join(" ")
    };
    if normalized.is_empty() {
        return "（无可显示的文字）".to_owned();
    }

    let chars: Vec<char> = normalized.chars().collect();
    let lower_chars: Vec<char> = normalized.to_lowercase().chars().collect();
    let query_chars: Vec<char> = query
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_lowercase()
        .chars()
        .collect();
    let match_char = if query_chars.is_empty() {
        0
    } else {
        lower_chars
            .windows(query_chars.len())
            .position(|window| window == query_chars)
            .unwrap_or(0)
    };
    let leading = max_chars / 3;
    let start = match_char.saturating_sub(leading);
    let end = (start + max_chars).min(chars.len());
    let mut preview: String = chars[start..end].iter().collect();
    if start > 0 {
        preview.insert(0, '…');
    }
    if end < chars.len() {
        preview.push('…');
    }
    preview
}

fn search_match_ranges(text: &str, query: &str) -> Vec<Range<usize>> {
    let char_starts: Vec<usize> = text.char_indices().map(|(start, _)| start).collect();
    let folded: Vec<String> = text.chars().map(|ch| ch.to_lowercase().collect()).collect();
    let mut ranges = Vec::new();
    for term in query.split_whitespace() {
        let term_chars = term
            .chars()
            .map(|ch| ch.to_lowercase().collect::<String>())
            .collect::<Vec<_>>();
        if term_chars.is_empty() || folded.len() < term_chars.len() {
            continue;
        }
        let mut at = 0usize;
        while at + term_chars.len() <= folded.len() {
            if folded[at..at + term_chars.len()] == term_chars {
                let start = char_starts[at];
                let end_char = at + term_chars.len();
                let end = char_starts.get(end_char).copied().unwrap_or(text.len());
                ranges.push(start..end);
                at = end_char;
            } else {
                at += 1;
            }
        }
    }
    ranges.sort_by_key(|range| (range.start, range.end));
    let mut merged: Vec<Range<usize>> = Vec::new();
    for range in ranges {
        if let Some(previous) = merged.last_mut()
            && range.start <= previous.end
        {
            previous.end = previous.end.max(range.end);
        } else {
            merged.push(range);
        }
    }
    merged
}

fn search_highlight_layout_job(
    text: &str,
    query: &str,
    font_size: f32,
    color: egui::Color32,
    family: egui::FontFamily,
    theme: ReaderTheme,
) -> egui::text::LayoutJob {
    let mut job = egui::text::LayoutJob::default();
    let normal = egui::text::TextFormat {
        font_id: egui::FontId::new(font_size, family),
        line_height: Some(font_size * 1.55),
        color,
        ..Default::default()
    };
    let highlighted = egui::text::TextFormat {
        color: theme.text,
        background: egui::Color32::from_rgb(255, 229, 153),
        underline: egui::Stroke::new(1.0, theme.accent),
        ..normal.clone()
    };

    let mut cursor = 0usize;
    for range in search_match_ranges(text, query) {
        if cursor < range.start {
            job.append(&text[cursor..range.start], 0.0, normal.clone());
        }
        job.append(&text[range.clone()], 0.0, highlighted.clone());
        cursor = range.end;
    }
    if cursor < text.len() {
        job.append(&text[cursor..], 0.0, normal);
    }
    job
}

fn article_reader_action_button(
    ui: &mut egui::Ui,
    label: &str,
    active: bool,
    enabled: bool,
) -> egui::Response {
    let theme = ReaderTheme::sspai();
    let color = if active { theme.accent } else { theme.text };
    ui.add_enabled(
        enabled,
        egui::Button::new(egui::RichText::new(label).size(13.0).color(color))
            .min_size(egui::vec2(92.0, 30.0))
            .fill(if active {
                theme.selected_bg
            } else {
                theme.panel
            })
            .stroke(egui::Stroke::new(
                1.0,
                if active { theme.accent } else { theme.border },
            ))
            .corner_radius(egui::CornerRadius::same(4)),
    )
}

fn selection_toolbar_button(ui: &mut egui::Ui, icon: &str, label: &str) -> bool {
    let theme = ReaderTheme::sspai();
    let (rect, response) = ui.allocate_exact_size(egui::vec2(62.0, 42.0), egui::Sense::click());
    if response.hovered() || response.has_focus() {
        ui.painter().rect_filled(
            rect.shrink(1.0),
            egui::CornerRadius::same(6),
            theme.selected_bg,
        );
    }
    let icon_color = if response.hovered() || response.has_focus() {
        theme.accent
    } else {
        theme.text
    };
    let label_color = if response.hovered() || response.has_focus() {
        theme.text
    } else {
        theme.muted
    };
    ui.painter().text(
        egui::pos2(rect.center().x, rect.top() + 12.0),
        egui::Align2::CENTER_CENTER,
        icon,
        egui::FontId::proportional(17.0),
        icon_color,
    );
    ui.painter().text(
        egui::pos2(rect.center().x, rect.bottom() - 9.0),
        egui::Align2::CENTER_CENTER,
        label,
        egui::FontId::proportional(13.0),
        label_color,
    );
    response
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .clicked()
}
fn rect_changed(a: egui::Rect, b: egui::Rect) -> bool {
    (a.min - b.min).length_sq() > 0.25 || (a.max - b.max).length_sq() > 0.25
}

/// 用系统默认浏览器打开链接，不经过 shell/cmd 字符串解释。
fn open_in_browser(url: &str) {
    let _ = open::that_detached(url);
}

fn resource_card<R>(
    ui: &mut egui::Ui,
    add_contents: impl FnOnce(&mut egui::Ui) -> R,
) -> egui::InnerResponse<R> {
    let theme = ReaderTheme::sspai();
    let card = egui::Frame::new()
        .fill(egui::Color32::TRANSPARENT)
        .stroke(egui::Stroke::NONE)
        .inner_margin(egui::Margin::symmetric(4, 10))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.set_height(RESOURCE_CARD_HEIGHT);
            add_contents(ui)
        });
    let divider_y = card.response.rect.bottom() - 1.0;
    ui.painter().line_segment(
        [
            egui::pos2(card.response.rect.left(), divider_y),
            egui::pos2(card.response.rect.right(), divider_y),
        ],
        egui::Stroke::new(1.0, theme.border.linear_multiply(0.7)),
    );
    card
}

fn navigation_section_label(ui: &mut egui::Ui, label: &str, theme: ReaderTheme) {
    ui.add_space(6.0);
    ui.label(
        egui::RichText::new(label)
            .size(12.0)
            .strong()
            .color(theme.muted),
    );
}

fn resource_domain(url: &str) -> String {
    reqwest::Url::parse(url)
        .ok()
        .and_then(|parsed| parsed.host_str().map(str::to_owned))
        .unwrap_or_else(|| url.to_owned())
}

#[cfg(test)]
mod tests {
    use super::excerpt_thought_feature;
    use super::{
        ARTICLE_ROW_HEIGHT, ModalPayload, ModalState, PanelState, SelectedQuote, TagDialog,
        article_navigation_target, article_scroll_offset, feed_navigation_target,
        feed_subscription_feature, feed_unread_index, projection_scope_for_route,
        reconcile_article_selection, resource_card, resource_domain, search_match_ranges,
        search_preview, web_clipping_feature,
    };
    use crate::article_library_lifecycle::ProjectionScope;
    use crate::excerpt_thought_lifecycle::ExcerptTarget;
    use crate::gui_state::{ArticleCollection, PanelPayload, Route};
    use crate::model::{Article, Feed};
    use eframe::egui;
    use std::time::{Duration, Instant};

    #[test]
    fn desktop_routes_map_to_article_library_scopes() {
        assert_eq!(
            projection_scope_for_route(Route::Articles(ArticleCollection::Feed(Some(7)))),
            Some(ProjectionScope::Feed(7))
        );
        assert_eq!(
            projection_scope_for_route(Route::Articles(ArticleCollection::Saved)),
            Some(ProjectionScope::ArticleBookmarks)
        );
        assert_eq!(
            projection_scope_for_route(Route::Articles(ArticleCollection::ReadLater)),
            Some(ProjectionScope::ReadLater)
        );
        assert_eq!(
            projection_scope_for_route(Route::Articles(ArticleCollection::Feed(None))),
            Some(ProjectionScope::All)
        );
        assert_eq!(
            projection_scope_for_route(Route::Archive),
            Some(ProjectionScope::Archive)
        );
        assert_eq!(projection_scope_for_route(Route::Resources), None);
    }

    #[test]
    fn article_selection_keeps_present_choice_restores_memory_and_drops_missing_rows() {
        let article = |id| Article {
            id,
            feed_id: 7,
            entry_id: format!("entry-{id}"),
            url: None,
            title: Some(format!("Article {id}")),
            author: None,
            published: None,
            content: None,
            is_read: false,
            starred: false,
            read_later: false,
            archived: false,
            fetched_at: 1,
        };
        let articles = vec![article(11), article(12)];

        assert_eq!(
            reconcile_article_selection(Some(11), Some(12), &articles),
            Some(11)
        );
        assert_eq!(
            reconcile_article_selection(Some(99), Some(12), &articles),
            Some(12)
        );
        assert_eq!(
            reconcile_article_selection(None, Some(12), &articles),
            Some(12)
        );
        assert_eq!(
            reconcile_article_selection(Some(99), Some(98), &articles),
            None
        );
    }

    #[test]
    fn article_keyboard_navigation_stays_in_list_bounds() {
        let article = |id| Article {
            id,
            feed_id: 7,
            entry_id: format!("entry-{id}"),
            url: None,
            title: Some(format!("Article {id}")),
            author: None,
            published: None,
            content: None,
            is_read: false,
            starred: false,
            read_later: false,
            archived: false,
            fetched_at: 1,
        };
        let articles = vec![article(11), article(12), article(13)];

        assert_eq!(article_navigation_target(&articles, 12, -1), Some(11));
        assert_eq!(article_navigation_target(&articles, 12, 1), Some(13));
        assert_eq!(article_navigation_target(&articles, 11, -1), None);
        assert_eq!(article_navigation_target(&articles, 13, 1), None);
        assert_eq!(
            article_scroll_offset(&articles, Some(13)),
            Some(2.0 * ARTICLE_ROW_HEIGHT)
        );
        assert_eq!(article_scroll_offset(&articles, None), None);
        assert_eq!(article_scroll_offset(&articles, Some(99)), None);
    }

    #[test]
    fn feed_keyboard_navigation_stays_in_list_bounds() {
        let feed = |id| Feed {
            id,
            url: format!("https://example.com/{id}.xml"),
            title: Some(format!("Feed {id}")),
            last_fetch: None,
            next_fetch: 0,
            last_error: None,
            fail_count: 0,
            disabled: false,
            interval_secs: None,
        };
        let feeds = vec![feed(11), feed(12), feed(13)];

        assert_eq!(feed_navigation_target(&feeds, 12, -1), Some(11));
        assert_eq!(feed_navigation_target(&feeds, 12, 1), Some(13));
        assert_eq!(feed_navigation_target(&feeds, 11, -1), None);
        assert_eq!(feed_navigation_target(&feeds, 13, 1), None);
    }

    #[test]
    fn feed_unread_index_keeps_the_projection_count_for_each_feed() {
        let index = feed_unread_index(&[(7, 3), (9, 0), (11, 42)]);

        assert_eq!(index.get(&7), Some(&3));
        assert_eq!(index.get(&9), Some(&0));
        assert_eq!(index.get(&11), Some(&42));
        assert_eq!(index.get(&13), None);
    }

    #[test]
    fn delayed_read_marking_requires_ten_continuous_seconds() {
        let start = Instant::now();
        let mut marking = super::DelayedReadMarking::default();

        assert!(!marking.observe(Some(7), true, start));
        assert!(!marking.observe(Some(7), true, start + Duration::from_secs(9)));
        assert!(marking.observe(Some(7), true, start + Duration::from_secs(10)));
    }

    #[test]
    fn delayed_read_marking_pauses_for_blockers_and_resets_on_article_change() {
        let start = Instant::now();
        let mut marking = super::DelayedReadMarking::default();

        assert!(!marking.observe(Some(7), true, start));
        assert!(!marking.observe(Some(7), true, start + Duration::from_secs(6)));
        assert!(!marking.observe(Some(7), false, start + Duration::from_secs(20)));
        assert!(!marking.observe(Some(7), true, start + Duration::from_secs(25)));
        assert!(marking.observe(Some(7), true, start + Duration::from_secs(29)));

        assert!(!marking.observe(Some(8), true, start + Duration::from_secs(40)));
        assert!(!marking.observe(None, true, start + Duration::from_secs(50)));
    }

    #[test]
    fn resource_cards_fill_the_same_available_width() {
        let context = egui::Context::default();
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(800.0, 600.0),
            )),
            ..Default::default()
        };
        let mut widths = Vec::new();

        let _ = context.run_ui(input, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                widths.push(
                    resource_card(ui, |ui| {
                        ui.label("短内容");
                    })
                    .response
                    .rect
                    .width(),
                );
                widths.push(
                    resource_card(ui, |ui| {
                        ui.label("这是一张拥有更长标题和说明文字的资源卡片");
                    })
                    .response
                    .rect
                    .width(),
                );
            });
        });

        assert_eq!(widths.len(), 2);
        assert!(
            (widths[0] - widths[1]).abs() < 0.5,
            "resource cards should be equal width, got {widths:?}"
        );
        assert!(
            widths[0] > 700.0,
            "resource cards should fill the available row, got {widths:?}"
        );
    }

    #[test]
    fn resource_cards_keep_a_stable_height_for_different_content_lengths() {
        let context = egui::Context::default();
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(800.0, 600.0),
            )),
            ..Default::default()
        };
        let mut heights = Vec::new();

        let _ = context.run_ui(input, |ui| {
            heights.push(
                resource_card(ui, |ui| {
                    ui.label("短内容");
                })
                .response
                .rect
                .height(),
            );
            heights.push(
                resource_card(ui, |ui| {
                    ui.label("长内容 ".repeat(160));
                })
                .response
                .rect
                .height(),
            );
        });

        assert_eq!(heights.len(), 2);
        assert!(
            (heights[0] - heights[1]).abs() < 0.5,
            "resource cards should keep a stable height, got {heights:?}"
        );
    }

    #[test]
    fn resource_domain_keeps_cards_scannable_without_query_noise() {
        assert_eq!(
            resource_domain("https://koboyo.com/icons?q=app+icon"),
            "koboyo.com"
        );
        assert_eq!(resource_domain("not a url"), "not a url");
    }

    #[test]
    fn actual_modal_payloads_derive_dirty_state_from_their_drafts() {
        let mut tags = TagDialog {
            article_id: 7,
            draft: "rust".into(),
            original: "rust".into(),
            focus_input: false,
        };
        assert!(!ModalState::EditTags(tags.clone()).is_dirty());
        tags.draft.push_str(", architecture");
        assert!(ModalState::EditTags(tags).is_dirty());

        let web = web_clipping_feature::WebClipDialog::default();
        assert!(!ModalState::SaveWebPage(web).is_dirty());

        let quote = SelectedQuote {
            article_id: 7,
            text: "excerpt".into(),
            start_offset: Some(0),
            end_offset: Some(7),
            anchor_prefix: String::new(),
            anchor_suffix: String::new(),
        };
        let mut thought = excerpt_thought_feature::CommentDialog {
            quote,
            target: ExcerptTarget::Existing(3),
            draft: "current thought".into(),
            original: "current thought".into(),
            error: None,
            focus_input: false,
        };
        assert!(!ModalState::WriteThought(thought.clone()).is_dirty());
        thought = excerpt_thought_feature::CommentDialog {
            draft: "edited thought".into(),
            ..thought
        };
        assert!(ModalState::WriteThought(thought).is_dirty());
    }

    #[test]
    fn feed_settings_panel_remains_owned_by_its_feed_route() {
        let feed = Feed {
            id: 7,
            url: "https://example.com/feed.xml".into(),
            title: Some("Rust Blog".into()),
            interval_secs: Some(3600),
            last_fetch: Some(0),
            next_fetch: 0,
            last_error: None,
            fail_count: 0,
            disabled: false,
        };
        let panel =
            PanelState::FeedSettings(feed_subscription_feature::SettingsDraft::from_feed(&feed));
        assert!(!panel.is_dirty());
        assert!(panel.is_compatible(Route::Articles(ArticleCollection::Feed(Some(7)))));
        assert!(!panel.is_compatible(Route::Articles(ArticleCollection::Feed(Some(8)))));
    }

    #[test]
    fn search_preview_strips_html_and_centers_unicode_matches() {
        let source = "<p>开头是一段说明。</p><p>这里包含关键架构决策，后面还有补充内容。</p>";
        let preview = search_preview(source, "架构", 12);
        assert!(preview.contains("关键架构决策"));
        assert!(!preview.contains('<'));

        let plain = search_preview("摘录中提到状态机", "状态机", 40);
        assert_eq!(plain, "摘录中提到状态机");
    }

    #[test]
    fn search_highlight_ranges_cover_all_unicode_and_case_insensitive_matches() {
        let text = "乡音让内容有乡音，也支持 CloudFlare 与 cloudflare。";
        let chinese = search_match_ranges(text, "乡音");
        assert_eq!(
            chinese
                .iter()
                .map(|range| &text[range.clone()])
                .collect::<Vec<_>>(),
            ["乡音", "乡音"]
        );

        let english = search_match_ranges(text, "CLOUDFLARE");
        assert_eq!(
            english
                .iter()
                .map(|range| &text[range.clone()])
                .collect::<Vec<_>>(),
            ["CloudFlare", "cloudflare"]
        );
        let multiple = search_match_ranges(text, "乡音 cloudflare");
        assert_eq!(multiple.len(), 4);
        assert!(search_match_ranges(text, "不存在").is_empty());
        assert!(search_match_ranges(text, "   ").is_empty());
    }
}
