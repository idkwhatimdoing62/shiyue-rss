//! 桌面阅读器（egui/eframe，ADR-13）+ 内置抓取调度（ADR-14）+ 关窗到托盘（ADR-15）。
//! 三栏：源 | 文章 | 正文。正文按原文顺序穿插 文字/图片（ADR-16），图片原生纹理渲染。
//!
//! 进程模型：UI 在主线程；RSS Refresh 与 Knowledge Processing 各自通过窄 facade
//! 表达意图和 snapshot。后台模块只在短事务期间打开数据库，并通过 notice 请求 repaint。

use anyhow::Result;
use chrono::Utc;
use eframe::egui::{self, ViewportCommand};
use std::collections::{HashMap, HashSet};
use std::error::Error as _;
use std::hash::{Hash, Hasher};
use std::io::Read as _;
use std::ops::{Deref, DerefMut, Range};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tray_icon::menu::{Menu, MenuEvent, MenuId, MenuItem};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

use crate::article_library_lifecycle::{
    ArticleBatchAction, ArticleLibraryLifecycle, ArticleLibraryProjection, ArticleLifecycleChange,
    ChangeDisposition as ArticleChangeDisposition, LifecycleFailure, ProjectionScope,
};
use crate::backup::{BackupEntry, BackupProtection, BackupStore, DEFAULT_BACKUP_KEEP};
use crate::config::{Config, Paths};
use crate::db::Db;
use crate::feed_subscription::{
    ChangeDisposition, FeedSubscriptions, InitialRefreshOutcome, SubscriptionChange,
};
use crate::gui_modal::{self, InitialFocus, ModalHostAction};
use crate::gui_state::{
    ArticleCollection, DiscardOwner, ModalKind, ModalPayload, PanelPayload, Route, UiAction,
    UiEffect, UiState,
};
use crate::gui_theme::ReaderTheme;
use crate::image_store::{CacheStats, DEFAULT_LIMIT_BYTES, ImageStore};
use crate::knowledge_workflow::{
    ConnectionState, KnowledgeEngine, KnowledgeNotice, TaskKey, TaskKind as KnowledgeTaskKind,
    TaskSnapshot, TaskStage as KnowledgeTaskStage, TaskStatus as KnowledgeTaskStatus,
};
use crate::local_data_maintenance::{
    MaintenanceEngine, MaintenanceNotice, MaintenanceParticipant, MaintenanceRequest,
    MaintenanceSnapshot, MaintenanceStage, MaintenanceStatus,
};
use crate::model::{
    Article, ArticleSelection, Feed, SearchHistoryEntry, SearchHit, SearchHitKind, TextAnchor,
    resolve_excerpt_anchor,
};
use crate::notify;
use crate::rss_refresh_workflow::{
    RefreshNotice, RefreshRunStatus, RefreshWorkflowStatus, RssRefreshWorkflow, RunId,
};
use crate::text::{self, Block};

const WINDOW_TITLE: &str = "拾阅 · RSS 阅读器";
const FEED_PANEL_WIDTH: f32 = 240.0;
const ARTICLE_PANEL_WIDTH: f32 = 340.0;
const ARTICLE_MAX_WIDTH: f32 = 820.0;
const IMAGE_WORKER_COUNT: usize = 4;
const IMAGE_MAX_ATTEMPTS: u8 = 3;
const IMAGE_MAX_BYTES: u64 = 25 * 1024 * 1024;

pub fn run(paths: Paths, cfg: Config) -> Result<()> {
    let native = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(WINDOW_TITLE)
            .with_app_id("rrss-reading-optimized")
            // The two navigation columns are intentionally fixed-width; give
            // the reader enough initial room for a 780 px text measure.
            .with_inner_size([1440.0, 860.0])
            .with_min_inner_size([1120.0, 680.0]),
        ..Default::default()
    };
    eframe::run_native(
        WINDOW_TITLE,
        native,
        Box::new(move |cc| {
            install_cjk_font(&cc.egui_ctx);
            egui_extras::install_image_loaders(&cc.egui_ctx);
            install_style(&cc.egui_ctx);
            GuiApp::new(cc, &paths, cfg)
                .map(|a| Box::new(a) as Box<dyn eframe::App>)
                .map_err(Into::into)
        }),
    )
    .map_err(|e| anyhow::anyhow!("egui 启动失败: {e}"))
}

// ---------- 后台模块适配 ----------

/// GUI-only signals that are not part of RSS workflow state.
struct Shared {
    /// 窗口是否聚焦（UI 每帧写）；聚焦时不弹 toast。
    focused: AtomicBool,
}

// ---------- 托盘 ----------

fn build_tray() -> Result<(TrayIcon, MenuId, MenuId, MenuId)> {
    let menu = Menu::new();
    let toggle = MenuItem::new("显示 / 隐藏", true, None);
    let fetch = MenuItem::new("抓取一次", true, None);
    let quit = MenuItem::new("退出", true, None);
    menu.append(&toggle)?;
    menu.append(&fetch)?;
    menu.append(&quit)?;
    let tray = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip("拾阅")
        .with_icon(make_icon())
        .build()?;
    Ok((
        tray,
        toggle.id().clone(),
        fetch.id().clone(),
        quit.id().clone(),
    ))
}

/// 代码里生成一个纯色托盘图标，免得塞资源文件。ponytail: 够用，想要好看再换 png。
fn make_icon() -> Icon {
    let (w, h) = (32u32, 32u32);
    let mut rgba = Vec::with_capacity((w * h * 4) as usize);
    for _ in 0..(w * h) {
        rgba.extend_from_slice(&[0xE9, 0x5A, 0x2B, 0xFF]); // RSS 橙
    }
    Icon::from_rgba(rgba, w, h).expect("生成托盘图标失败")
}

const JB_MONO_REGULAR: &[u8] = include_bytes!("../assets/fonts/JetBrainsMono-Regular.ttf");
const JB_MONO_BOLD: &[u8] = include_bytes!("../assets/fonts/JetBrainsMono-Bold.ttf");
const LXGW_WENKAI_REGULAR: &[u8] = include_bytes!("../assets/fonts/LXGWWenKaiLite-Regular.ttf");
const LXGW_WENKAI_MEDIUM: &[u8] = include_bytes!("../assets/fonts/LXGWWenKaiLite-Medium.ttf");

/// Match the markdown editor's portable font stack: JetBrains Mono owns the
/// Latin glyphs and LXGW WenKai Lite supplies Chinese. The files are embedded
/// in the executable, so the layout no longer depends on the host machine.
fn install_cjk_font(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    for (name, bytes) in [
        ("jb-mono", JB_MONO_REGULAR),
        ("jb-mono-bold", JB_MONO_BOLD),
        ("lxgw-wenkai", LXGW_WENKAI_REGULAR),
        ("lxgw-wenkai-medium", LXGW_WENKAI_MEDIUM),
    ] {
        fonts.font_data.insert(
            name.to_owned(),
            egui::FontData::from_owned(bytes.to_vec()).into(),
        );
    }
    for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
        let names = fonts.families.entry(family).or_default();
        names.insert(0, "lxgw-wenkai".to_owned());
        names.insert(0, "jb-mono".to_owned());
    }
    fonts.families.insert(
        egui::FontFamily::Name("cjk-bold".into()),
        vec!["jb-mono-bold".to_owned(), "lxgw-wenkai-medium".to_owned()],
    );
    ctx.set_fonts(fonts);
}

fn install_style(ctx: &egui::Context) {
    let theme = ReaderTheme::sspai();
    ctx.all_styles_mut(|style| {
        style.text_styles.insert(
            egui::TextStyle::Body,
            egui::FontId::new(16.0, egui::FontFamily::Proportional),
        );
        style.text_styles.insert(
            egui::TextStyle::Button,
            egui::FontId::new(13.0, egui::FontFamily::Proportional),
        );
        style.text_styles.insert(
            egui::TextStyle::Small,
            egui::FontId::new(12.0, egui::FontFamily::Proportional),
        );
        style.spacing.item_spacing = egui::vec2(8.0, 7.0);
        style.spacing.button_padding = egui::vec2(9.0, 5.0);
        style.visuals.window_fill = theme.canvas;
        style.visuals.panel_fill = theme.panel;
        style.visuals.extreme_bg_color = theme.code_bg;
        style.visuals.faint_bg_color = theme.code_bg;
        style.visuals.hyperlink_color = theme.link;
        style.visuals.override_text_color = Some(theme.text);
        style.visuals.widgets.noninteractive.bg_stroke.color = theme.border;
        style.visuals.widgets.inactive.bg_fill = egui::Color32::TRANSPARENT;
        style.visuals.widgets.inactive.weak_bg_fill = egui::Color32::TRANSPARENT;
        style.visuals.widgets.hovered.weak_bg_fill = theme.accent.gamma_multiply(0.08);
        style.visuals.widgets.active.weak_bg_fill = theme.accent.gamma_multiply(0.16);
        style.visuals.selection.bg_fill = theme.accent.gamma_multiply(0.22);
        style.visuals.selection.stroke.color = theme.text;
        for widget in [
            &mut style.visuals.widgets.noninteractive,
            &mut style.visuals.widgets.inactive,
            &mut style.visuals.widgets.hovered,
            &mut style.visuals.widgets.active,
            &mut style.visuals.widgets.open,
        ] {
            widget.corner_radius = egui::CornerRadius::same(5);
        }
        // Only the article body opts into selection. This keeps sidebar,
        // metadata and saved-quote labels out of a cross-widget drag.
        style.interaction.selectable_labels = false;
        style.interaction.multi_widget_text_select = true;
    });
}

// ---------- App ----------

struct GuiApp {
    db: DbSlot,
    db_path: PathBuf,
    shared: Arc<Shared>,
    rss_refresh: RssRefreshWorkflow,
    rss_last_terminal_notice: Option<RunId>,
    notifications_enabled: bool,
    _tray: TrayIcon, // 持有，drop 即销毁托盘
    tray_toggle: MenuId,
    tray_fetch: MenuId,
    tray_quit: MenuId,
    feeds: Vec<(Feed, i64)>,
    articles: Vec<Article>,
    article_tags: HashMap<i64, Vec<String>>,
    /// 中栏当前展示普通订阅文章，还是统一的文章收藏库。
    /// 收藏库中的本地网页快照 id。用集合缓存，避免 UI 每帧逐条查库。
    web_clipping_ids: HashSet<i64>,
    saved_article_count: usize,
    read_later_count: usize,
    batch_mode: bool,
    batch_selection: HashSet<i64>,
    // 选中态存 id 而非下标，后台刷新重排后也不跳（ADR-14）。
    sel_article_id: Option<i64>,
    article_route_memory: HashMap<ArticleCollection, ArticleRouteMemory>,
    current_body_scroll: f32,
    hidden: bool,
    quitting: bool,
    body_article_id: Option<i64>,
    image_cache: HashMap<String, ImageState>,
    image_job_tx: std_mpsc::Sender<String>,
    image_event_rx: std_mpsc::Receiver<ImageEvent>,
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
    formula_cache: HashMap<String, FormulaState>,
    formula_job_tx: std_mpsc::Sender<FormulaJob>,
    formula_event_rx: std_mpsc::Receiver<FormulaEvent>,
    /// 左栏入口显示的有效摘录数量；写入或删除后立即刷新。
    saved_selection_count: usize,
    /// 已归档文章数量。
    archived_article_count: usize,
    pending_selection_anchor: Option<ArticleSelection>,
    pending_body_scroll: Option<(i64, f32)>,
    /// 快捷操作浮层中当前等待处理的选区。
    selection_popup_geometry: Option<SelectionPopupGeometry>,
    /// 每次新选区使用不同的浮层 id，避免旧浮层的点击关闭事件误伤新浮层。
    selection_popup_generation: u64,
    /// 跨标题、正文、列表和图片的文章级拖选状态。
    article_selection_drag: Option<ArticleSelectionDrag>,
    web_clip_event_tx: std_mpsc::Sender<WebClipEvent>,
    web_clip_event_rx: std_mpsc::Receiver<WebClipEvent>,
    web_clip_request_generation: u64,
    resource_query: String,
    resource_filter: ResourceFilter,
    resource_search_results: Vec<serde_json::Value>,
    resource_search_error: Option<String>,
    ai_api_key_draft: String,
    ai_settings_message: Option<String>,
    knowledge_engine: KnowledgeEngine,
    connection_state: Option<ConnectionState>,
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

#[derive(Debug)]
struct FeedAddDialog {
    url: String,
    error: Option<String>,
    focus_input: bool,
}

#[derive(Debug, Clone)]
struct FeedSettingsPanel {
    feed_id: i64,
    title: String,
    url: String,
    disabled: bool,
    original_disabled: bool,
    interval_draft: String,
    original_interval: String,
    error: Option<String>,
}

impl FeedSettingsPanel {
    fn from_feed(feed: &Feed) -> Self {
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
}

impl Default for FeedAddDialog {
    fn default() -> Self {
        Self {
            url: String::new(),
            error: None,
            focus_input: true,
        }
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
        Route::Articles(ArticleCollection::Saved) => Some(ProjectionScope::ArticleBookmarks),
        Route::Articles(ArticleCollection::ReadLater) => Some(ProjectionScope::ReadLater),
        Route::Articles(ArticleCollection::SearchResult(id)) => Some(ProjectionScope::Article(id)),
        Route::Articles(ArticleCollection::Feed(Some(id))) => Some(ProjectionScope::Feed(id)),
        Route::Archive => Some(ProjectionScope::Archive),
        Route::Articles(ArticleCollection::Feed(None))
        | Route::Resources
        | Route::Excerpts
        | Route::Storage => None,
    }
}

#[allow(clippy::too_many_arguments)]
fn adopt_article_projection_data(
    projection: ArticleLibraryProjection,
    articles: &mut Vec<Article>,
    article_tags: &mut HashMap<i64, Vec<String>>,
    fixed_bookmark_ids: &mut HashSet<i64>,
    saved_count: &mut usize,
    read_later_count: &mut usize,
    archived_count: &mut usize,
    feeds: &mut [(Feed, i64)],
) -> ProjectionScope {
    let ArticleLibraryProjection {
        scope,
        articles: projected_articles,
        tags,
        fixed_bookmark_ids: projected_fixed_ids,
        counts,
        feed_unread,
    } = projection;
    *articles = projected_articles;
    *article_tags = tags;
    *fixed_bookmark_ids = projected_fixed_ids;
    *saved_count = counts.bookmarks;
    *read_later_count = counts.read_later;
    *archived_count = counts.archived;
    for (feed_id, unread) in feed_unread {
        if let Some((_, current)) = feeds.iter_mut().find(|(feed, _)| feed.id == feed_id) {
            *current = i64::try_from(unread).unwrap_or(i64::MAX);
        }
    }
    scope
}

#[derive(Debug)]
struct ResourceAddDialog {
    url: String,
    note: String,
    private: bool,
    error: Option<String>,
    focus_input: bool,
}

impl Default for ResourceAddDialog {
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

#[derive(Debug, Clone, PartialEq, Eq)]
struct ResourceEditDialog {
    id: i64,
    title: String,
    purpose_zh: String,
    note: String,
    private: bool,
    rating: i64,
    original: ResourceEditValues,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ResourceEditValues {
    title: String,
    purpose_zh: String,
    note: String,
    private: bool,
    rating: i64,
}

#[derive(Debug, Clone, Copy, Default)]
struct ArticleRouteMemory {
    selected_article_id: Option<i64>,
    body_scroll: f32,
}

struct ResourceImportDialog {
    candidates: Vec<crate::resource_library_lifecycle::ImportCandidate>,
    selected: HashSet<i64>,
    initial_selected: HashSet<i64>,
}

#[derive(Debug, Default)]
struct SearchDialog {
    query: String,
    searched_query: String,
    results: Vec<SearchHit>,
    error: Option<String>,
    focus_input: bool,
    history: Vec<SearchHistoryEntry>,
}

#[derive(Debug, Clone)]
struct TagDialog {
    article_id: i64,
    draft: String,
    original: String,
    focus_input: bool,
}

#[derive(Debug)]
struct WebClipDialog {
    /// 可以是 http(s) 地址，也可以是用户粘贴的完整 HTML / HTML 片段。
    source: String,
    title: String,
    /// 粘贴 HTML 时用于解析相对链接；网址抓取模式会自动使用最终地址。
    base_url: String,
    fetching: bool,
    active_request: Option<u64>,
    error: Option<String>,
    focus_input: bool,
}

impl Default for WebClipDialog {
    fn default() -> Self {
        Self {
            source: String::new(),
            title: String::new(),
            base_url: String::new(),
            fetching: false,
            active_request: None,
            error: None,
            focus_input: true,
        }
    }
}

enum WebClipEvent {
    Complete {
        request_id: u64,
        result: Result<crate::web_clip::FetchedWebClip, String>,
    },
}

#[derive(Debug, Clone)]
struct DeleteWebClipDialog {
    article_id: i64,
    title: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SelectedQuote {
    article_id: i64,
    text: String,
    start_offset: Option<i64>,
    end_offset: Option<i64>,
    anchor_prefix: String,
    anchor_suffix: String,
}

struct CommentDialog {
    quote: SelectedQuote,
    draft: String,
    error: Option<String>,
    focus_input: bool,
}

enum ModalState {
    AddFeed(FeedAddDialog),
    DeleteFeed { id: i64, title: String },
    Search(SearchDialog),
    EditTags(TagDialog),
    WriteThought(CommentDialog),
    SaveWebPage(WebClipDialog),
    DeleteWebPage(DeleteWebClipDialog),
    AddResource(ResourceAddDialog),
    DeleteResource { id: i64, title: String },
    ImportResources(ResourceImportDialog),
    RestoreBackup(BackupEntry),
    ClearImages,
}

impl ModalPayload for ModalState {
    fn kind(&self) -> ModalKind {
        match self {
            Self::AddFeed(_) => ModalKind::AddFeed,
            Self::DeleteFeed { .. } => ModalKind::DeleteFeed,
            Self::Search(_) => ModalKind::Search,
            Self::EditTags(_) => ModalKind::EditTags,
            Self::WriteThought(_) => ModalKind::WriteThought,
            Self::SaveWebPage(_) => ModalKind::SaveWebPage,
            Self::DeleteWebPage(_) => ModalKind::DeleteWebPage,
            Self::AddResource(_) => ModalKind::AddResource,
            Self::DeleteResource { .. } => ModalKind::DeleteResource,
            Self::ImportResources(_) => ModalKind::ImportResources,
            Self::RestoreBackup(_) => ModalKind::RestoreBackup,
            Self::ClearImages => ModalKind::ClearImages,
        }
    }

    fn is_dirty(&self) -> bool {
        match self {
            Self::AddFeed(dialog) => !dialog.url.trim().is_empty(),
            Self::Search(_) => false,
            Self::EditTags(dialog) => dialog.draft != dialog.original,
            Self::WriteThought(dialog) => !dialog.draft.trim().is_empty(),
            Self::SaveWebPage(dialog) => {
                !dialog.source.trim().is_empty()
                    || !dialog.title.trim().is_empty()
                    || !dialog.base_url.trim().is_empty()
                    || dialog.fetching
            }
            Self::AddResource(dialog) => {
                !dialog.url.trim().is_empty() || !dialog.note.trim().is_empty() || dialog.private
            }
            Self::ImportResources(dialog) => dialog.selected != dialog.initial_selected,
            Self::DeleteFeed { .. }
            | Self::DeleteWebPage(_)
            | Self::DeleteResource { .. }
            | Self::RestoreBackup(_)
            | Self::ClearImages => false,
        }
    }

    fn active_request_id(&self) -> Option<u64> {
        match self {
            Self::SaveWebPage(dialog) => dialog.active_request,
            _ => None,
        }
    }
}

enum PanelState {
    ResourceEditor(ResourceEditDialog),
    FeedSettings(FeedSettingsPanel),
}

impl PanelPayload for PanelState {
    fn is_dirty(&self) -> bool {
        match self {
            Self::ResourceEditor(dialog) => {
                dialog.title != dialog.original.title
                    || dialog.purpose_zh != dialog.original.purpose_zh
                    || dialog.note != dialog.original.note
                    || dialog.private != dialog.original.private
                    || dialog.rating != dialog.original.rating
            }
            Self::FeedSettings(panel) => {
                panel.disabled != panel.original_disabled
                    || panel.interval_draft.trim() != panel.original_interval
            }
        }
    }

    fn is_compatible(&self, route: Route) -> bool {
        match self {
            Self::ResourceEditor(_) => route == Route::Resources,
            Self::FeedSettings(panel) => {
                route == Route::Articles(ArticleCollection::Feed(Some(panel.feed_id)))
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

#[derive(Debug, Clone, Copy)]
enum DiscardDecision {
    KeepEditing,
    Discard,
}

fn discard_guard_controls(ui: &mut egui::Ui, visible: bool) -> Option<DiscardDecision> {
    if !visible {
        return None;
    }
    let mut decision = None;
    ui.separator();
    ui.colored_label(egui::Color32::from_rgb(190, 86, 86), "有尚未保存的修改");
    ui.weak("继续刚才的操作会丢弃这些修改。");
    ui.horizontal(|ui| {
        if ui.button("继续编辑").clicked() {
            decision = Some(DiscardDecision::KeepEditing);
        }
        if ui.button("放弃修改").clicked() {
            decision = Some(DiscardDecision::Discard);
        }
    });
    decision
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ArticleDocCursor {
    span_index: usize,
    local_char: usize,
    char_index: usize,
}

#[derive(Debug, Clone)]
struct ArticleSelectionDrag {
    article_id: i64,
    anchor: ArticleDocCursor,
    focus: ArticleDocCursor,
}

struct RenderedArticleSpan {
    chars: Range<usize>,
    galley: Arc<egui::Galley>,
    global_from_galley: egui::emath::TSTransform,
    global_rect: egui::Rect,
    source_layer: egui::LayerId,
    /// Cursor calculated by egui while this row owns the pointer.  Keeping
    /// this local hit-test result avoids DPI/viewport transform differences
    /// between `Context::pointer_interact_pos` and a nested scroll layer.
    pointer_local_char: Option<usize>,
}

#[derive(Default)]
struct ArticleSelectionFrame {
    plain_text: String,
    char_len: usize,
    spans: Vec<RenderedArticleSpan>,
}

impl ArticleSelectionFrame {
    fn push_span(&mut self, text: &str, mut span: RenderedArticleSpan) {
        if text.is_empty() {
            return;
        }

        // Keep each rendered block as a distinct paragraph in the article
        // selection model.  The separator is deliberately kept outside the
        // span range so a drag ending at either edge never returns an
        // unexpected leading/trailing newline.
        if !self.plain_text.is_empty() {
            self.plain_text.push_str("\n\n");
            self.char_len += 2;
        }

        let start = self.char_len;
        self.plain_text.push_str(text);
        self.char_len += text.chars().count();
        span.chars = start..self.char_len;
        self.spans.push(span);
    }
}

struct ArticleSelectionResult {
    popup_request: Option<SelectionPopupRequest>,
    drag_started: bool,
}

/// A byte range in an article run that came from an HTML anchor.
///
/// Keeping this separate from `Block::Link` lets one rendered label retain
/// link styling while the article-level selection model treats it as plain
/// text for copying and quoting.
#[derive(Clone, Debug)]
struct ArticleLinkRange {
    range: Range<usize>,
    url: String,
}

#[derive(Debug)]
struct ImageFailure {
    message: String,
    detail: String,
    attempts: u8,
    retryable: bool,
}

enum ImageEvent {
    Progress {
        uri: String,
        attempt: u8,
    },
    Complete {
        uri: String,
        result: Result<Arc<[u8]>, ImageFailure>,
    },
}

enum ImageState {
    Loading {
        started: Instant,
        attempt: u8,
    },
    Ready {
        bytes: Arc<[u8]>,
        dimensions: Option<(u32, u32)>,
    },
    Failed(ImageFailure),
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

#[derive(Clone)]
struct FormulaJob {
    key: String,
    source: String,
    display: bool,
}

enum FormulaEvent {
    Complete {
        key: String,
        result: Result<Arc<[u8]>, String>,
    },
}

enum FormulaState {
    Loading,
    Ready(Arc<[u8]>),
    Failed(String),
}

impl GuiApp {
    fn has_modal_dialog(&self) -> bool {
        self.ui_state.has_modal()
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
                    self.article_selection_drag = None;
                    self.clear_selection_popover();
                    match to {
                        Route::Articles(_) => self.load_articles(),
                        Route::Archive => self.load_articles(),
                        Route::Storage => self.refresh_storage_overview(),
                        Route::Resources | Route::Excerpts => {}
                    }
                    if let Some(collection) = to.article_collection()
                        && let Some(memory) = self.article_route_memory.get(&collection).copied()
                        && memory
                            .selected_article_id
                            .is_some_and(|id| self.articles.iter().any(|article| article.id == id))
                    {
                        self.sel_article_id = memory.selected_article_id;
                        self.pending_body_scroll = memory
                            .selected_article_id
                            .map(|id| (id, memory.body_scroll));
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

    fn set_resource_panel(&mut self, dialog: Option<ResourceEditDialog>) {
        let panel = dialog.map(PanelState::ResourceEditor);
        let effects = self.ui_state.reduce(UiAction::SetPanel(panel));
        self.apply_ui_effects(effects);
    }

    fn resource_dialog(&self) -> Option<&ResourceEditDialog> {
        match self.ui_state.panel() {
            Some(PanelState::ResourceEditor(dialog)) => Some(dialog),
            Some(PanelState::FeedSettings(_)) | None => None,
        }
    }

    fn resource_dialog_mut(&mut self) -> Option<&mut ResourceEditDialog> {
        match self.ui_state.panel_mut() {
            Some(PanelState::ResourceEditor(dialog)) => Some(dialog),
            Some(PanelState::FeedSettings(_)) | None => None,
        }
    }

    fn set_feed_settings_panel(&mut self, feed: Option<Feed>) {
        let panel = feed.map(|feed| PanelState::FeedSettings(FeedSettingsPanel::from_feed(&feed)));
        let effects = self.ui_state.reduce(UiAction::SetPanel(panel));
        self.apply_ui_effects(effects);
    }

    fn feed_settings_panel(&self) -> Option<&FeedSettingsPanel> {
        match self.ui_state.panel() {
            Some(PanelState::FeedSettings(panel)) => Some(panel),
            Some(PanelState::ResourceEditor(_)) | None => None,
        }
    }

    fn feed_settings_panel_mut(&mut self) -> Option<&mut FeedSettingsPanel> {
        match self.ui_state.panel_mut() {
            Some(PanelState::FeedSettings(panel)) => Some(panel),
            Some(PanelState::ResourceEditor(_)) | None => None,
        }
    }

    fn notice(&mut self, message: impl Into<String>) {
        self.ui_state.show_notice(message, Instant::now());
    }

    fn apply_discard_decision(&mut self, decision: Option<DiscardDecision>) {
        match decision {
            Some(DiscardDecision::KeepEditing) => {
                self.ui_state.reduce(UiAction::KeepEditing);
            }
            Some(DiscardDecision::Discard) => {
                let effects = self.ui_state.reduce(UiAction::ConfirmDiscard);
                self.apply_ui_effects(effects);
            }
            None => {}
        }
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

    fn clear_selection_popover(&mut self) {
        self.ui_state.reduce(UiAction::SetPopover(None));
        self.selection_popup_geometry = None;
    }

    fn feed_add_dialog_mut(&mut self) -> Option<&mut FeedAddDialog> {
        match self.ui_state.modal_mut() {
            Some(ModalState::AddFeed(dialog)) => Some(dialog),
            _ => None,
        }
    }

    fn search_dialog(&self) -> Option<&SearchDialog> {
        match self.ui_state.modal() {
            Some(ModalState::Search(dialog)) => Some(dialog),
            _ => None,
        }
    }

    fn search_dialog_mut(&mut self) -> Option<&mut SearchDialog> {
        match self.ui_state.modal_mut() {
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

    fn comment_dialog_mut(&mut self) -> Option<&mut CommentDialog> {
        match self.ui_state.modal_mut() {
            Some(ModalState::WriteThought(dialog)) => Some(dialog),
            _ => None,
        }
    }

    fn web_clip_dialog_mut(&mut self) -> Option<&mut WebClipDialog> {
        match self.ui_state.modal_mut() {
            Some(ModalState::SaveWebPage(dialog)) => Some(dialog),
            _ => None,
        }
    }

    fn resource_add_dialog_mut(&mut self) -> Option<&mut ResourceAddDialog> {
        match self.ui_state.modal_mut() {
            Some(ModalState::AddResource(dialog)) => Some(dialog),
            _ => None,
        }
    }

    fn resource_import_dialog_mut(&mut self) -> Option<&mut ResourceImportDialog> {
        match self.ui_state.modal_mut() {
            Some(ModalState::ImportResources(dialog)) => Some(dialog),
            _ => None,
        }
    }

    fn new(cc: &eframe::CreationContext, paths: &Paths, cfg: Config) -> Result<Self> {
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
        let knowledge_engine =
            KnowledgeEngine::start(paths.db_file.clone(), resource_enrichment_config.clone())?;
        let shared = Arc::new(Shared {
            focused: AtomicBool::new(true),
        });
        let repaint = cc.egui_ctx.clone();
        let rss_refresh =
            RssRefreshWorkflow::start_scheduled(paths.db_file.clone(), cfg.clone(), move || {
                repaint.request_repaint()
            })?;

        let (tray, tray_toggle, tray_fetch, tray_quit) = build_tray()?;
        let (image_job_tx, image_job_rx) = std_mpsc::channel();
        let (image_event_tx, image_event_rx) = std_mpsc::channel();
        let (formula_job_tx, formula_job_rx) = std_mpsc::channel();
        let (formula_event_tx, formula_event_rx) = std_mpsc::channel();
        let (web_clip_event_tx, web_clip_event_rx) = std_mpsc::channel();
        let image_client = reqwest::blocking::Client::builder()
            // A single article can expose many CDN images at once. HTTP/1.1
            // plus a small worker pool is markedly steadier than opening an
            // unbounded number of HTTP/2 streams on flaky desktop networks.
            .http1_only()
            .connect_timeout(Duration::from_secs(8))
            .timeout(Duration::from_secs(30))
            .pool_idle_timeout(Duration::from_secs(30))
            .pool_max_idle_per_host(IMAGE_WORKER_COUNT)
            .redirect(reqwest::redirect::Policy::custom(|attempt| {
                if attempt.previous().len() > 10 {
                    return attempt.error("图片重定向次数过多");
                }
                if let Err(message) = crate::web_clip::validate_public_url(attempt.url()) {
                    return attempt.error(message);
                }
                attempt.follow()
            }))
            .user_agent(concat!("Shiyue/", env!("CARGO_PKG_VERSION")))
            .build()?;
        let image_store = Arc::new(ImageStore::open(&paths.image_cache_dir)?);
        let _ = image_store.prune_to(DEFAULT_LIMIT_BYTES);
        let participants: Vec<Arc<dyn MaintenanceParticipant>> = vec![
            rss_refresh.maintenance_participant(),
            knowledge_engine.maintenance_participant(),
        ];
        let maintenance_engine =
            MaintenanceEngine::start(paths.db_file.clone(), backup_store.clone(), participants)?;
        spawn_image_workers(
            image_client,
            image_job_rx,
            image_event_tx,
            image_store.clone(),
        );
        spawn_formula_worker(formula_job_rx, formula_event_tx);
        let restored_route = cc
            .storage
            .and_then(|storage| storage.get_string("shiyue.desktop.route"))
            .and_then(|value| Route::from_stable_key(&value))
            .unwrap_or_default();
        let mut ui_state = InteractionState::default();
        ui_state.initialize_route(restored_route);
        let mut app = GuiApp {
            db: DbSlot(Some(db)),
            db_path: paths.db_file.clone(),
            shared,
            rss_refresh,
            rss_last_terminal_notice: None,
            notifications_enabled: cfg.notifications,
            _tray: tray,
            tray_toggle,
            tray_fetch,
            tray_quit,
            feeds: Vec::new(),
            articles: Vec::new(),
            article_tags: HashMap::new(),
            web_clipping_ids: HashSet::new(),
            saved_article_count: 0,
            read_later_count: 0,
            batch_mode: false,
            batch_selection: HashSet::new(),
            sel_article_id: None,
            article_route_memory: HashMap::new(),
            current_body_scroll: 0.0,
            hidden: false,
            quitting: false,
            body_article_id: None,
            image_cache: HashMap::new(),
            image_job_tx,
            image_event_rx,
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
            formula_cache: HashMap::new(),
            formula_job_tx,
            formula_event_rx,
            saved_selection_count: 0,
            archived_article_count: 0,
            pending_selection_anchor: None,
            pending_body_scroll: None,
            selection_popup_geometry: None,
            selection_popup_generation: 0,
            article_selection_drag: None,
            web_clip_event_tx,
            web_clip_event_rx,
            web_clip_request_generation: 0,
            resource_query: String::new(),
            resource_filter: ResourceFilter::Active,
            resource_search_results: Vec::new(),
            resource_search_error: None,
            ai_api_key_draft: String::new(),
            ai_settings_message: None,
            knowledge_engine,
            connection_state: None,
        };
        app.reload();
        app.refresh_saved_selection_count();
        Ok(app)
    }

    fn refresh_saved_selection_count(&mut self) {
        self.saved_selection_count = self.db.saved_selection_count().unwrap_or_default();
    }

    fn refresh_article_library_metadata(&mut self) {
        let scope = self
            .current_article_projection_scope()
            .unwrap_or(ProjectionScope::ArticleBookmarks);
        match ArticleLibraryLifecycle::new(&self.db).project(scope) {
            Ok(projection) => self.adopt_article_library_metadata(&projection),
            Err(error) => self.report_article_library_failure("刷新文章资料", error),
        }
    }

    fn current_article_projection_scope(&self) -> Option<ProjectionScope> {
        projection_scope_for_route(self.ui_state.route())
    }

    fn adopt_article_library_metadata(&mut self, projection: &ArticleLibraryProjection) {
        self.saved_article_count = projection.counts.bookmarks;
        self.read_later_count = projection.counts.read_later;
        self.archived_article_count = projection.counts.archived;
        for (feed_id, unread) in &projection.feed_unread {
            if let Some((_, current)) = self.feeds.iter_mut().find(|(feed, _)| feed.id == *feed_id)
            {
                *current = i64::try_from(*unread).unwrap_or(i64::MAX);
            }
        }
    }

    fn adopt_article_library_projection(&mut self, projection: ArticleLibraryProjection) {
        let _scope = adopt_article_projection_data(
            projection,
            &mut self.articles,
            &mut self.article_tags,
            &mut self.web_clipping_ids,
            &mut self.saved_article_count,
            &mut self.read_later_count,
            &mut self.archived_article_count,
            &mut self.feeds,
        );
        if self
            .sel_article_id
            .is_some_and(|id| !self.articles.iter().any(|article| article.id == id))
        {
            self.sel_article_id = None;
            self.body_article_id = None;
            self.clear_selection_popover();
            self.article_selection_drag = None;
        }
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
        self.adopt_article_library_projection(outcome.projection);
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

        let active = crate::local_data_maintenance::WriterGate::maintenance_active(&self.db_path)
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
                    self.refresh_saved_selection_count();
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
                    if completed.new_article_count > 0
                        && self.notifications_enabled
                        && !self.shared.focused.load(Ordering::Relaxed)
                    {
                        notify::notify_new(feeds_with_new, completed.new_article_count);
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
        let connection_task = self.connection_state.clone();
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
                ui.heading("资料库与离线缓存");
                ui.label("资料默认保存在本机；图片缓存和备份均可独立清理。所有恢复都会先创建安全副本。");
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
                        egui::TextEdit::singleline(&mut self.ai_api_key_draft)
                            .password(true)
                            .hint_text("sk-…")
                            .desired_width(320.0),
                    );
                    if ui.button("保存 Key").clicked() {
                        match crate::resource_enrichment::save_api_key(&self.ai_api_key_draft) {
                            Ok(_) => {
                                self.ai_api_key_draft.clear();
                                self.ai_settings_message =
                                    Some("API Key 已保存到 Windows 凭据管理器".into());
                            }
                            Err(error) => self.ai_settings_message = Some(error.to_string()),
                        }
                    }
                    let connection_busy = matches!(
                        connection_task.as_ref(),
                        Some(ConnectionState::Running)
                    );
                    if ui
                        .add_enabled(!connection_busy, egui::Button::new("测试连接"))
                        .clicked()
                    {
                        self.begin_ai_connection_test(&ctx);
                    }
                    if ui.button("删除 Key").clicked() {
                        match crate::resource_enrichment::delete_api_key() {
                            Ok(_) => self.ai_settings_message = Some("API Key 已删除".into()),
                            Err(error) => self.ai_settings_message = Some(error.to_string()),
                        }
                    }
                });
                if let Some(message) = &self.ai_settings_message {
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
            Ok(feeds) => self.feeds = feeds,
            Err(error) => {
                tracing::warn!(detail = %error.technical_detail, "reload subscriptions failed");
                self.notice(error.user_message);
            }
        }
        if let Some(ArticleCollection::Feed(selected)) = self.ui_state.route().article_collection()
        {
            let selected_is_valid =
                selected.is_some_and(|id| self.feeds.iter().any(|(feed, _)| feed.id == id));
            if !selected_is_valid {
                let route = Route::Articles(ArticleCollection::Feed(
                    self.feeds.first().map(|(feed, _)| feed.id),
                ));
                let effects = self
                    .ui_state
                    .reduce(UiAction::ReplaceUnavailableRoute(route));
                self.apply_ui_effects(effects);
            }
        }
        self.load_articles();
    }

    fn show_feed_dialogs(&mut self, ctx: &egui::Context) {
        let kind = self.ui_state.modal_kind();
        let discard_pending = self.ui_state.discard_owner() == Some(DiscardOwner::Modal);
        match kind {
            Some(ModalKind::AddFeed) => {
                let Some(dialog) = self.feed_add_dialog_mut() else {
                    return;
                };
                let response =
                    gui_modal::show(ctx, ModalKind::AddFeed, discard_pending, |ui, focus| {
                        let mut submit = false;
                        let mut cancel = false;
                        ui.label("粘贴 RSS、Atom 或博客订阅地址");
                        let input = ui.add(
                            egui::TextEdit::singleline(&mut dialog.url)
                                .hint_text("https://example.com/feed.xml")
                                .desired_width(f32::INFINITY),
                        );
                        if focus == InitialFocus::PrimaryField && dialog.focus_input {
                            input.request_focus();
                            dialog.focus_input = false;
                        }
                        if let Some(error) = &dialog.error {
                            ui.colored_label(egui::Color32::RED, error);
                        }
                        ui.horizontal(|ui| {
                            if ui.button("添加并立即抓取").clicked()
                                || (input.lost_focus()
                                    && ui.input(|input| input.key_pressed(egui::Key::Enter)))
                            {
                                submit = true;
                            }
                            if ui.button("取消").clicked() {
                                cancel = true;
                            }
                        });
                        (submit.then(|| dialog.url.trim().to_owned()), cancel)
                    });
                self.apply_modal_host_action(response.action);
                let Some((add_url, cancel)) = response.inner else {
                    return;
                };
                if cancel {
                    self.close_modal();
                    return;
                }
                if let Some(url) = add_url {
                    match FeedSubscriptions::session(self.db_path.clone(), &self.rss_refresh)
                        .apply(SubscriptionChange::Add { url })
                    {
                        Ok(outcome) => {
                            let id = outcome
                                .subscription
                                .as_ref()
                                .expect("add returns a durable subscription")
                                .id;
                            self.complete_modal();
                            self.reload();
                            self.select_feed(id);
                            match outcome.refresh {
                                Some(InitialRefreshOutcome::Deferred) => {
                                    self.notice("订阅已添加，将在资料维护结束后刷新")
                                }
                                Some(InitialRefreshOutcome::Queued) => {
                                    self.notice("订阅已添加，正在抓取文章")
                                }
                                _ => self.notice("订阅已添加"),
                            }
                        }
                        Err(error) => {
                            tracing::warn!(detail = %error.technical_detail, "add subscription failed");
                            if let Some(dialog) = self.feed_add_dialog_mut() {
                                dialog.error = Some(error.user_message);
                            }
                        }
                    }
                }
            }
            Some(ModalKind::DeleteFeed) => {
                let target = match self.ui_state.modal() {
                    Some(ModalState::DeleteFeed { id, title }) => (*id, title.clone()),
                    _ => return,
                };
                let response = gui_modal::show(ctx, ModalKind::DeleteFeed, false, |ui, _| {
                    let mut delete = false;
                    let mut cancel = false;
                    ui.label(format!("确定删除订阅“{}”吗？", target.1));
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
                self.apply_modal_host_action(response.action);
                let Some((delete, cancel)) = response.inner else {
                    return;
                };
                if cancel {
                    self.complete_modal();
                } else if delete {
                    self.complete_modal();
                    match FeedSubscriptions::session(self.db_path.clone(), &self.rss_refresh).apply(
                        SubscriptionChange::Delete {
                            target: target.0.to_string(),
                        },
                    ) {
                        Ok(outcome) if outcome.disposition == ChangeDisposition::Deleted => {
                            self.reload();
                            self.notice("订阅已删除");
                        }
                        Ok(_) => self.notice("没有找到该订阅"),
                        Err(error) => {
                            tracing::warn!(detail = %error.technical_detail, "delete subscription failed");
                            self.notice(error.user_message);
                        }
                    }
                }
            }
            _ => {}
        }
    }

    fn load_articles(&mut self) {
        let Some(scope) = self.current_article_projection_scope() else {
            self.articles.clear();
            self.article_tags.clear();
            self.web_clipping_ids.clear();
            self.refresh_article_library_metadata();
            return;
        };
        match ArticleLibraryLifecycle::new(&self.db).project(scope) {
            Ok(projection) => self.adopt_article_library_projection(projection),
            Err(error) => {
                self.articles.clear();
                self.article_tags.clear();
                self.web_clipping_ids.clear();
                self.report_article_library_failure("读取文章资料", error);
            }
        }
    }

    fn select_feed(&mut self, id: i64) {
        let route = Route::Articles(ArticleCollection::Feed(Some(id)));
        if self.ui_state.route() == route {
            self.load_articles();
        } else {
            self.navigate(route);
        }
    }

    fn select_saved_articles(&mut self) {
        let route = Route::Articles(ArticleCollection::Saved);
        if self.ui_state.route() == route {
            self.load_articles();
        } else {
            self.navigate(route);
        }
    }

    fn select_read_later(&mut self) {
        let route = Route::Articles(ArticleCollection::ReadLater);
        if self.ui_state.route() == route {
            self.load_articles();
        } else {
            self.navigate(route);
        }
    }

    fn open_search(&mut self) {
        let history = self.db.search_history(12).unwrap_or_default();
        if self.search_dialog().is_none() {
            self.open_modal(ModalState::Search(SearchDialog::default()));
        }
        let Some(dialog) = self.search_dialog_mut() else {
            return;
        };
        dialog.focus_input = true;
        dialog.history = history;
        self.clear_selection_popover();
    }

    fn run_search(&mut self) {
        let Some(dialog) = self.search_dialog() else {
            return;
        };
        let query = dialog.query.trim().to_owned();
        let result = if query.is_empty() {
            None
        } else {
            Some(self.db.search_library(&query, 200))
        };
        let Some(dialog) = self.search_dialog_mut() else {
            return;
        };
        dialog.searched_query = query.clone();
        dialog.error = None;
        dialog.results.clear();
        if query.is_empty() {
            return;
        }
        match result.expect("non-empty search has a result") {
            Ok(results) => dialog.results = results,
            Err(error) => dialog.error = Some(format!("搜索失败：{error}")),
        }
    }

    fn open_search_result(&mut self, hit: &SearchHit) {
        let anchored_selection = hit
            .selection_id
            .and_then(|selection_id| self.db.get_selection(selection_id).ok());
        if hit.archived {
            self.navigate(Route::Articles(ArticleCollection::SearchResult(
                hit.article_id,
            )));
            if self
                .articles
                .iter()
                .any(|article| article.id == hit.article_id)
            {
                self.sel_article_id = Some(hit.article_id);
                self.body_article_id = None;
                self.clear_selection_popover();
                self.article_selection_drag = None;
                self.pending_selection_anchor = anchored_selection;
                self.complete_modal();
                self.notice("正在查看已归档文章（未恢复）");
            }
            return;
        }

        if matches!(hit.kind, SearchHitKind::WebClipping)
            || self.db.is_web_clipping(hit.article_id).unwrap_or(false)
        {
            self.select_saved_articles();
        } else {
            self.select_feed(hit.feed_id);
        }
        self.select_article(hit.article_id);
        self.pending_selection_anchor = anchored_selection;
        self.complete_modal();
        self.notice("已打开搜索结果");
    }

    /// 点开即已读（ADR-16），未读数同步减一。
    fn select_article(&mut self, id: i64) {
        if self.sel_article_id != Some(id) {
            self.body_article_id = None;
            self.current_body_scroll = 0.0;
            self.clear_selection_popover();
            self.article_selection_drag = None;
        }
        self.sel_article_id = Some(id);
        if self
            .articles
            .iter()
            .find(|article| article.id == id)
            .is_some_and(|article| !article.is_read)
            && let Err(error) = self.apply_article_library_change(ArticleLifecycleChange::SetRead {
                article_id: id,
                target: true,
            })
        {
            self.report_article_library_failure("标记已读", error);
        }
    }

    fn mark_unread(&mut self, id: i64) {
        if self
            .articles
            .iter()
            .find(|article| article.id == id)
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
        let Some(was_starred) = self
            .articles
            .iter()
            .find(|article| article.id == id)
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
        let current = self
            .articles
            .iter()
            .find(|article| article.id == id)
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
        let tags = if let Some(tags) = self.article_tags.get(&article_id) {
            tags.clone()
        } else {
            match ArticleLibraryLifecycle::new(&self.db)
                .project(ProjectionScope::Article(article_id))
            {
                Ok(projection) => projection
                    .tags
                    .get(&article_id)
                    .cloned()
                    .unwrap_or_default(),
                Err(error) => {
                    self.report_article_library_failure("读取标签", error);
                    return;
                }
            }
        };
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

    fn selected_article(&self) -> Option<&Article> {
        let id = self.sel_article_id?;
        self.articles.iter().find(|a| a.id == id)
    }

    fn open_web_clip_dialog(&mut self) {
        if self.ui_state.modal_kind() != Some(ModalKind::SaveWebPage) {
            self.open_modal(ModalState::SaveWebPage(WebClipDialog::default()));
        }
        self.clear_selection_popover();
    }

    fn begin_web_clip_import(&mut self, ctx: &egui::Context) {
        self.web_clip_request_generation = self.web_clip_request_generation.wrapping_add(1);
        let next_request_id = self.web_clip_request_generation;
        let Some(dialog) = self.web_clip_dialog_mut() else {
            return;
        };
        if dialog.fetching {
            return;
        }
        let source = dialog.source.trim().to_owned();
        if source.is_empty() {
            dialog.error = Some("请粘贴网页地址或 HTML".to_owned());
            return;
        }
        dialog.error = None;

        if let Some(fetch_source) = normalized_web_url(&source) {
            let request_id = next_request_id;
            dialog.fetching = true;
            dialog.active_request = Some(request_id);
            let event_tx = self.web_clip_event_tx.clone();
            let repaint = ctx.clone();
            std::thread::spawn(move || {
                let result = crate::web_clip::client()
                    .and_then(|client| crate::web_clip::fetch_html(&client, &fetch_source))
                    .map_err(|error| error.to_string());
                let _ = event_tx.send(WebClipEvent::Complete { request_id, result });
                repaint.request_repaint();
            });
            return;
        }
        if !source.trim_start().starts_with('<')
            && source.lines().count() == 1
            && (source.contains("://") || source.to_ascii_lowercase().starts_with("http:"))
        {
            dialog.error = Some("网页地址格式不正确，只支持 http:// 或 https://".to_owned());
            return;
        }

        let title_override = non_empty_owned(&dialog.title);
        let explicit_base = non_empty_owned(&dialog.base_url);
        match prepare_pasted_web_clip(&source, explicit_base.as_deref()) {
            Ok((snapshot_title, content)) => {
                let title = title_override
                    .or(snapshot_title)
                    .unwrap_or_else(|| "未命名网页".to_owned());
                self.finish_web_clip_save(None, &title, &content);
            }
            Err(error) => dialog.error = Some(error),
        }
    }

    fn receive_web_clip_events(&mut self, ctx: &egui::Context) {
        while let Ok(event) = self.web_clip_event_rx.try_recv() {
            let WebClipEvent::Complete { request_id, result } = event;
            if !self
                .ui_state
                .accepts_modal_event(ModalKind::SaveWebPage, request_id)
            {
                continue;
            }
            let Some(dialog) = self.web_clip_dialog_mut() else {
                continue;
            };
            dialog.fetching = false;
            dialog.active_request = None;
            match result {
                Ok(fetched) => {
                    let snapshot = text::prepare_html_snapshot(&fetched.html);
                    if snapshot.content.trim().is_empty() {
                        dialog.error = Some("网页抓取成功，但没有识别到可阅读正文".to_owned());
                        continue;
                    }
                    let title = non_empty_owned(&dialog.title)
                        .or(snapshot.title)
                        .unwrap_or_else(|| fetched.original_url.clone());
                    let effective_base = snapshot
                        .base_href
                        .as_deref()
                        .and_then(|base| resolve_http_url(base, Some(&fetched.final_url)))
                        .or_else(|| Some(fetched.final_url.clone()));
                    let content = with_html_base(&snapshot.content, effective_base.as_deref());
                    self.finish_web_clip_save(Some(&fetched.original_url), &title, &content);
                }
                Err(error) => {
                    dialog.error = Some(format!("抓取失败：{error}"));
                }
            }
            ctx.request_repaint();
        }
    }

    fn receive_knowledge_updates(&mut self, ctx: &egui::Context) {
        let notices = self.knowledge_engine.try_notices().collect::<Vec<_>>();
        for notice in notices {
            match notice {
                KnowledgeNotice::Changed(key) => {
                    let snapshot = self.knowledge_engine.snapshot(key).ok().flatten();
                    let notice = snapshot
                        .as_ref()
                        .and_then(|snapshot| match snapshot.status {
                            KnowledgeTaskStatus::Succeeded => match snapshot.key.kind {
                                KnowledgeTaskKind::ResourceCompletion => {
                                    Some("资源抓取和 AI 整理已完成".to_owned())
                                }
                                KnowledgeTaskKind::ArticleSummary => {
                                    Some("AI 总结和中文翻译已保存".to_owned())
                                }
                            },
                            KnowledgeTaskStatus::Failed | KnowledgeTaskStatus::Interrupted => {
                                snapshot.user_message.clone()
                            }
                            KnowledgeTaskStatus::Queued | KnowledgeTaskStatus::Running => None,
                        });
                    if let Some(notice) = notice {
                        self.notice(notice);
                    }
                }
                KnowledgeNotice::ConnectionChanged(state) => {
                    self.ai_settings_message = Some(match &state {
                        ConnectionState::Running => "正在测试 DeepSeek 连接".to_owned(),
                        ConnectionState::Succeeded(message) => message.clone(),
                        ConnectionState::Failed { detail } => format!("连接失败：{detail}"),
                    });
                    self.connection_state = Some(state);
                }
                KnowledgeNotice::ModuleFault {
                    user_message,
                    technical_detail,
                } => {
                    tracing::warn!("knowledge module: {technical_detail}");
                    self.notice(user_message);
                }
            }
        }
        ctx.request_repaint();
    }

    fn knowledge_task(&self, kind: KnowledgeTaskKind, target_id: i64) -> Option<TaskSnapshot> {
        self.knowledge_engine
            .snapshot(TaskKey::new(kind, target_id))
            .ok()
            .flatten()
    }

    fn retry_resource_task(&mut self, resource_id: i64, _ctx: &egui::Context) {
        let result = self.knowledge_engine.request(TaskKey::new(
            KnowledgeTaskKind::ResourceCompletion,
            resource_id,
        ));
        match result {
            Ok(_) => {
                self.notice("已重新加入后台处理队列");
            }
            Err(error) => {
                self.notice(format!("无法重试后台任务：{error:#}"));
            }
        }
    }

    fn begin_ai_connection_test(&mut self, ctx: &egui::Context) {
        match self.knowledge_engine.test_connection() {
            Ok(()) => {
                self.connection_state = Some(ConnectionState::Running);
                ctx.request_repaint();
            }
            Err(error) => {
                self.connection_state = Some(ConnectionState::Failed {
                    detail: format!("{error:#}"),
                });
            }
        }
    }

    fn begin_article_ai(&mut self, article_id: i64, _ctx: &egui::Context) {
        let result = self
            .knowledge_engine
            .request(TaskKey::new(KnowledgeTaskKind::ArticleSummary, article_id));
        match result {
            Ok(_) => {}
            Err(error) => {
                self.notice(format!("无法提交文章 AI 任务：{error:#}"));
            }
        }
    }

    fn finish_web_clip_save(&mut self, source_url: Option<&str>, title: &str, content: &str) {
        match self
            .db
            .save_web_clipping(source_url, Some(title), content, Utc::now().timestamp())
        {
            Ok(article_id) => {
                self.complete_modal();
                self.select_saved_articles();
                self.select_article(article_id);
                self.notice("正文快照已保存到本机；网页图片仍需联网加载");
            }
            Err(error) => {
                if let Some(dialog) = self.web_clip_dialog_mut() {
                    dialog.error = Some(format!("保存失败：{error}"));
                } else {
                    self.notice(format!("网页保存失败：{error}"));
                }
            }
        }
    }

    fn remove_saved_article(&mut self, id: i64) {
        if self.web_clipping_ids.contains(&id) {
            let title = self
                .articles
                .iter()
                .find(|article| article.id == id)
                .and_then(|article| article.title.clone())
                .unwrap_or_else(|| "未命名网页".to_owned());
            self.open_modal(ModalState::DeleteWebPage(DeleteWebClipDialog {
                article_id: id,
                title,
            }));
        } else {
            self.toggle_star(id);
        }
    }

    fn save_favorite_quote(&mut self, quote: SelectedQuote) {
        let anchor = TextAnchor {
            start_offset: quote.start_offset,
            end_offset: quote.end_offset,
            prefix: quote.anchor_prefix.clone(),
            suffix: quote.anchor_suffix.clone(),
        };
        let result = self.db.add_favorite_selection_with_anchor(
            quote.article_id,
            &quote.text,
            &anchor,
            Utc::now().timestamp(),
        );
        match result {
            Ok(_) => {
                self.refresh_saved_selection_count();
                self.notice("已摘录，可在左侧「摘录与想法」查看");
            }
            Err(error) => {
                self.notice(format!("摘录失败：{error}"));
            }
        }
    }

    fn begin_comment(&mut self, quote: SelectedQuote) {
        self.open_modal(ModalState::WriteThought(CommentDialog {
            quote,
            draft: String::new(),
            error: None,
            focus_input: true,
        }));
    }

    fn submit_comment(&mut self) {
        let Some(ModalState::WriteThought(dialog)) = self.ui_state.modal() else {
            return;
        };
        let quote = dialog.quote.clone();
        let draft = dialog.draft.clone();
        if draft.trim().is_empty() {
            if let Some(dialog) = self.comment_dialog_mut() {
                dialog.error = Some("想法内容不能为空".to_owned());
            }
            return;
        }
        let anchor = TextAnchor {
            start_offset: quote.start_offset,
            end_offset: quote.end_offset,
            prefix: quote.anchor_prefix.clone(),
            suffix: quote.anchor_suffix.clone(),
        };
        let result = self.db.add_comment_with_anchor(
            quote.article_id,
            &quote.text,
            &anchor,
            &draft,
            Utc::now().timestamp(),
        );
        match result {
            Ok(_) => {
                self.complete_modal();
                self.refresh_saved_selection_count();
                self.notice("想法已保存，可在左侧「摘录与想法」查看");
            }
            Err(error) => {
                if let Some(dialog) = self.comment_dialog_mut() {
                    dialog.error = Some(format!("想法保存失败：{error}"));
                }
            }
        }
    }

    fn show_comment_dialog(&mut self, ctx: &egui::Context) {
        if self.ui_state.modal_kind() != Some(ModalKind::WriteThought) {
            return;
        }
        let show_discard = self.ui_state.discard_owner() == Some(DiscardOwner::Modal);
        let Some(dialog) = self.comment_dialog_mut() else {
            return;
        };
        let mut submit = false;
        let mut cancel = false;
        let response = gui_modal::show(ctx, ModalKind::WriteThought, show_discard, |ui, focus| {
            ui.label("选中的文字：");
            egui::Frame::group(ui.style()).show(ui, |ui| {
                ui.add(
                    egui::Label::new(
                        egui::RichText::new(&dialog.quote.text)
                            .size(15.0)
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
        });
        self.apply_modal_host_action(response.action);
        if cancel {
            self.close_modal();
        } else if submit {
            self.submit_comment();
        }
    }

    fn show_web_clip_dialog(&mut self, ctx: &egui::Context) {
        if self.ui_state.modal_kind() != Some(ModalKind::SaveWebPage) {
            return;
        }
        let show_discard = self.ui_state.discard_owner() == Some(DiscardOwner::Modal);
        let Some(dialog) = self.web_clip_dialog_mut() else {
            return;
        };
        let theme = ReaderTheme::sspai();
        let mut import = false;
        let mut cancel = false;
        let response = gui_modal::show(ctx, ModalKind::SaveWebPage, show_discard, |ui, focus| {
            ui.label(
                egui::RichText::new("粘贴网页地址，或直接粘贴 HTML 源码")
                    .size(15.0)
                    .color(theme.text),
            );
            ui.add_space(4.0);
            ui.label(
                egui::RichText::new("正文会作为本地快照保存；网页中的远程图片仍需要联网加载。")
                    .size(12.0)
                    .color(theme.muted),
            );
            ui.add_space(12.0);
            ui.label("网页地址 / HTML");
            let source_response = ui.add_enabled(
                !dialog.fetching,
                egui::TextEdit::multiline(&mut dialog.source)
                    .desired_rows(10)
                    .desired_width(f32::INFINITY)
                    .hint_text("https://example.com/article\n\n或\n\n<article>…</article>"),
            );
            if focus == InitialFocus::PrimaryField && dialog.focus_input {
                source_response.request_focus();
                dialog.focus_input = false;
            }
            if source_response.changed() {
                dialog.error = None;
            }
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                ui.label("标题（可选）");
                ui.add_enabled(
                    !dialog.fetching,
                    egui::TextEdit::singleline(&mut dialog.title)
                        .desired_width(ui.available_width())
                        .hint_text("留空则从 HTML 自动识别"),
                );
            });
            ui.horizontal(|ui| {
                ui.label("基础网址（可选）");
                ui.add_enabled(
                    !dialog.fetching,
                    egui::TextEdit::singleline(&mut dialog.base_url)
                        .desired_width(ui.available_width())
                        .hint_text("仅粘贴 HTML 时，用于解析相对图片和链接"),
                );
            });
            if let Some(error) = &dialog.error {
                ui.add_space(6.0);
                ui.label(egui::RichText::new(error).color(theme.link).size(12.0));
            }
            ui.add_space(12.0);
            ui.horizontal(|ui| {
                let import_label = if dialog.fetching {
                    "正在抓取网页…"
                } else {
                    "保存网页"
                };
                if ui
                    .add_enabled(
                        !dialog.fetching,
                        egui::Button::new(import_label)
                            .fill(theme.accent)
                            .stroke(egui::Stroke::NONE),
                    )
                    .clicked()
                {
                    import = true;
                }
                if dialog.fetching {
                    ui.spinner();
                }
                let cancel_label = if dialog.fetching {
                    "关闭窗口"
                } else {
                    "取消"
                };
                if ui.button(cancel_label).clicked() {
                    cancel = true;
                }
            });
        });
        self.apply_modal_host_action(response.action);
        if cancel {
            self.close_modal();
        } else if import {
            self.begin_web_clip_import(ctx);
        }
    }

    fn show_delete_web_clip_dialog(&mut self, ctx: &egui::Context) {
        if self.ui_state.modal_kind() != Some(ModalKind::DeleteWebPage) {
            return;
        }
        let Some(ModalState::DeleteWebPage(dialog)) = self.ui_state.modal() else {
            return;
        };
        let dialog = dialog.clone();
        let mut confirm = false;
        let mut cancel = false;
        let response = gui_modal::show(ctx, ModalKind::DeleteWebPage, false, |ui, _| {
            ui.label(format!("确定永久删除「{}」吗？", dialog.title));
            ui.weak("正文快照及其摘录、想法会一起删除，无法撤销。");
            ui.add_space(12.0);
            ui.horizontal(|ui| {
                if ui.button("永久删除").clicked() {
                    confirm = true;
                }
                if ui.button("取消").clicked() {
                    cancel = true;
                }
            });
        });
        self.apply_modal_host_action(response.action);
        if confirm {
            match self.db.delete_web_clipping(dialog.article_id) {
                Ok(changed) if changed > 0 => {
                    if self.sel_article_id == Some(dialog.article_id) {
                        self.sel_article_id = None;
                        self.body_article_id = None;
                    }
                    self.load_articles();
                    self.refresh_saved_selection_count();
                    self.notice("本地网页已永久删除");
                }
                Ok(_) => {
                    self.notice("网页不存在或已经删除");
                }
                Err(error) => {
                    self.notice(format!("删除失败：{error}"));
                }
            }
            self.complete_modal();
        } else if cancel {
            self.complete_modal();
        }
    }

    fn show_resource_library_page(&mut self, root_ui: &mut egui::Ui) {
        if self.ui_state.route() != Route::Resources {
            return;
        }
        let ctx = root_ui.ctx().clone();
        use crate::resource_library_lifecycle::{
            ResourceCollection, ResourceCurationState, ResourceHealth, ResourceLibraryLifecycle,
            ResourcePrivacy, SystemClock,
        };
        let collection = match self.resource_filter {
            ResourceFilter::Active => ResourceCollection::Active,
            ResourceFilter::PendingReview => ResourceCollection::PendingReview,
            ResourceFilter::Broken => ResourceCollection::Broken,
            ResourceFilter::Archived => ResourceCollection::Archived,
        };
        let projection =
            ResourceLibraryLifecycle::new(&self.db, &self.knowledge_engine, &SystemClock).project(
                crate::resource_library_lifecycle::ProjectionScope::collection(collection),
            );
        let (rows, counts, projection_error) = match projection {
            Ok(projection) => (projection.resources, projection.counts, None),
            Err(error) => (
                Vec::new(),
                crate::resource_library_lifecycle::ResourceLibraryCounts::default(),
                Some(error.to_string()),
            ),
        };
        enum Action {
            Open(String),
            Edit(Box<crate::resource_library_lifecycle::Resource>),
            Transition(i64, ResourceCurationState),
            Delete(i64),
            Retry(i64, String),
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
                if self.resource_dialog().is_some() {
                    egui::Panel::right("resource-editor")
                        .resizable(true)
                        .default_size(480.0)
                        .size_range(360.0..=720.0)
                        .show(ui, |ui| {
                            ui.set_min_width(ui.available_width());
                            self.show_resource_editor(ui);
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
                        self.open_modal(ModalState::AddResource(ResourceAddDialog::default()));
                    }
                    if ui.button("导入网页收藏").clicked() {
                        match ResourceLibraryLifecycle::new(
                            &self.db,
                            &self.knowledge_engine,
                            &SystemClock,
                        )
                        .preview_web_clipping_import()
                        {
                            Ok(candidates) => {
                                let selected = candidates
                                    .iter()
                                    .filter(|item| !item.already_imported)
                                    .map(|item| item.article_id)
                                    .collect::<HashSet<_>>();
                                self.open_modal(ModalState::ImportResources(
                                    ResourceImportDialog {
                                        initial_selected: selected.clone(),
                                        selected,
                                        candidates,
                                    },
                                ));
                            }
                            Err(error) => self.notice(format!("读取网页收藏失败：{error}")),
                        }
                    }
                    ui.add(
                        egui::TextEdit::singleline(&mut self.resource_query)
                            .hint_text("搜索标题、URL、用途或备注")
                            .desired_width(280.0),
                    );
                    if ui.button("搜索资源和文章").clicked() {
                        match crate::resource_library_lifecycle::legacy_search_json(
                            &self.db,
                            &self.resource_query,
                            true,
                            true,
                            false,
                            20,
                        ) {
                            Ok(results) => {
                                self.resource_search_results = results;
                                self.resource_search_error = None;
                            }
                            Err(error) => {
                                self.resource_search_results.clear();
                                self.resource_search_error = Some(error.to_string());
                            }
                        }
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
                if let Some(error) = &self.resource_search_error {
                    ui.colored_label(egui::Color32::RED, format!("搜索失败：{error}"));
                }
                if let Some(error) = &projection_error {
                    ui.colored_label(egui::Color32::RED, format!("读取资源失败：{error}"));
                }
                if !self.resource_query.trim().is_empty() {
                    ui.weak(format!(
                        "与 CLI 相同的统一搜索结果：{} 条",
                        self.resource_search_results.len()
                    ));
                    for result in &self.resource_search_results {
                        let kind = if result["result_type"] == "article" {
                            "收藏文章"
                        } else {
                            "网站资源"
                        };
                        ui.horizontal_wrapped(|ui| {
                            ui.label(format!("[{kind}]"));
                            ui.strong(result["title"].as_str().unwrap_or("未命名"));
                            if let Some(url) = result["url"].as_str() {
                                ui.hyperlink_to("打开", url);
                            }
                            if let Some(purpose) = result["purpose_zh"].as_str() {
                                ui.weak(purpose);
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
                    for resource in &rows {
                        resource_card(ui, |ui| {
                            ui.horizontal(|ui| {
                                ui.vertical(|ui| {
                                    ui.strong(
                                        resource.title.as_deref().unwrap_or("尚未整理的网站"),
                                    );
                                    ui.hyperlink_to(&resource.url, &resource.url);
                                    if let Some(purpose) = &resource.purpose_zh {
                                        ui.label(purpose);
                                    }
                                    if let Some(note) = &resource.private_note {
                                        ui.weak(format!("备注：{note}"));
                                    }
                                    ui.weak(format!(
                                        "状态：{}   隐私：{}   评分：{}",
                                        match (resource.curation_state, resource.health) {
                                            (ResourceCurationState::PendingReview, _) =>
                                                "等待你确认",
                                            (ResourceCurationState::Archived, _) => "已归档",
                                            (_, ResourceHealth::Broken) => "已收藏，源站失效",
                                            (_, ResourceHealth::Unknown) => "已收藏，尚未检查",
                                            (_, ResourceHealth::Healthy) => "已收藏，可供 AI 搜索",
                                        },
                                        if resource.privacy == ResourcePrivacy::Private {
                                            "私密"
                                        } else {
                                            "公开"
                                        },
                                        resource
                                            .manual_rating
                                            .map_or("未设置".into(), |v| format!("{v}/5"))
                                    ));
                                });
                            });
                            ui.horizontal(|ui| {
                                if ui.button("访问网站").clicked() {
                                    action = Some(Action::Open(resource.url.clone()));
                                }
                                if ui.button("编辑信息").clicked() {
                                    action = Some(Action::Edit(Box::new(resource.clone())));
                                }
                                match resource.curation_state {
                                    ResourceCurationState::PendingReview => {
                                        if ui.button("确认收藏，让 AI 能搜到").clicked() {
                                            action = Some(Action::Transition(
                                                resource.id,
                                                ResourceCurationState::Active,
                                            ));
                                        }
                                    }
                                    ResourceCurationState::Active => {
                                        if ui.button("归档").clicked() {
                                            action = Some(Action::Transition(
                                                resource.id,
                                                ResourceCurationState::Archived,
                                            ));
                                        }
                                    }
                                    ResourceCurationState::Archived => {
                                        if ui.button("恢复").clicked() {
                                            action = Some(Action::Transition(
                                                resource.id,
                                                ResourceCurationState::Active,
                                            ));
                                        }
                                    }
                                }
                                if resource.privacy == ResourcePrivacy::Public
                                    && resource.curation_state != ResourceCurationState::Archived
                                    && ui
                                        .button(if resource.purpose_zh.is_none() {
                                            "补全描述"
                                        } else {
                                            "重新补全信息"
                                        })
                                        .clicked()
                                {
                                    action = Some(Action::Retry(resource.id, resource.url.clone()));
                                }
                                if matches!(
                                    resource.curation_state,
                                    ResourceCurationState::PendingReview
                                        | ResourceCurationState::Archived
                                ) && ui.button("永久删除").clicked()
                                {
                                    action = Some(Action::Delete(resource.id));
                                }
                            });
                        });
                        ui.add_space(6.0);
                    }
                    if rows.is_empty() {
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
                let values = ResourceEditValues {
                    title: resource.title.unwrap_or_default(),
                    purpose_zh: resource.purpose_zh.unwrap_or_default(),
                    note: resource.private_note.unwrap_or_default(),
                    private: resource.privacy == ResourcePrivacy::Private,
                    rating: resource.manual_rating.unwrap_or(0),
                };
                self.set_resource_panel(Some(ResourceEditDialog {
                    id: resource.id,
                    title: values.title.clone(),
                    purpose_zh: values.purpose_zh.clone(),
                    note: values.note.clone(),
                    private: values.private,
                    rating: values.rating,
                    original: values,
                }));
            }
            Some(Action::Transition(id, status)) => {
                match ResourceLibraryLifecycle::new(&self.db, &self.knowledge_engine, &SystemClock)
                    .apply(
                    crate::resource_library_lifecycle::ResourceLifecycleChange::SetCurationState {
                        resource_id: id,
                        target: status,
                    },
                    crate::resource_library_lifecycle::ProjectionScope::collection(collection),
                ) {
                    Ok(_) => {
                        self.notice("资源状态已更新");
                    }
                    Err(e) => self.notice(format!("操作失败：{e}")),
                }
            }
            Some(Action::Delete(id)) => {
                let title =
                    ResourceLibraryLifecycle::new(&self.db, &self.knowledge_engine, &SystemClock)
                        .project(crate::resource_library_lifecycle::ProjectionScope::Resource(id))
                        .ok()
                        .and_then(|projection| projection.detail)
                        .and_then(|detail| detail.resource.title)
                        .unwrap_or_else(|| format!("资源 #{id}"));
                self.open_modal(ModalState::DeleteResource { id, title });
            }
            Some(Action::Retry(id, url)) => {
                let _ = url;
                self.retry_resource_task(id, &ctx);
            }
            None => {}
        }
    }

    fn show_resource_add_dialog(&mut self, ctx: &egui::Context) {
        use crate::resource_library_lifecycle::{
            CreateResource, ResourceCollection, ResourceKind, ResourceLibraryLifecycle,
            ResourceLifecycleChange, ResourcePrivacy, ResourceSource, SystemClock,
        };
        if self.ui_state.modal_kind() != Some(ModalKind::AddResource) {
            return;
        }
        let show_discard = self.ui_state.discard_owner() == Some(DiscardOwner::Modal);
        let Some(dialog) = self.resource_add_dialog_mut() else {
            return;
        };
        let mut save = false;
        let response = gui_modal::show(ctx, ModalKind::AddResource, show_discard, |ui, focus| {
            ui.label("网址（唯一必填项）");
            let input = ui.add(
                egui::TextEdit::singleline(&mut dialog.url)
                    .desired_width(f32::INFINITY)
                    .hint_text("https://koboyo.com/icons?q=app+icon"),
            );
            if focus == InitialFocus::PrimaryField && dialog.focus_input {
                input.request_focus();
                dialog.focus_input = false;
            }
            ui.label("私人备注（可选）");
            ui.add(
                egui::TextEdit::multiline(&mut dialog.note)
                    .desired_rows(3)
                    .desired_width(f32::INFINITY),
            );
            ui.checkbox(&mut dialog.private, "私密资源（永不发送到云端 AI）");
            if let Some(error) = &dialog.error {
                ui.colored_label(egui::Color32::RED, error);
            }
            if ui.button("立即保存").clicked() {
                save = true;
            }
        });
        let input = save.then(|| CreateResource {
            url: dialog.url.clone(),
            parent_resource_id: None,
            linked_article_id: None,
            kind: ResourceKind::Page,
            title: None,
            private_note: non_empty_owned(&dialog.note),
            privacy: if dialog.private {
                ResourcePrivacy::Private
            } else {
                ResourcePrivacy::Public
            },
            source: ResourceSource::Gui,
            manual_rating: None,
        });
        self.apply_modal_host_action(response.action);
        if let Some(input) = input {
            match ResourceLibraryLifecycle::new(&self.db, &self.knowledge_engine, &SystemClock)
                .apply(
                    ResourceLifecycleChange::Create(input),
                    crate::resource_library_lifecycle::ProjectionScope::collection(
                        ResourceCollection::Active,
                    ),
                ) {
                Ok(_) => {
                    self.complete_modal();
                    self.notice("资源网址已保存；断网也不会丢失");
                }
                Err(e) => {
                    if let Some(dialog) = self.resource_add_dialog_mut() {
                        dialog.error = Some(e.to_string());
                    }
                }
            }
        }
    }

    fn show_resource_editor(&mut self, ui: &mut egui::Ui) {
        let show_discard = self.ui_state.discard_owner() == Some(DiscardOwner::Panel);
        let task = self.resource_dialog().and_then(|dialog| {
            self.knowledge_task(KnowledgeTaskKind::ResourceCompletion, dialog.id)
        });
        let details = self.resource_dialog().and_then(|dialog| {
            let service = crate::resource_library_lifecycle::ResourceLibraryLifecycle::new(
                &self.db,
                &self.knowledge_engine,
                &crate::resource_library_lifecycle::SystemClock,
            );
            service
                .project(crate::resource_library_lifecycle::ProjectionScope::Resource(dialog.id))
                .ok()
                .and_then(|projection| projection.detail)
                .map(|detail| (detail.resource, detail.categories, detail.tags))
        });
        let Some(dialog) = self.resource_dialog_mut() else {
            return;
        };
        let resource_id = dialog.id;
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
        if let Some(task) = &task {
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
                    enrich = true;
                }
            } else {
                ui.label("AI 信息已更新");
            }
            ui.separator();
        }
        if let Some((resource, categories, tags)) = &details {
            ui.label(egui::RichText::new("AI 补全信息").strong());
            let has_ai_details = resource.purpose_zh.is_some()
                || resource.use_when_zh.is_some()
                || !resource.capabilities.is_empty()
                || !resource.limitations.is_empty()
                || !categories.is_empty()
                || !tags.is_empty()
                || resource.pricing.is_some()
                || resource.requires_login.is_some()
                || !resource.languages.is_empty();
            if !has_ai_details {
                ui.weak("这条资源还没有成功生成 AI 描述。网页或文章内容已经保存，可以重新补全。");
                if resource.privacy == crate::resource_library_lifecycle::ResourcePrivacy::Private {
                    ui.weak("私密资源不会发送给 AI；取消“私密资源”并保存后才能补全。");
                } else if ui.button("立即补全描述").clicked() {
                    enrich = true;
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
            if !categories.is_empty() {
                let values = categories
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
            if !tags.is_empty() {
                ui.label(format!(
                    "标签：{}",
                    tags.iter()
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
        ui.label(egui::RichText::new("可手动修改").strong());
        ui.label("标题");
        ui.add(egui::TextEdit::singleline(&mut dialog.title).desired_width(f32::INFINITY));
        ui.label("用途");
        ui.add(
            egui::TextEdit::multiline(&mut dialog.purpose_zh)
                .desired_rows(6)
                .desired_width(f32::INFINITY),
        );
        ui.label("私人备注");
        ui.add(
            egui::TextEdit::multiline(&mut dialog.note)
                .desired_rows(6)
                .desired_width(f32::INFINITY),
        );
        ui.checkbox(&mut dialog.private, "私密资源");
        ui.horizontal(|ui| {
            ui.label("评分");
            ui.add(
                egui::Slider::new(&mut dialog.rating, 0..=5).custom_formatter(|v, _| {
                    if v == 0.0 {
                        "未设置".into()
                    } else {
                        format!("{v:.0}/5")
                    }
                }),
            );
        });
        ui.add_space(8.0);
        if ui.button("保存修改").clicked() {
            save = true;
        }
        let update = save.then(|| {
            (
                dialog.id,
                non_empty_owned(&dialog.title),
                non_empty_owned(&dialog.purpose_zh),
                non_empty_owned(&dialog.note),
                dialog.private,
                (dialog.rating > 0).then_some(dialog.rating),
            )
        });
        let discard_decision = discard_guard_controls(ui, show_discard);
        self.apply_discard_decision(discard_decision);
        if discard_decision.is_some() {
            return;
        }
        if close {
            self.set_resource_panel(None);
            return;
        }
        if let Some((id, title, purpose, note, private, rating)) = update {
            use crate::resource_library_lifecycle::{
                CompleteManualEdit, FailureKind, LifecycleFailure, ResourceLibraryLifecycle,
                ResourceLifecycleChange, ResourcePrivacy, SystemClock,
            };
            let lifecycle =
                ResourceLibraryLifecycle::new(&self.db, &self.knowledge_engine, &SystemClock);
            let result = lifecycle
                .project(crate::resource_library_lifecycle::ProjectionScope::Resource(id))
                .and_then(|projection| {
                    let detail = projection.detail.ok_or_else(|| LifecycleFailure {
                        kind: FailureKind::Storage,
                        user_message: "资源详情读取失败".into(),
                        technical_detail: format!("RESOURCE_DETAIL_MISSING: {id}"),
                    })?;
                    lifecycle
                        .apply(
                            ResourceLifecycleChange::CompleteManualEdit(CompleteManualEdit {
                                resource_id: id,
                                title,
                                purpose_zh: purpose,
                                use_when_zh: detail.resource.use_when_zh,
                                private_note: note,
                                privacy: if private {
                                    ResourcePrivacy::Private
                                } else {
                                    ResourcePrivacy::Public
                                },
                                manual_rating: rating,
                                categories: detail.categories,
                                tags: detail.tags,
                            }),
                            crate::resource_library_lifecycle::ProjectionScope::Resource(id),
                        )
                        .map(|_| ())
                });
            match result {
                Ok(_) => {
                    self.ui_state.finish_panel();
                    self.notice("资源已更新");
                }
                Err(e) => self.notice(format!("保存失败：{e}")),
            }
        }
        if enrich {
            let ctx = ui.ctx().clone();
            self.retry_resource_task(resource_id, &ctx);
        }
    }

    fn show_feed_settings_panel(&mut self, ui: &mut egui::Ui) {
        let show_discard = self.ui_state.discard_owner() == Some(DiscardOwner::Panel);
        let mut save = false;
        let mut close = false;
        let Some(panel) = self.feed_settings_panel_mut() else {
            return;
        };
        ui.horizontal(|ui| {
            ui.heading("订阅设置");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("关闭").clicked() {
                    close = true;
                }
            });
        });
        ui.separator();
        ui.label(egui::RichText::new(&panel.title).strong());
        ui.hyperlink_to(&panel.url, &panel.url);
        ui.add_space(10.0);
        ui.checkbox(&mut panel.disabled, "暂停这个订阅");
        ui.weak("暂停后不再自动抓取；重新启用时会立即抓取一次。");
        ui.add_space(10.0);
        ui.label("刷新间隔");
        ui.add(
            egui::TextEdit::singleline(&mut panel.interval_draft)
                .hint_text("例如 30m、6h；留空使用全局默认")
                .desired_width(f32::INFINITY),
        );
        ui.weak("支持 s / m / h / d；修改间隔不会立刻抓取。");
        if let Some(error) = &panel.error {
            ui.add_space(8.0);
            ui.colored_label(egui::Color32::RED, error);
        }
        ui.add_space(12.0);
        if ui.button("保存设置").clicked() {
            save = true;
        }

        let discard_decision = discard_guard_controls(ui, show_discard);
        self.apply_discard_decision(discard_decision);
        if discard_decision.is_some() {
            return;
        }
        if close {
            self.set_feed_settings_panel(None);
            return;
        }
        if !save {
            return;
        }

        let Some(panel) = self.feed_settings_panel() else {
            return;
        };
        let feed_id = panel.feed_id;
        let disabled = panel.disabled;
        let disabled_changed = disabled != panel.original_disabled;
        let interval_draft = panel.interval_draft.trim().to_owned();
        let interval_changed = interval_draft != panel.original_interval;
        let interval = if interval_changed {
            if interval_draft.is_empty() {
                if let Some(panel) = self.feed_settings_panel_mut() {
                    panel.error = Some("当前版本暂不支持清除单源间隔，请输入新的间隔".into());
                }
                return;
            }
            match crate::config::parse_duration(&interval_draft) {
                Ok(seconds) if seconds > 0 => Some(seconds),
                _ => {
                    if let Some(panel) = self.feed_settings_panel_mut() {
                        panel.error = Some("请输入大于 0 的间隔，例如 30m 或 6h".into());
                    }
                    return;
                }
            }
        } else {
            None
        };

        let result = (|| {
            let subscriptions = FeedSubscriptions::session(self.db_path.clone(), &self.rss_refresh);
            if let Some(seconds) = interval {
                subscriptions.apply(SubscriptionChange::SetInterval {
                    id: feed_id,
                    seconds,
                })?;
            }
            let refresh = if disabled_changed {
                subscriptions
                    .apply(if disabled {
                        SubscriptionChange::Disable { id: feed_id }
                    } else {
                        SubscriptionChange::Enable { id: feed_id }
                    })?
                    .refresh
            } else {
                None
            };
            Ok::<_, crate::feed_subscription::SubscriptionError>(refresh)
        })();
        match result {
            Ok(refresh) => {
                self.ui_state.finish_panel();
                self.reload();
                match refresh {
                    Some(InitialRefreshOutcome::Queued) => self.notice("订阅设置已保存，正在刷新"),
                    Some(InitialRefreshOutcome::Deferred) => {
                        self.notice("订阅设置已保存，将在资料维护结束后刷新")
                    }
                    _ => self.notice("订阅设置已保存"),
                }
            }
            Err(error) => {
                tracing::warn!(detail = %error.technical_detail, "save subscription settings failed");
                if let Some(panel) = self.feed_settings_panel_mut() {
                    panel.error = Some(error.user_message);
                }
            }
        }
    }

    fn show_resource_delete_confirmation(&mut self, ctx: &egui::Context) {
        if self.ui_state.modal_kind() != Some(ModalKind::DeleteResource) {
            return;
        }
        let Some(ModalState::DeleteResource { id, title }) = self.ui_state.modal() else {
            return;
        };
        let id = *id;
        let title = title.clone();
        let mut confirm = false;
        let mut cancel = false;
        let response = gui_modal::show(ctx, ModalKind::DeleteResource, false, |ui, _| {
            ui.label(format!("确定永久删除「{title}」吗？"));
            ui.weak("资源快照、分类、标签和整理记录会一起删除；关联的博客文章不会删除。");
            ui.horizontal(|ui| {
                if ui.button("确认永久删除").clicked() {
                    confirm = true;
                }
                if ui.button("取消").clicked() {
                    cancel = true;
                }
            });
        });
        self.apply_modal_host_action(response.action);
        if confirm {
            match crate::resource_library_lifecycle::ResourceLibraryLifecycle::new(
                &self.db,
                &self.knowledge_engine,
                &crate::resource_library_lifecycle::SystemClock,
            )
            .apply(
                crate::resource_library_lifecycle::ResourceLifecycleChange::Delete {
                    resource_id: id,
                },
                crate::resource_library_lifecycle::ProjectionScope::collection(
                    crate::resource_library_lifecycle::ResourceCollection::Archived,
                ),
            ) {
                Ok(_) => self.notice("资源已永久删除"),
                Err(error) => self.notice(format!("删除失败：{error}")),
            }
            self.complete_modal();
        } else if cancel {
            self.complete_modal();
        }
    }

    fn show_resource_import_dialog(&mut self, ctx: &egui::Context) {
        if self.ui_state.modal_kind() != Some(ModalKind::ImportResources) {
            return;
        }
        let show_discard = self.ui_state.discard_owner() == Some(DiscardOwner::Modal);
        let Some(dialog) = self.resource_import_dialog_mut() else {
            return;
        };
        let mut import = false;
        let response = gui_modal::show(ctx, ModalKind::ImportResources, show_discard, |ui, _| {
            ui.label(
                "只创建 Resource 与原 Article 的关联，不复制正文，也不改变原文章、标签或收藏状态。",
            );
            ui.separator();
            egui::ScrollArea::vertical().show(ui, |ui| {
                for candidate in &dialog.candidates {
                    let mut checked = dialog.selected.contains(&candidate.article_id);
                    ui.horizontal(|ui| {
                        let response = ui.add_enabled(
                            !candidate.already_imported,
                            egui::Checkbox::new(&mut checked, ""),
                        );
                        if response.changed() {
                            if checked {
                                dialog.selected.insert(candidate.article_id);
                            } else {
                                dialog.selected.remove(&candidate.article_id);
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
            if ui
                .add_enabled(
                    !dialog.selected.is_empty(),
                    egui::Button::new(format!("导入选中的 {} 项", dialog.selected.len())),
                )
                .clicked()
            {
                import = true;
            }
        });
        let ids = import.then(|| dialog.selected.iter().copied().collect::<Vec<_>>());
        self.apply_modal_host_action(response.action);
        if let Some(ids) = ids {
            match crate::resource_library_lifecycle::ResourceLibraryLifecycle::new(
                &self.db,
                &self.knowledge_engine,
                &crate::resource_library_lifecycle::SystemClock,
            )
            .apply(
                crate::resource_library_lifecycle::ResourceLifecycleChange::ImportWebClippings {
                    article_ids: ids,
                },
                crate::resource_library_lifecycle::ProjectionScope::collection(
                    crate::resource_library_lifecycle::ResourceCollection::Active,
                ),
            ) {
                Ok(outcome) => {
                    self.notice(format!(
                        "已导入 {} 个资源，正在后台补全描述",
                        outcome.affected_resource_ids.len()
                    ));
                    self.complete_modal();
                }
                Err(error) => self.notice(format!("导入失败：{error}")),
            }
        }
    }

    fn show_saved_library_page(&mut self, root_ui: &mut egui::Ui) {
        if self.ui_state.route() != Route::Excerpts {
            return;
        }

        let theme = ReaderTheme::sspai();
        let (rows, load_error) = match self.db.saved_selections() {
            Ok(rows) => (rows, None),
            Err(error) => (Vec::new(), Some(error.to_string())),
        };
        let mut open_article = None;
        let mut delete_selection = None;

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
                    egui::RichText::new("摘录用于保留原文片段；想法是附在摘录上的个人笔记。")
                        .size(13.0)
                        .color(theme.muted),
                );
                ui.add_space(8.0);
                ui.separator();
                ui.add_space(6.0);

                if let Some(error) = &load_error {
                    ui.colored_label(ui.visuals().error_fg_color, format!("读取失败：{error}"));
                    return;
                }
                if rows.is_empty() {
                    ui.vertical_centered(|ui| {
                        ui.add_space(80.0);
                        ui.label(
                            egui::RichText::new("还没有摘录或想法")
                                .size(18.0)
                                .color(theme.text)
                                .family(egui::FontFamily::Name("cjk-bold".into())),
                        );
                        ui.add_space(8.0);
                        ui.label(
                            egui::RichText::new("在正文中选中文字，然后点击“摘录”或“写想法”。")
                                .size(13.0)
                                .color(theme.muted),
                        );
                    });
                    return;
                }

                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        for (selection, feed_id, article_title) in &rows {
                            egui::Frame::new()
                                .fill(theme.code_bg)
                                .stroke(egui::Stroke::new(1.0, theme.border))
                                .corner_radius(egui::CornerRadius::same(7))
                                .inner_margin(egui::Margin::symmetric(14, 12))
                                .show(ui, |ui| {
                                    ui.set_width(ui.available_width());
                                    ui.horizontal(|ui| {
                                        if selection.is_favorite {
                                            ui.label(
                                                egui::RichText::new("★ 摘录")
                                                    .size(12.0)
                                                    .color(theme.accent),
                                            );
                                        }
                                        if selection.comment.is_some() {
                                            ui.label(
                                                egui::RichText::new("✎ 想法")
                                                    .size(12.0)
                                                    .color(theme.link),
                                            );
                                        }
                                        ui.with_layout(
                                            egui::Layout::right_to_left(egui::Align::Center),
                                            |ui| {
                                                ui.label(
                                                    egui::RichText::new(text::fmt_ts(
                                                        selection.updated_at,
                                                    ))
                                                    .size(11.0)
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
                                                selection.selected_text
                                            ))
                                            .size(15.0)
                                            .color(theme.text),
                                        )
                                        .wrap(),
                                    );
                                    if let Some(comment) = &selection.comment {
                                        ui.add_space(9.0);
                                        egui::Frame::new()
                                            .fill(theme.selected_bg)
                                            .corner_radius(egui::CornerRadius::same(5))
                                            .inner_margin(egui::Margin::symmetric(10, 8))
                                            .show(ui, |ui| {
                                                ui.add(
                                                    egui::Label::new(
                                                        egui::RichText::new(comment)
                                                            .size(14.0)
                                                            .color(theme.text),
                                                    )
                                                    .wrap(),
                                                );
                                            });
                                    }
                                    ui.add_space(9.0);
                                    ui.horizontal(|ui| {
                                        let title = article_title
                                            .as_deref()
                                            .filter(|title| !title.trim().is_empty())
                                            .unwrap_or("未命名文章");
                                        ui.add(
                                            egui::Label::new(
                                                egui::RichText::new(title)
                                                    .size(12.0)
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
                                                            egui::RichText::new("删除")
                                                                .size(12.0)
                                                                .color(theme.muted),
                                                        )
                                                        .stroke(egui::Stroke::NONE),
                                                    )
                                                    .clicked()
                                                {
                                                    delete_selection = Some(selection.id);
                                                }
                                                if ui
                                                    .add(
                                                        egui::Button::new(
                                                            egui::RichText::new("打开文章 ↗")
                                                                .size(12.0)
                                                                .color(theme.link),
                                                        )
                                                        .stroke(egui::Stroke::NONE),
                                                    )
                                                    .clicked()
                                                {
                                                    open_article = Some((
                                                        *feed_id,
                                                        selection.article_id,
                                                        selection.clone(),
                                                    ));
                                                }
                                            },
                                        );
                                    });
                                });
                            ui.add_space(10.0);
                        }
                    });
            });

        if let Some(selection_id) = delete_selection {
            match self.db.delete_selection(selection_id) {
                Ok(_) => {
                    self.refresh_saved_selection_count();
                    self.notice("摘录已删除");
                }
                Err(error) => {
                    self.notice(format!("删除失败：{error}"));
                }
            }
        }
        if let Some((feed_id, article_id, selection)) = open_article {
            if self.web_clipping_ids.contains(&article_id)
                || self.db.is_web_clipping(article_id).unwrap_or(false)
            {
                self.select_saved_articles();
            } else {
                self.select_feed(feed_id);
            }
            self.select_article(article_id);
            self.pending_selection_anchor = Some(selection);
            self.notice("已打开原文章，正在定位摘录");
        }
    }

    fn show_archive_library_page(&mut self, root_ui: &mut egui::Ui) {
        if self.ui_state.route() != Route::Archive {
            return;
        }

        let theme = ReaderTheme::sspai();
        let articles = self.articles.clone();
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
                    .size(13.0)
                    .color(theme.muted),
                );
                ui.add_space(8.0);
                ui.separator();
                ui.add_space(6.0);

                if articles.is_empty() {
                    ui.vertical_centered(|ui| {
                        ui.add_space(80.0);
                        ui.label(
                            egui::RichText::new("还没有归档文章")
                                .size(18.0)
                                .color(theme.text)
                                .family(egui::FontFamily::Name("cjk-bold".into())),
                        );
                        ui.add_space(8.0);
                        ui.label(
                            egui::RichText::new("在文章列表中右键一篇文章即可归档。")
                                .size(13.0)
                                .color(theme.muted),
                        );
                    });
                    return;
                }

                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        for article in &articles {
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
                                            .size(15.0)
                                            .color(theme.text)
                                            .family(egui::FontFamily::Name("cjk-bold".into())),
                                        )
                                        .wrap(),
                                    );
                                    ui.add_space(6.0);
                                    ui.horizontal(|ui| {
                                        let date = article
                                            .published
                                            .map(text::fmt_ts)
                                            .unwrap_or_else(|| text::fmt_ts(article.fetched_at));
                                        ui.label(
                                            egui::RichText::new(date).size(11.0).color(theme.muted),
                                        );
                                        ui.with_layout(
                                            egui::Layout::right_to_left(egui::Align::Center),
                                            |ui| {
                                                if ui
                                                    .add(
                                                        egui::Button::new(
                                                            egui::RichText::new("恢复并打开")
                                                                .size(12.0)
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
        if self.ui_state.modal_kind() != Some(ModalKind::Search) {
            return;
        }
        let Some(dialog) = self.search_dialog_mut() else {
            return;
        };
        let theme = ReaderTheme::sspai();
        let mut submit = false;
        let mut selected_hit = None;
        let mut clear_history = false;

        let response = gui_modal::show(ctx, ModalKind::Search, false, |ui, focus| {
            ui.horizontal(|ui| {
                let input = ui.add_sized(
                    egui::vec2((ui.available_width() - 76.0).max(180.0), 34.0),
                    egui::TextEdit::singleline(&mut dialog.query)
                        .hint_text("搜索文章、网页快照、摘录和想法…")
                        .font(egui::TextStyle::Body),
                );
                if dialog.focus_input && focus == InitialFocus::PrimaryField {
                    input.request_focus();
                    dialog.focus_input = false;
                }
                if input.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    submit = true;
                }
                if ui
                    .add_sized(
                        egui::vec2(68.0, 34.0),
                        egui::Button::new(egui::RichText::new("搜索").size(13.0).color(theme.text))
                            .fill(theme.selected_bg)
                            .stroke(egui::Stroke::new(1.0, theme.border)),
                    )
                    .clicked()
                {
                    submit = true;
                }
            });
            ui.add_space(7.0);
            ui.label(
                egui::RichText::new(
                    "支持标题、作者、正文、网址、摘录原文和想法内容；最多显示 200 条。",
                )
                .size(12.0)
                .color(theme.muted),
            );
            ui.add_space(8.0);
            ui.separator();
            ui.add_space(6.0);

            if let Some(error) = &dialog.error {
                ui.colored_label(ui.visuals().error_fg_color, error);
                return;
            }
            if dialog.searched_query.is_empty() {
                if !dialog.history.is_empty() {
                    ui.horizontal(|ui| {
                        ui.label(
                            egui::RichText::new("最近搜索")
                                .size(13.0)
                                .color(theme.text)
                                .family(egui::FontFamily::Name("cjk-bold".into())),
                        );
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui
                                .add(
                                    egui::Button::new(
                                        egui::RichText::new("清空").size(12.0).color(theme.muted),
                                    )
                                    .stroke(egui::Stroke::NONE),
                                )
                                .clicked()
                            {
                                clear_history = true;
                            }
                        });
                    });
                    ui.add_space(5.0);
                    let history = dialog.history.clone();
                    ui.horizontal_wrapped(|ui| {
                        for entry in history {
                            if ui
                                .add(
                                    egui::Button::new(format!(
                                        "{}  · {}",
                                        entry.query, entry.result_count
                                    ))
                                    .fill(theme.code_bg)
                                    .stroke(egui::Stroke::new(1.0, theme.border)),
                                )
                                .clicked()
                            {
                                dialog.query = entry.query;
                                submit = true;
                            }
                        }
                    });
                    ui.add_space(18.0);
                }
                ui.vertical_centered(|ui| {
                    ui.add_space(55.0);
                    ui.label(
                        egui::RichText::new("在一个入口里找回所有阅读资料")
                            .size(18.0)
                            .color(theme.text)
                            .family(egui::FontFamily::Name("cjk-bold".into())),
                    );
                    ui.add_space(8.0);
                    ui.label(
                        egui::RichText::new("快捷键 Ctrl + F")
                            .size(13.0)
                            .color(theme.muted),
                    );
                });
                return;
            }
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new(format!("找到 {} 条结果", dialog.results.len()))
                        .size(13.0)
                        .color(theme.text)
                        .family(egui::FontFamily::Name("cjk-bold".into())),
                );
                ui.label(
                    egui::RichText::new(format!("“{}”", dialog.searched_query))
                        .size(12.0)
                        .color(theme.muted),
                );
            });
            ui.add_space(6.0);
            if dialog.results.is_empty() {
                ui.vertical_centered(|ui| {
                    ui.add_space(75.0);
                    ui.label(egui::RichText::new("没有匹配内容").size(16.0));
                    ui.add_space(6.0);
                    ui.label(
                        egui::RichText::new("换一个更短或更常见的关键词试试。")
                            .size(12.0)
                            .color(theme.muted),
                    );
                });
                return;
            }

            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    for hit in &dialog.results {
                        let (kind, kind_color) = match hit.kind {
                            SearchHitKind::Article => ("文章", theme.link),
                            SearchHitKind::WebClipping => ("网页快照", theme.accent),
                            SearchHitKind::Excerpt => ("摘录", theme.link),
                            SearchHitKind::Thought => ("想法", theme.accent),
                        };
                        let response = egui::Frame::new()
                            .fill(theme.code_bg)
                            .stroke(egui::Stroke::new(1.0, theme.border))
                            .corner_radius(egui::CornerRadius::same(7))
                            .inner_margin(egui::Margin::symmetric(14, 11))
                            .show(ui, |ui| {
                                ui.set_width(ui.available_width());
                                ui.horizontal(|ui| {
                                    ui.label(
                                        egui::RichText::new(kind)
                                            .size(11.0)
                                            .color(kind_color)
                                            .background_color(theme.selected_bg),
                                    );
                                    if hit.archived {
                                        ui.label(
                                            egui::RichText::new("已归档")
                                                .size(11.0)
                                                .color(theme.muted),
                                        );
                                    }
                                    ui.label(
                                        egui::RichText::new(text::fmt_ts(hit.timestamp))
                                            .size(11.0)
                                            .color(theme.muted),
                                    );
                                });
                                ui.add_space(5.0);
                                let title = hit
                                    .article_title
                                    .as_deref()
                                    .filter(|title| !title.trim().is_empty())
                                    .unwrap_or("未命名文章");
                                ui.add(
                                    egui::Label::new(search_highlight_layout_job(
                                        title,
                                        &dialog.searched_query,
                                        15.0,
                                        theme.text,
                                        egui::FontFamily::Name("cjk-bold".into()),
                                        theme,
                                    ))
                                    .wrap(),
                                );
                                ui.add_space(5.0);
                                let preview =
                                    search_preview(&hit.snippet, &dialog.searched_query, 180);
                                ui.add(
                                    egui::Label::new(search_highlight_layout_job(
                                        &preview,
                                        &dialog.searched_query,
                                        13.0,
                                        theme.muted,
                                        egui::FontFamily::Proportional,
                                        theme,
                                    ))
                                    .wrap(),
                                );
                            })
                            .response
                            .interact(egui::Sense::click());
                        if response.hovered() {
                            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                            ui.painter().rect_stroke(
                                response.rect,
                                egui::CornerRadius::same(7),
                                egui::Stroke::new(1.0, theme.accent),
                                egui::StrokeKind::Inside,
                            );
                        }
                        if response.clicked() {
                            selected_hit = Some(hit.clone());
                        }
                        ui.add_space(9.0);
                    }
                });
        });

        self.apply_modal_host_action(response.action);

        if clear_history {
            if let Err(error) = self.db.clear_search_history() {
                self.notice(format!("清空搜索历史失败：{error}"));
            }
            if let Some(dialog) = self.search_dialog_mut() {
                dialog.history.clear();
            }
        }

        if submit {
            self.run_search();
        } else if let Some(hit) = selected_hit {
            self.open_search_result(&hit);
        }
    }

    fn show_active_modal(&mut self, ctx: &egui::Context) {
        match self.ui_state.modal_kind() {
            Some(ModalKind::AddFeed | ModalKind::DeleteFeed) => self.show_feed_dialogs(ctx),
            Some(ModalKind::Search) => self.show_search_window(ctx),
            Some(ModalKind::EditTags) => self.show_tag_dialog(ctx),
            Some(ModalKind::WriteThought) => self.show_comment_dialog(ctx),
            Some(ModalKind::SaveWebPage) => self.show_web_clip_dialog(ctx),
            Some(ModalKind::DeleteWebPage) => self.show_delete_web_clip_dialog(ctx),
            Some(ModalKind::AddResource) => self.show_resource_add_dialog(ctx),
            Some(ModalKind::DeleteResource) => self.show_resource_delete_confirmation(ctx),
            Some(ModalKind::ImportResources) => self.show_resource_import_dialog(ctx),
            Some(ModalKind::RestoreBackup | ModalKind::ClearImages) => self.show_storage_modal(ctx),
            None => {}
        }
    }

    fn show_selection_notice(&mut self, ctx: &egui::Context) {
        let Some(notice) = self.ui_state.notice() else {
            return;
        };
        if notice.created.elapsed() > Duration::from_secs(4) {
            self.ui_state.reduce(UiAction::ClearNotice);
            return;
        }
        let message = notice.message.clone();
        let theme = ReaderTheme::sspai();
        let failed = message.contains("失败") || message.contains("错误");
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
                                    egui::RichText::new(message).size(13.0).color(theme.text),
                                )
                                .wrap(),
                            );
                        });
                    });
            });
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

    fn update_article_selection(
        &mut self,
        ctx: &egui::Context,
        article_id: i64,
        frame: &ArticleSelectionFrame,
    ) -> ArticleSelectionResult {
        let (pointer_pos, primary_pressed, primary_down, primary_released) = ctx.input(|input| {
            (
                input.pointer.interact_pos(),
                input.pointer.primary_pressed(),
                input.pointer.primary_down(),
                input.pointer.primary_released(),
            )
        });
        let mut drag_started = false;
        let drag_was_active = self.article_selection_drag.is_some();

        if self
            .article_selection_drag
            .as_ref()
            .is_some_and(|drag| drag.article_id != article_id)
        {
            self.article_selection_drag = None;
        }

        // `primary_pressed` is normally enough, but a label-selection drag
        // can capture the pointer before this panel is visited.  Arm the
        // article-level state on the first frame with the button down as
        // well; otherwise egui paints a valid blue selection while we never
        // receive an anchor from which to open the toolbar on release.
        if primary_pressed || (primary_down && self.article_selection_drag.is_none()) {
            if let Some(cursor) =
                pointer_pos.and_then(|position| article_cursor_for_pointer(frame, position))
            {
                self.article_selection_drag = Some(ArticleSelectionDrag {
                    article_id,
                    anchor: cursor,
                    focus: cursor,
                });
                drag_started = true;
            } else if primary_pressed {
                self.article_selection_drag = None;
            }
        }

        if (primary_down || primary_released)
            && let Some(position) = pointer_pos
            && let Some(cursor) = article_cursor_nearest(frame, position)
            && let Some(drag) = self.article_selection_drag.as_mut()
            && drag.article_id == article_id
        {
            drag.focus = cursor;
        }

        // When the pointer leaves the native window, some backends report the
        // button transition as `primary_down = false` without a separate
        // `primary_released` event.  Treat that transition as a release only
        // when an article drag was already armed; a normal click still yields
        // an empty quote and therefore no popup.
        let pointer_finished =
            primary_released || (drag_was_active && !primary_down && !primary_pressed);
        let popup_request = if pointer_finished {
            self.article_selection_drag.take().and_then(|drag| {
                let quote = selected_quote_from_article_text(
                    article_id,
                    &frame.plain_text,
                    drag.anchor.char_index,
                    drag.focus.char_index,
                )?;
                let (anchor_rect, source_layer) = article_cursor_anchor(frame, drag.focus)?;
                Some(SelectionPopupRequest {
                    quote,
                    anchor_rect,
                    source_layer,
                })
            })
        } else {
            None
        };

        ArticleSelectionResult {
            popup_request,
            drag_started,
        }
    }

    fn receive_images(&mut self, ctx: &egui::Context) {
        while let Ok(event) = self.image_event_rx.try_recv() {
            match event {
                ImageEvent::Progress { uri, attempt } => {
                    if let Some(ImageState::Loading {
                        attempt: current, ..
                    }) = self.image_cache.get_mut(&uri)
                    {
                        *current = attempt;
                    }
                }
                ImageEvent::Complete { uri, result } => {
                    let state = match result {
                        Ok(bytes) => match image::load_from_memory(bytes.as_ref()) {
                            Ok(decoded) => ImageState::Ready {
                                dimensions: Some((decoded.width(), decoded.height())),
                                bytes,
                            },
                            Err(error) => ImageState::Failed(ImageFailure {
                                message: "图片格式无法显示".to_owned(),
                                detail: error.to_string(),
                                attempts: 1,
                                retryable: false,
                            }),
                        },
                        Err(error) => ImageState::Failed(error),
                    };
                    self.image_cache.insert(uri, state);
                }
            }
            ctx.request_repaint();
        }
    }

    fn receive_formulas(&mut self, ctx: &egui::Context) {
        while let Ok(event) = self.formula_event_rx.try_recv() {
            match event {
                FormulaEvent::Complete { key, result } => {
                    self.formula_cache.insert(
                        key,
                        match result {
                            Ok(bytes) => FormulaState::Ready(bytes),
                            Err(error) => FormulaState::Failed(error),
                        },
                    );
                }
            }
            ctx.request_repaint();
        }
    }

    fn handle_tray_events(&mut self, ctx: &egui::Context) {
        while let Ok(ev) = MenuEvent::receiver().try_recv() {
            if ev.id == self.tray_toggle {
                self.hidden = !self.hidden;
                ctx.send_viewport_cmd(ViewportCommand::Visible(!self.hidden));
                if !self.hidden {
                    ctx.send_viewport_cmd(ViewportCommand::Focus);
                }
            } else if ev.id == self.tray_fetch {
                if let Err(error) = self.rss_refresh.request_all() {
                    self.notice(format!("无法启动订阅刷新：{error}"));
                }
            } else if ev.id == self.tray_quit {
                self.quitting = true;
                ctx.send_viewport_cmd(ViewportCommand::Close);
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
    }

    // eframe 0.35：App 入口是 ui(&mut Ui)，panel 在根 Ui 内 show。
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
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
        self.shared.focused.store(
            ctx.input(|i| i.viewport().focused.unwrap_or(true)),
            Ordering::Relaxed,
        );
        self.receive_images(&ctx);
        self.receive_formulas(&ctx);
        self.receive_web_clip_events(&ctx);
        self.receive_knowledge_updates(&ctx);
        self.handle_tray_events(&ctx);

        // 关窗 → 隐藏到托盘（除非托盘"退出"已置 quitting，ADR-15）。
        if !self.quitting && ctx.input(|i| i.viewport().close_requested()) {
            ctx.send_viewport_cmd(ViewportCommand::CancelClose);
            ctx.send_viewport_cmd(ViewportCommand::Visible(false));
            self.hidden = true;
        }
        // 心跳：即便隐藏/空闲也定期醒来轮询托盘事件。
        // ponytail: 250ms 轮询够跟手；想省这点空转再上 MenuEvent::set_event_handler + proxy。
        ctx.request_repaint_after(Duration::from_millis(250));

        let rss_snapshot = self.rss_refresh.snapshot();
        let busy = rss_snapshot.current.is_some();
        let theme = ReaderTheme::sspai();

        // 源栏
        let mut feed_click = None;
        let mut feed_settings_click = None;
        egui::Panel::left("feeds")
            .exact_size(FEED_PANEL_WIDTH)
            .resizable(false)
            .frame(
                egui::Frame::new()
                    .fill(theme.panel)
                    .stroke(egui::Stroke::new(1.0, theme.border))
                    .inner_margin(egui::Margin::symmetric(12, 10)),
            )
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new("订阅")
                            .size(18.0)
                            .family(egui::FontFamily::Name("cjk-bold".into())),
                    );
                    let total_unread: i64 = self.feeds.iter().map(|(_, unread)| unread).sum();
                    if total_unread > 0 {
                        ui.label(
                            egui::RichText::new(total_unread.to_string())
                                .small()
                                .color(egui::Color32::WHITE)
                                .background_color(theme.accent),
                        );
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("＋").on_hover_text("添加订阅").clicked() {
                            self.open_modal(ModalState::AddFeed(FeedAddDialog::default()));
                        }
                        let selected_feed = self
                            .ui_state
                            .route()
                            .article_collection()
                            .and_then(|collection| match collection {
                                ArticleCollection::Feed(Some(id)) => Some(id),
                                _ => None,
                            })
                            .and_then(|id| self.feeds.iter().find(|(feed, _)| feed.id == id))
                            .map(|(feed, _)| feed.clone());
                        if ui
                            .add_enabled(selected_feed.is_some(), egui::Button::new("－"))
                            .on_hover_text("删除当前订阅")
                            .clicked()
                            && let Some(feed) = selected_feed.as_ref()
                        {
                            self.open_modal(ModalState::DeleteFeed {
                                id: feed.id,
                                title: feed.title.clone().unwrap_or_else(|| feed.url.clone()),
                            });
                        }
                        if ui
                            .add_enabled(selected_feed.is_some(), egui::Button::new("⚙"))
                            .on_hover_text("当前订阅设置")
                            .clicked()
                            && let Some(feed) = selected_feed
                        {
                            feed_settings_click = Some(feed);
                        }
                        let label = if busy { "抓取中…" } else { "⟳ 刷新" };
                        if ui
                            .add_enabled(
                                !busy,
                                egui::Button::new(
                                    egui::RichText::new(label).size(12.0).color(theme.muted),
                                )
                                .stroke(egui::Stroke::NONE),
                            )
                            .clicked()
                            && let Err(error) = self.rss_refresh.request_all()
                        {
                            self.notice(format!("无法启动订阅刷新：{error}"));
                        }
                    });
                });
                ui.separator();
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
                let search_response = ui.add_enabled(
                    !modal_open,
                    egui::Button::new(egui::RichText::new("⌕ 全文搜索   Ctrl+F").size(13.0).color(
                        if self.ui_state.modal_kind() == Some(ModalKind::Search) {
                            theme.text
                        } else {
                            theme.muted
                        },
                    ))
                    .fill(if self.ui_state.modal_kind() == Some(ModalKind::Search) {
                        theme.selected_bg
                    } else {
                        egui::Color32::TRANSPARENT
                    })
                    .stroke(egui::Stroke::NONE)
                    .corner_radius(egui::CornerRadius::same(4))
                    .min_size(egui::vec2(ui.available_width(), 34.0)),
                );
                if search_response.clicked() {
                    self.open_search();
                }
                ui.add_space(4.0);
                let saved_articles_visible = matches!(
                    self.ui_state.route(),
                    Route::Articles(ArticleCollection::Saved)
                );
                let saved_articles_response = ui.add(
                    egui::Button::new(
                        egui::RichText::new(format!("★ 文章收藏  {}", self.saved_article_count))
                            .size(13.0)
                            .color(if saved_articles_visible {
                                theme.text
                            } else {
                                theme.accent
                            }),
                    )
                    .fill(if saved_articles_visible {
                        theme.selected_bg
                    } else {
                        egui::Color32::TRANSPARENT
                    })
                    .stroke(egui::Stroke::NONE)
                    .corner_radius(egui::CornerRadius::same(4))
                    .min_size(egui::vec2(ui.available_width(), 34.0)),
                );
                if saved_articles_response.clicked() {
                    self.select_saved_articles();
                    self.clear_selection_popover();
                }
                ui.add_space(4.0);
                let resources_response = ui.add(
                    egui::Button::new(egui::RichText::new("◆ 资源库").size(13.0).color(
                        if self.ui_state.route() == Route::Resources {
                            theme.text
                        } else {
                            theme.accent
                        },
                    ))
                    .fill(if self.ui_state.route() == Route::Resources {
                        theme.selected_bg
                    } else {
                        egui::Color32::TRANSPARENT
                    })
                    .stroke(egui::Stroke::NONE)
                    .corner_radius(egui::CornerRadius::same(4))
                    .min_size(egui::vec2(ui.available_width(), 34.0)),
                );
                if resources_response.clicked() {
                    self.navigate(Route::Resources);
                    self.clear_selection_popover();
                }
                ui.add_space(4.0);
                let read_later_response = ui.add(
                    egui::Button::new(
                        egui::RichText::new(format!("◷ 稍后读  {}", self.read_later_count))
                            .size(13.0)
                            .color(
                                if matches!(
                                    self.ui_state.route(),
                                    Route::Articles(ArticleCollection::ReadLater)
                                ) {
                                    theme.text
                                } else {
                                    theme.muted
                                },
                            ),
                    )
                    .fill(
                        if matches!(
                            self.ui_state.route(),
                            Route::Articles(ArticleCollection::ReadLater)
                        ) {
                            theme.selected_bg
                        } else {
                            egui::Color32::TRANSPARENT
                        },
                    )
                    .stroke(egui::Stroke::NONE)
                    .corner_radius(egui::CornerRadius::same(4))
                    .min_size(egui::vec2(ui.available_width(), 34.0)),
                );
                if read_later_response.clicked() {
                    self.select_read_later();
                    self.clear_selection_popover();
                }
                ui.add_space(4.0);
                let library_response = ui.add(
                    egui::Button::new(
                        egui::RichText::new(format!(
                            "✦ 摘录与想法  {}",
                            self.saved_selection_count
                        ))
                        .size(13.0)
                        .color(
                            if self.ui_state.route() == Route::Excerpts {
                                theme.text
                            } else {
                                theme.accent
                            },
                        ),
                    )
                    .fill(if self.ui_state.route() == Route::Excerpts {
                        theme.selected_bg
                    } else {
                        egui::Color32::TRANSPARENT
                    })
                    .stroke(egui::Stroke::NONE)
                    .corner_radius(egui::CornerRadius::same(4))
                    .min_size(egui::vec2(ui.available_width(), 34.0)),
                );
                if library_response.clicked() {
                    self.navigate(Route::Excerpts);
                    self.clear_selection_popover();
                }
                ui.add_space(4.0);
                let archive_response = ui.add(
                    egui::Button::new(
                        egui::RichText::new(format!("▣ 已归档  {}", self.archived_article_count))
                            .size(13.0)
                            .color(if self.ui_state.route() == Route::Archive {
                                theme.text
                            } else {
                                theme.muted
                            }),
                    )
                    .fill(if self.ui_state.route() == Route::Archive {
                        theme.selected_bg
                    } else {
                        egui::Color32::TRANSPARENT
                    })
                    .stroke(egui::Stroke::NONE)
                    .corner_radius(egui::CornerRadius::same(4))
                    .min_size(egui::vec2(ui.available_width(), 34.0)),
                );
                if archive_response.clicked() {
                    self.navigate(Route::Archive);
                    self.clear_selection_popover();
                }
                ui.add_space(4.0);
                let storage_response = ui.add(
                    egui::Button::new(egui::RichText::new("⚙ 资料库管理").size(13.0).color(
                        if self.ui_state.route() == Route::Storage {
                            theme.text
                        } else {
                            theme.muted
                        },
                    ))
                    .fill(if self.ui_state.route() == Route::Storage {
                        theme.selected_bg
                    } else {
                        egui::Color32::TRANSPARENT
                    })
                    .stroke(egui::Stroke::NONE)
                    .corner_radius(egui::CornerRadius::same(4))
                    .min_size(egui::vec2(ui.available_width(), 34.0)),
                );
                if storage_response.clicked() {
                    self.navigate(Route::Storage);
                    if self.ui_state.route() == Route::Storage {
                        self.storage_message = None;
                        self.refresh_storage_overview();
                    }
                }
                ui.add_space(4.0);
                ui.separator();
                ui.add_space(4.0);
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        for (fd, unread) in &self.feeds {
                            let title = fd.title.clone().unwrap_or_else(|| fd.url.clone());
                            let mark = if fd.disabled {
                                "✗"
                            } else if fd.fail_count > 0 {
                                "⚠"
                            } else if *unread > 0 {
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
                            let mut response = ui.add(
                                egui::Button::new(
                                    egui::RichText::new(format!("{mark} {title} ({unread})"))
                                        .size(13.0)
                                        .color(if sel { theme.text } else { theme.muted }),
                                )
                                .fill(fill)
                                .stroke(egui::Stroke::NONE)
                                .corner_radius(egui::CornerRadius::same(4))
                                .wrap()
                                .min_size(egui::vec2(ui.available_width(), 34.0)),
                            );
                            if let Some(error) = &fd.last_error {
                                response = response.on_hover_text(format!("最近刷新失败：{error}"));
                            }
                            if sel {
                                ui.painter().rect_filled(
                                    egui::Rect::from_min_max(
                                        response.rect.left_top(),
                                        egui::pos2(
                                            response.rect.left() + 3.0,
                                            response.rect.bottom(),
                                        ),
                                    ),
                                    egui::CornerRadius::same(2),
                                    theme.accent,
                                );
                            }
                            if response.clicked() {
                                feed_click = Some(fd.id);
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
        self.show_active_modal(&ctx);
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
        egui::Panel::left("articles")
            .exact_size(ARTICLE_PANEL_WIDTH)
            .resizable(false)
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
                        .size(18.0)
                        .family(egui::FontFamily::Name("cjk-bold".into())),
                    );
                    ui.label(
                        egui::RichText::new(format!("{} 篇", self.articles.len()))
                            .size(12.0)
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
                if saved_collection && self.articles.is_empty() {
                    ui.add_space(26.0);
                    ui.vertical_centered(|ui| {
                        ui.label(
                            egui::RichText::new("还没有文章收藏")
                                .size(15.0)
                                .color(theme.text)
                                .family(egui::FontFamily::Name("cjk-bold".into())),
                        );
                        ui.add_space(6.0);
                        ui.label(
                            egui::RichText::new(
                                "打开订阅文章，点击正文标题下方「收藏文章」；也可点右上角＋保存网页。",
                            )
                                .size(12.0)
                                .color(theme.muted),
                        );
                    });
                }
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        for a in &self.articles {
                            let is_web_clip = self.web_clipping_ids.contains(&a.id);
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
                            let article_button = egui::Button::new(
                                    egui::RichText::new(format!("{dot}{title}{star}"))
                                        .size(13.0)
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
                                .wrap()
                                .min_size(egui::vec2(ui.available_width(), 42.0));
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
                            if resp.clicked() {
                                if self.batch_mode {
                                    batch_toggles.push((
                                        a.id,
                                        !self.batch_selection.contains(&a.id),
                                    ));
                                } else {
                                    open_article = Some(a.id);
                                }
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
                                    format!("{author}  ·  {}", text::fmt_ts(ts))
                                }
                                (Some(author), None) => author.to_string(),
                                (None, Some(ts)) => text::fmt_ts(ts),
                                (None, None) => String::new(),
                            };
                            if !meta.is_empty() {
                                ui.add(
                                    egui::Label::new(
                                        egui::RichText::new(meta).size(11.0).color(theme.muted),
                                    )
                                    .wrap(),
                                );
                            }
                            ui.separator();
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

        // 正文栏
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
                let Some(a) = self.selected_article() else {
                    ui.centered_and_justified(|ui| ui.label("← 选一篇文章"));
                    return;
                };
                let title = a.title.clone().unwrap_or_default();
                let date = a.published.map(text::fmt_ts).unwrap_or_default();
                let url = a.url.clone();
                let author = a.author.clone();
                let article_starred = a.starred;
                let article_read_later = a.read_later;
                let article_tags = self
                    .article_tags
                    .get(&article_id)
                    .cloned()
                    .unwrap_or_default();
                let article_ai = self.db.article_ai(article_id).unwrap_or_default();
                let article_ai_task =
                    self.knowledge_task(KnowledgeTaskKind::ArticleSummary, article_id);
                let article_ai_is_busy = article_ai_task
                    .as_ref()
                    .is_some_and(|view| {
                        matches!(
                            view.status,
                            KnowledgeTaskStatus::Queued | KnowledgeTaskStatus::Running
                        )
                    });
                let is_web_clipping = self.web_clipping_ids.contains(&article_id);
                let blocks = match a.content.as_deref() {
                    Some(c) if !c.trim().is_empty() => text::content_blocks(c, a.url.as_deref()),
                    _ => Vec::new(),
                };
                let saved_selections = self
                    .db
                    .selections_for_article(article_id)
                    .unwrap_or_default();
                let mut delete_selection = None;
                let mut toggle_article_star = false;
                let mut toggle_article_read_later = false;
                let mut edit_article_tags = false;
                let mut generate_article_ai = false;
                let mut selection_frame = ArticleSelectionFrame::default();
                let mut body_scroll = egui::ScrollArea::vertical()
                    .id_salt(("article-body-v2", article_id))
                    .hscroll(false);
                if reset_body_scroll {
                    body_scroll = body_scroll.scroll_offset(egui::Vec2::ZERO);
                } else if let Some((pending_article_id, offset)) = self.pending_body_scroll.take() {
                    if pending_article_id == article_id {
                        body_scroll = body_scroll.scroll_offset(egui::vec2(0.0, offset.max(0.0)));
                    } else {
                        self.pending_body_scroll = Some((pending_article_id, offset));
                    }
                }
                let body_scroll_output = body_scroll.show_viewport(ui, |ui, viewport| {
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
                                let title_response = selectable_text_block_with_style(
                                    ui,
                                    article_id,
                                    usize::MAX,
                                    &title,
                                    &[],
                                    &[],
                                    ArticleTextStyle::Title,
                                    &mut selection_frame,
                                );
                                if reset_body_scroll {
                                    title_response.scroll_to_me(Some(egui::Align::Min));
                                }
                                ui.horizontal_wrapped(|ui| {
                                    if let Some(author) = &author {
                                        ui.label(
                                            egui::RichText::new(author)
                                                .size(12.0)
                                                .color(theme.muted),
                                        );
                                        ui.label(
                                            egui::RichText::new("·").size(12.0).color(theme.muted),
                                        );
                                    }
                                    ui.label(
                                        egui::RichText::new(date).size(12.0).color(theme.muted),
                                    );
                                    if let Some(u) = &url
                                        && ui
                                            .add(
                                                egui::Button::new(
                                                    egui::RichText::new("在浏览器中打开 ↗")
                                                        .size(12.0)
                                                        .color(theme.link),
                                                )
                                                .stroke(egui::Stroke::NONE),
                                            )
                                            .clicked()
                                        {
                                            open_in_browser(u);
                                        }
                                    if is_web_clipping {
                                        ui.label(
                                            egui::RichText::new("◫ 已保存网页")
                                                .size(12.0)
                                                .color(theme.muted),
                                        );
                                    } else {
                                        let (label, color, fill) = if article_starred {
                                            ("★ 已收藏", theme.accent, theme.selected_bg)
                                        } else {
                                            ("☆ 收藏文章", theme.muted, egui::Color32::TRANSPARENT)
                                        };
                                        if ui
                                            .add(
                                                egui::Button::new(
                                                    egui::RichText::new(label)
                                                        .size(12.0)
                                                        .color(color),
                                                )
                                                .fill(fill)
                                                .stroke(egui::Stroke::new(1.0, theme.border))
                                                .corner_radius(egui::CornerRadius::same(4)),
                                            )
                                            .clicked()
                                        {
                                            toggle_article_star = true;
                                        }
                                    }
                                    let (later_label, later_color, later_fill) =
                                        if article_read_later {
                                            ("◷ 已在稍后读", theme.accent, theme.selected_bg)
                                        } else {
                                            ("◷ 稍后读", theme.muted, egui::Color32::TRANSPARENT)
                                        };
                                    if ui
                                        .add(
                                            egui::Button::new(
                                                egui::RichText::new(later_label)
                                                    .size(12.0)
                                                    .color(later_color),
                                            )
                                            .fill(later_fill)
                                            .stroke(egui::Stroke::new(1.0, theme.border)),
                                        )
                                        .clicked()
                                    {
                                        toggle_article_read_later = true;
                                    }
                                    if ui
                                        .add(
                                            egui::Button::new(
                                                egui::RichText::new("标签")
                                                    .size(12.0)
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
                                                .size(11.0)
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
                                        KnowledgeTaskStatus::Queued | KnowledgeTaskStatus::Running
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
                                            text::fmt_ts(ai.updated_at)
                                        ));
                                    });
                                    ui.add_space(18.0);
                                }
                                if !saved_selections.is_empty() {
                                    ui.collapsing(
                                        format!("已保存摘录（{}）", saved_selections.len()),
                                        |ui| {
                                            for saved in &saved_selections {
                                                egui::Frame::group(ui.style()).show(ui, |ui| {
                                                    ui.label(&saved.selected_text);
                                                    ui.horizontal(|ui| {
                                                        if saved.is_favorite {
                                                            ui.label("★ 已摘录");
                                                        }
                                                        if let Some(comment) = &saved.comment {
                                                            ui.weak(format!("评论：{comment}"));
                                                        }
                                                        if ui.small_button("删除").clicked() {
                                                            delete_selection = Some(saved.id);
                                                        }
                                                    });
                                                });
                                                ui.add_space(4.0);
                                            }
                                        },
                                    );
                                    ui.add_space(10.0);
                                }
                                if blocks.is_empty() {
                                    ui.label("（此源未提供正文，点上方按钮看原文）");
                                }

                                // Keep ordinary paragraphs in coherent labels so their
                                // typography remains stable.  The article-level selection
                                // frame below joins these labels with headings and lists,
                                // allowing a drag to cross every semantic block.
                                let mut index = 0;
                                while index < blocks.len() {
                                    match &blocks[index] {
                                        Block::Quote(quote) => {
                                            let quote_frame = egui::Frame::new()
                                                .inner_margin(egui::Margin::symmetric(38, 24))
                                                .show(ui, |ui| {
                                                    ui.set_width(ui.available_width());
                                                    selectable_text_block_with_style(
                                                        ui,
                                                        article_id,
                                                        index,
                                                        quote,
                                                        &[],
                                                        &[],
                                                        ArticleTextStyle::Quote,
                                                        &mut selection_frame,
                                                    );
                                                });
                                            let decoration = egui::Color32::from_rgb(224, 224, 224);
                                            ui.painter().text(
                                                quote_frame.response.rect.left_top()
                                                    + egui::vec2(8.0, -4.0),
                                                egui::Align2::LEFT_TOP,
                                                "“",
                                                egui::FontId::new(
                                                    52.0,
                                                    egui::FontFamily::Name("cjk-bold".into()),
                                                ),
                                                decoration,
                                            );
                                            ui.painter().text(
                                                quote_frame.response.rect.right_bottom()
                                                    + egui::vec2(-8.0, 3.0),
                                                egui::Align2::RIGHT_BOTTOM,
                                                "”",
                                                egui::FontId::new(
                                                    52.0,
                                                    egui::FontFamily::Name("cjk-bold".into()),
                                                ),
                                                decoration,
                                            );
                                            index += 1;
                                            ui.add_space(15.0);
                                        }
                                        Block::Code(code) => {
                                            egui::Frame::new()
                                                .fill(theme.code_bg)
                                                .corner_radius(egui::CornerRadius::same(4))
                                                .inner_margin(egui::Margin::symmetric(20, 10))
                                                .show(ui, |ui| {
                                                    ui.set_width(ui.available_width());
                                                    selectable_text_block_with_style(
                                                        ui,
                                                        article_id,
                                                        index,
                                                        code,
                                                        &[],
                                                        &[],
                                                        ArticleTextStyle::Code,
                                                        &mut selection_frame,
                                                    );
                                                });
                                            index += 1;
                                            ui.add_space(25.0);
                                        }
                                        Block::CodeBlock { text: code, language } => {
                                            egui::Frame::new()
                                                .fill(theme.code_bg)
                                                .corner_radius(egui::CornerRadius::same(4))
                                                .inner_margin(egui::Margin::symmetric(20, 10))
                                                .show(ui, |ui| {
                                                    ui.set_width(ui.available_width());
                                                    ui.with_layout(
                                                        egui::Layout::right_to_left(egui::Align::Center),
                                                        |ui| {
                                                            ui.label(
                                                                egui::RichText::new(language.to_uppercase())
                                                                    .monospace()
                                                                    .size(11.0)
                                                                    .color(theme.muted),
                                                            );
                                                        },
                                                    );
                                                    selectable_text_block_with_style(
                                                        ui,
                                                        article_id,
                                                        index,
                                                        code,
                                                        &[],
                                                        &[],
                                                        ArticleTextStyle::Code,
                                                        &mut selection_frame,
                                                    );
                                                });
                                            index += 1;
                                            ui.add_space(25.0);
                                        }
                                        Block::Image(uri) => {
                                            article_image(
                                                ui,
                                                &viewport,
                                                uri,
                                                None,
                                                &mut self.image_cache,
                                                &self.image_job_tx,
                                            );
                                            index += 1;
                                        }
                                        Block::LinkedImage { uri, url, alt } => {
                                            article_image(
                                                ui,
                                                &viewport,
                                                uri,
                                                Some(url),
                                                &mut self.image_cache,
                                                &self.image_job_tx,
                                            );
                                            if let Some(alt) = alt {
                                                ui.label(
                                                    egui::RichText::new(alt)
                                                        .size(13.0)
                                                        .color(theme.muted),
                                                );
                                                ui.add_space(8.0);
                                            }
                                            index += 1;
                                        }
                                        Block::Caption(caption) => {
                                            ui.with_layout(
                                                egui::Layout::top_down(egui::Align::Center),
                                                |ui| {
                                                    ui.add(
                                                        egui::Label::new(
                                                            egui::RichText::new(caption)
                                                                .size(13.0)
                                                                .color(theme.muted),
                                                        )
                                                        .selectable(true)
                                                        .wrap(),
                                                    );
                                                },
                                            );
                                            index += 1;
                                            ui.add_space(16.0);
                                        }
                                        Block::DefinitionList(items) => {
                                            egui::Frame::new()
                                                .fill(theme.code_bg)
                                                .stroke(egui::Stroke::new(1.0, theme.border))
                                                .corner_radius(egui::CornerRadius::same(5))
                                                .inner_margin(egui::Margin::symmetric(18, 14))
                                                .show(ui, |ui| {
                                                    ui.set_width(ui.available_width());
                                                    for (item_index, item) in items.iter().enumerate() {
                                                        ui.label(
                                                            egui::RichText::new(&item.term)
                                                                .family(egui::FontFamily::Name("cjk-bold".into()))
                                                                .size(16.0)
                                                                .color(theme.text),
                                                        );
                                                        for definition in &item.definitions {
                                                            ui.horizontal(|ui| {
                                                                ui.label(
                                                                    egui::RichText::new("—")
                                                                        .color(theme.accent),
                                                                );
                                                                ui.add(
                                                                    egui::Label::new(
                                                                        egui::RichText::new(definition)
                                                                            .size(15.0)
                                                                            .color(theme.text),
                                                                    )
                                                                    .selectable(true)
                                                                    .wrap(),
                                                                );
                                                            });
                                                        }
                                                        if item_index + 1 < items.len() {
                                                            ui.add_space(10.0);
                                                        }
                                                    }
                                                });
                                            index += 1;
                                            ui.add_space(22.0);
                                        }
                                        Block::Table { rows, header_rows, column_count } => {
                                            egui::Frame::new()
                                                .stroke(egui::Stroke::new(1.0, theme.border))
                                                .corner_radius(egui::CornerRadius::same(4))
                                                .inner_margin(egui::Margin::symmetric(12, 10))
                                                .show(ui, |ui| {
                                                    egui::ScrollArea::horizontal()
                                                        .id_salt(("article-table", article_id, index))
                                                        .show(ui, |ui| {
                                                            ui.set_min_width(ui.available_width().max(420.0));
                                                            let gap = 12.0;
                                                            let columns = (*column_count).max(1) as f32;
                                                            let unit = ((ui.available_width()
                                                                - gap * (columns - 1.0))
                                                                / columns)
                                                                .max(90.0);
                                                            let logical_layout = text::table_cell_columns(rows, *column_count);
                                                            for (row_index, row) in rows.iter().enumerate() {
                                                                let fill = if row_index % 2 == 1 {
                                                                    theme.code_bg
                                                                } else {
                                                                    egui::Color32::TRANSPARENT
                                                                };
                                                                egui::Frame::new()
                                                                    .fill(fill)
                                                                    .inner_margin(egui::Margin::symmetric(8, 8))
                                                                    .show(ui, |ui| {
                                                                        ui.horizontal(|ui| {
                                                                            ui.spacing_mut().item_spacing.x = gap;
                                                                            let mut current_column = 0usize;
                                                                            for (column, cell_index) in &logical_layout[row_index] {
                                                                                if *column > current_column {
                                                                                    let skipped = *column - current_column;
                                                                                    ui.add_space(
                                                                                        unit * skipped as f32
                                                                                            + gap * skipped.saturating_sub(1) as f32,
                                                                                    );
                                                                                }
                                                                                let cell = &row[*cell_index];
                                                                                let width = unit * cell.col_span as f32
                                                                                    + gap * (cell.col_span.saturating_sub(1)) as f32;
                                                                                ui.allocate_ui_with_layout(
                                                                                    egui::vec2(width, 0.0),
                                                                                    egui::Layout::top_down(egui::Align::Min),
                                                                                    |ui| {
                                                                                        let text = egui::RichText::new(&cell.text)
                                                                                            .size(15.0)
                                                                                            .color(theme.text);
                                                                                        let text = if row_index < *header_rows || cell.header {
                                                                                            text.strong()
                                                                                        } else {
                                                                                            text
                                                                                        };
                                                                                        ui.add(egui::Label::new(text).selectable(true).wrap());
                                                                                        if cell.row_span > 1 {
                                                                                            ui.label(
                                                                                                egui::RichText::new(format!("跨 {} 行", cell.row_span))
                                                                                                    .size(10.0)
                                                                                                    .color(theme.muted),
                                                                                            );
                                                                                        }
                                                                                    },
                                                                                );
                                                                                current_column = column + cell.col_span;
                                                                            }
                                                                        });
                                                                    });
                                                            }
                                                        });
                                                });
                                            index += 1;
                                            ui.add_space(22.0);
                                        }
                                        Block::Math { source, display } => {
                                            formula_block(
                                                ui,
                                                source,
                                                *display,
                                                &mut self.formula_cache,
                                                &self.formula_job_tx,
                                            );
                                            index += 1;
                                            ui.add_space(20.0);
                                        }
                                        Block::ListItemStart { depth } => {
                                            let start = index;
                                            let item_depth = *depth;
                                            let mut list_text = String::from("▪ ");
                                            let mut list_strong_ranges = Vec::new();
                                            let mut list_inline_code_ranges = Vec::new();
                                            let mut list_link_ranges = Vec::new();
                                            let mut previous_was_strong = false;
                                            let mut previous_was_link = false;
                                            let mut previous_was_inline_code = false;
                                            let mut previous_link_had_space_after = false;
                                            let mut list_images: Vec<(
                                                String,
                                                Option<String>,
                                                Option<String>,
                                            )> = Vec::new();
                                            index += 1;
                                            while index < blocks.len() {
                                                if matches!(
                                                    &blocks[index],
                                                    Block::ListItemEnd { depth } if *depth == item_depth
                                                ) {
                                                    index += 1;
                                                    break;
                                                }
                                                let block = &blocks[index];
                                                match block {
                                                    Block::Image(uri) => {
                                                        list_images.push((uri.clone(), None, None));
                                                        index += 1;
                                                        continue;
                                                    }
                                                    Block::LinkedImage { uri, url, alt } => {
                                                        list_images.push((
                                                            uri.clone(),
                                                            Some(url.clone()),
                                                            alt.clone(),
                                                        ));
                                                        index += 1;
                                                        continue;
                                                    }
                                                    _ => {}
                                                }
                                                let value = match block {
                                                    Block::Text(text)
                                                    | Block::Strong(text)
                                                    | Block::InlineCode(text)
                                                    | Block::Link { text, .. } => text,
                                                    _ => break,
                                                };
                                                let next_link_has_prefix = matches!(
                                                    block,
                                                    Block::Link { link_start, .. } if *link_start > 0
                                                );
                                                if list_text != "▪ " {
                                                    if previous_link_had_space_after {
                                                        list_text.push(' ');
                                                    } else {
                                                        list_text.push_str(body_fragment_separator(
                                                            &list_text,
                                                            value,
                                                            previous_was_strong,
                                                            previous_was_link,
                                                            previous_was_inline_code,
                                                            matches!(block, Block::Strong(_)),
                                                            matches!(block, Block::Link { .. }),
                                                            matches!(block, Block::InlineCode(_)),
                                                            next_link_has_prefix,
                                                        ));
                                                    }
                                                }
                                                let value_start = list_text.len();
                                                list_text.push_str(value);
                                                if matches!(block, Block::Strong(_)) {
                                                    list_strong_ranges.push(
                                                        value_start..value_start + value.len(),
                                                    );
                                                }
                                                if matches!(block, Block::InlineCode(_)) {
                                                    list_inline_code_ranges.push(
                                                        value_start..value_start + value.len(),
                                                    );
                                                }
                                                if let Block::Link {
                                                    url,
                                                    link_start,
                                                    ..
                                                } = block
                                                {
                                                    list_link_ranges.push(ArticleLinkRange {
                                                        range: value_start + *link_start
                                                            ..value_start + value.len(),
                                                        url: url.clone(),
                                                    });
                                                }
                                                previous_was_strong =
                                                    matches!(block, Block::Strong(_));
                                                previous_was_link =
                                                    matches!(block, Block::Link { .. });
                                                previous_was_inline_code =
                                                    matches!(block, Block::InlineCode(_));
                                                previous_link_had_space_after = matches!(
                                                    block,
                                                    Block::Link {
                                                        space_after: true,
                                                        ..
                                                    }
                                                );
                                                index += 1;
                                            }
                                            ui.add_space(4.0);
                                            ui.horizontal(|ui| {
                                                ui.add_space(
                                                    22.0 + item_depth.saturating_sub(1) as f32 * 24.0,
                                                );
                                                ui.vertical(|ui| {
                                                    ui.set_width(ui.available_width());
                                                    if list_text != "▪ " {
                                                        selectable_text_block_with_inline_style(
                                                            ui,
                                                            article_id,
                                                            start,
                                                            &list_text,
                                                            &list_strong_ranges,
                                                            &list_inline_code_ranges,
                                                            &list_link_ranges,
                                                            ArticleTextStyle::List,
                                                            &mut selection_frame,
                                                        );
                                                    }
                                                    for (uri, link_url, alt) in &list_images {
                                                        article_image(
                                                            ui,
                                                            &viewport,
                                                            uri,
                                                            link_url.as_deref(),
                                                            &mut self.image_cache,
                                                            &self.image_job_tx,
                                                        );
                                                        if let Some(alt) = alt {
                                                            ui.label(
                                                                egui::RichText::new(alt)
                                                                    .size(13.0)
                                                                    .color(theme.muted),
                                                            );
                                                        }
                                                    }
                                                });
                                            });
                                            ui.add_space(20.0);
                                        }
                                        Block::Heading(heading) => {
                                            selectable_text_block_with_style(
                                                ui,
                                                article_id,
                                                index,
                                                heading,
                                                &[],
                                                &[],
                                                ArticleTextStyle::Heading,
                                                &mut selection_frame,
                                            );
                                            index += 1;
                                            ui.add_space(20.0);
                                        }
                                        Block::HeadingWithInlineCode {
                                            text,
                                            inline_code_ranges,
                                        } => {
                                            let inline_code_ranges = inline_code_ranges
                                                .iter()
                                                .map(|range| range.start..range.end)
                                                .collect::<Vec<_>>();
                                            selectable_text_block_with_inline_style(
                                                ui,
                                                article_id,
                                                index,
                                                text,
                                                &[],
                                                &inline_code_ranges,
                                                &[],
                                                ArticleTextStyle::Heading,
                                                &mut selection_frame,
                                            );
                                            index += 1;
                                            ui.add_space(20.0);
                                        }
                                        Block::HeadingLink {
                                            text,
                                            links,
                                        } => {
                                            let link_ranges = links
                                                .iter()
                                                .map(|link| ArticleLinkRange {
                                                    range: link.start..link.end,
                                                    url: link.url.clone(),
                                                })
                                                .collect::<Vec<_>>();
                                            selectable_text_block_with_style(
                                                ui,
                                                article_id,
                                                index,
                                                text,
                                                &[],
                                                &link_ranges,
                                                ArticleTextStyle::Heading,
                                                &mut selection_frame,
                                            );
                                            index += 1;
                                            ui.add_space(20.0);
                                        }
                                        Block::Strong(heading)
                                            if text::is_numbered_heading(heading) =>
                                        {
                                            selectable_text_block_with_style(
                                                ui,
                                                article_id,
                                                index,
                                                heading,
                                                &[],
                                                &[],
                                                ArticleTextStyle::Heading,
                                                &mut selection_frame,
                                            );
                                            index += 1;
                                            ui.add_space(20.0);
                                        }
                                        Block::ListItemEnd { .. } => {
                                            index += 1;
                                        }
                                        _ => {
                                            let start = index;
                                            let mut run = String::new();
                                            let mut strong_ranges = Vec::new();
                                            let mut inline_code_ranges = Vec::new();
                                            let mut link_ranges = Vec::new();
                                            let mut previous_was_strong = false;
                                            let mut previous_was_link = false;
                                            let mut previous_was_inline_code = false;
                                            let mut previous_link_had_space_after = false;
                                            while index < blocks.len() {
                                                let block = &blocks[index];
                                                if matches!(
                                                    block,
                                                    Block::Image(_)
                                                        | Block::LinkedImage { .. }
                                                        | Block::Heading(_)
                                                        | Block::HeadingWithInlineCode { .. }
                                                        | Block::HeadingLink { .. }
                                                        | Block::Quote(_)
                                                        | Block::Code(_)
                                                        | Block::CodeBlock { .. }
                                                        | Block::Caption(_)
                                                        | Block::DefinitionList(_)
                                                        | Block::Table { .. }
                                                        | Block::Math { .. }
                                                        | Block::ListItemStart { .. }
                                                        | Block::ListItemEnd { .. }
                                                ) || matches!(
                                                    block,
                                                    Block::Text(text) if is_bullet_text(text)
                                                ) || matches!(
                                                    block,
                                                    Block::Strong(text)
                                                        if text::is_numbered_heading(text)
                                                ) {
                                                    break;
                                                }
                                                let value = match block {
                                                    Block::Text(text)
                                                    | Block::Strong(text)
                                                    | Block::InlineCode(text)
                                                    | Block::Link { text, .. } => text,
                                                    Block::Image(_)
                                                    | Block::LinkedImage { .. }
                                                    | Block::Heading(_)
                                                    | Block::HeadingWithInlineCode { .. }
                                                    | Block::HeadingLink { .. }
                                                    | Block::Quote(_)
                                                    | Block::Code(_)
                                                    | Block::CodeBlock { .. }
                                                    | Block::Caption(_)
                                                    | Block::DefinitionList(_)
                                                    | Block::Table { .. }
                                                    | Block::Math { .. }
                                                    | Block::ListItemStart { .. }
                                                    | Block::ListItemEnd { .. } => {
                                                        unreachable!()
                                                    }
                                                };
                                                let next_link_has_prefix = matches!(
                                                    block,
                                                    Block::Link { link_start, .. }
                                                        if *link_start > 0
                                                );
                                                if !run.is_empty() {
                                                    if previous_link_had_space_after {
                                                        run.push(' ');
                                                    } else {
                                                        run.push_str(body_fragment_separator(
                                                            &run,
                                                            value,
                                                            previous_was_strong,
                                                            previous_was_link,
                                                            previous_was_inline_code,
                                                            matches!(block, Block::Strong(_)),
                                                            matches!(block, Block::Link { .. }),
                                                            matches!(block, Block::InlineCode(_)),
                                                            next_link_has_prefix,
                                                        ));
                                                    }
                                                }
                                                let value_start = run.len();
                                                run.push_str(value);
                                                let is_strong = matches!(block, Block::Strong(_));
                                                let is_link = matches!(block, Block::Link { .. });
                                                if is_strong {
                                                    strong_ranges.push(value_start..run.len());
                                                }
                                                if matches!(block, Block::InlineCode(_)) {
                                                    inline_code_ranges.push(value_start..run.len());
                                                }
                                                if is_link
                                                    && let Block::Link {
                                                        url, link_start, ..
                                                    } = block
                                                    {
                                                        link_ranges.push(ArticleLinkRange {
                                                            range: value_start + *link_start
                                                                ..run.len(),
                                                            url: url.clone(),
                                                        });
                                                    }
                                                previous_was_strong = is_strong;
                                                previous_was_link = is_link;
                                                previous_was_inline_code =
                                                    matches!(block, Block::InlineCode(_));
                                                previous_link_had_space_after = matches!(
                                                    block,
                                                    Block::Link {
                                                        space_after: true,
                                                        ..
                                                    }
                                                );
                                                index += 1;
                                            }
                                            if !run.trim().is_empty() {
                                                selectable_text_block_with_inline_style(
                                                    ui,
                                                    article_id,
                                                    start,
                                                    &run,
                                                    &strong_ranges,
                                                    &inline_code_ranges,
                                                    &link_ranges,
                                                    ArticleTextStyle::Body,
                                                    &mut selection_frame,
                                                );
                                                ui.add_space(20.0);
                                            }
                                        }
                                    }
                                }
                            },
                        );
                        ui.add_space(side_margin);
                    });
                });
                if self
                    .pending_selection_anchor
                    .as_ref()
                    .is_some_and(|selection| selection.article_id == article_id)
                    && let Some(selection) = self.pending_selection_anchor.take()
                {
                    let anchor = TextAnchor {
                        start_offset: selection.start_offset,
                        end_offset: selection.end_offset,
                        prefix: selection.anchor_prefix.clone(),
                        suffix: selection.anchor_suffix.clone(),
                    };
                    if let Some(range) = resolve_excerpt_anchor(
                        &selection_frame.plain_text,
                        &selection.selected_text,
                        &anchor,
                    ) {
                        if let Some(span) = selection_frame.spans.iter().find(|span| {
                            span.chars.start <= range.start && span.chars.end >= range.start
                        }) {
                            let offset = body_scroll_output.state.offset.y
                                + span.global_rect.top()
                                - body_scroll_output.inner_rect.top()
                                - 28.0;
                            self.pending_body_scroll = Some((article_id, offset.max(0.0)));
                            self.notice("已定位到摘录原文");
                            ctx.request_repaint();
                        }
                    } else {
                        self.notice("正文已更新，暂时找不到这段摘录");
                    }
                }
                let selection_result =
                    self.update_article_selection(&ctx, article_id, &selection_frame);
                let selection_drag_started = selection_result.drag_started;
                let selection_popup_request = selection_result.popup_request;
                let scroll_offset = body_scroll_output.state.offset;
                self.current_body_scroll = scroll_offset.y;
                let popover_matches_article = self
                    .ui_state
                    .popover()
                    .is_some_and(|popover| popover.quote.article_id == article_id);
                let popup_moved_away_from_selection = popover_matches_article
                    && self
                        .selection_popup_geometry
                        .as_ref()
                        .is_some_and(|popup| {
                            (popup.scroll_offset - scroll_offset).length_sq() > 0.25
                        });
                let popup_layout_changed = popover_matches_article
                    && self
                    .selection_popup_geometry
                    .as_ref()
                    .is_some_and(|popup| {
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
                    self.ui_state.reduce(UiAction::SetPopover(Some(SelectionPopoverState {
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
                if let Some(selection_id) = delete_selection {
                    match self.db.delete_selection(selection_id) {
                        Ok(_) => {
                            self.refresh_saved_selection_count();
                            self.notice("已删除摘录");
                        }
                        Err(error) => {
                            self.notice(format!("删除失败：{error}"));
                        }
                    }
                }
                if toggle_article_star {
                    self.toggle_star(article_id);
                }
                if toggle_article_read_later {
                    self.toggle_read_later(article_id);
                }
                if edit_article_tags {
                    self.open_tag_dialog(article_id);
                }
                if generate_article_ai {
                    self.begin_article_ai(article_id, &ctx);
                }
            });
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

fn search_preview(source: &str, query: &str, max_chars: usize) -> String {
    let text = text::content_blocks(source, None)
        .into_iter()
        .filter_map(|block| match block {
            Block::Text(value)
            | Block::Strong(value)
            | Block::InlineCode(value)
            | Block::Heading(value)
            | Block::Quote(value)
            | Block::Code(value) => Some(value),
            Block::CodeBlock { text, .. } | Block::Math { source: text, .. } => Some(text),
            Block::HeadingWithInlineCode { text, .. }
            | Block::HeadingLink { text, .. }
            | Block::Link { text, .. } => Some(text),
            Block::Table { rows, .. } => Some(
                rows.into_iter()
                    .flatten()
                    .map(|cell| cell.text)
                    .collect::<Vec<_>>()
                    .join(" "),
            ),
            Block::Caption(value) => Some(value),
            Block::DefinitionList(items) => Some(
                items
                    .into_iter()
                    .flat_map(|item| std::iter::once(item.term).chain(item.definitions))
                    .collect::<Vec<_>>()
                    .join(" "),
            ),
            Block::ListItemStart { .. }
            | Block::ListItemEnd { .. }
            | Block::Image(_)
            | Block::LinkedImage { .. } => None,
        })
        .collect::<Vec<_>>()
        .join(" ");
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
        egui::FontId::proportional(15.0),
        icon_color,
    );
    ui.painter().text(
        egui::pos2(rect.center().x, rect.bottom() - 9.0),
        egui::Align2::CENTER_CENTER,
        label,
        egui::FontId::proportional(11.0),
        label_color,
    );
    response
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .clicked()
}

fn article_cursor_under_pointer(
    frame: &ArticleSelectionFrame,
    position: egui::Pos2,
) -> Option<ArticleDocCursor> {
    if let Some((span_index, span)) = frame
        .spans
        .iter()
        .enumerate()
        .find(|(_, span)| span.pointer_local_char.is_some())
    {
        let local_char = span
            .pointer_local_char
            .unwrap_or_default()
            .min(span.chars.end.saturating_sub(span.chars.start));
        return Some(ArticleDocCursor {
            span_index,
            local_char,
            char_index: span.chars.start + local_char,
        });
    }
    frame
        .spans
        .iter()
        .enumerate()
        // Use the transformed interaction rectangle instead of
        // `Response::contains_pointer`.  The latter can be false while egui's
        // label-selection plugin is already dragging (the selection paint is
        // then considered the covering layer), even though the pointer is
        // still inside the article row.  Requiring it here made the native
        // blue selection appear without arming our article-level popup.
        .find(|(_, span)| span.global_rect.contains(position))
        .map(|(span_index, span)| article_cursor_in_span(span_index, span, position))
}

fn article_cursor_for_pointer(
    frame: &ArticleSelectionFrame,
    position: egui::Pos2,
) -> Option<ArticleDocCursor> {
    article_cursor_under_pointer(frame, position).or_else(|| {
        // A drag may start in the whitespace between two short labels (or on
        // an image).  Treat the nearest row as the insertion point, but only
        // inside the article's own bounding box so clicks in the sidebars do
        // not accidentally start an article selection.
        let bounds = frame
            .spans
            .iter()
            .map(|span| span.global_rect)
            .reduce(|left, right| left.union(right))?;
        if bounds.expand(10.0).contains(position) {
            article_cursor_nearest(frame, position)
        } else {
            None
        }
    })
}

fn article_cursor_nearest(
    frame: &ArticleSelectionFrame,
    position: egui::Pos2,
) -> Option<ArticleDocCursor> {
    if let Some((span_index, span)) = frame
        .spans
        .iter()
        .enumerate()
        .find(|(_, span)| span.pointer_local_char.is_some())
    {
        let local_char = span
            .pointer_local_char
            .unwrap_or_default()
            .min(span.chars.end.saturating_sub(span.chars.start));
        return Some(ArticleDocCursor {
            span_index,
            local_char,
            char_index: span.chars.start + local_char,
        });
    }
    frame
        .spans
        .iter()
        .enumerate()
        .min_by(|(_, left), (_, right)| {
            vertical_distance(left.global_rect, position)
                .total_cmp(&vertical_distance(right.global_rect, position))
                .then_with(|| {
                    horizontal_distance(left.global_rect, position)
                        .total_cmp(&horizontal_distance(right.global_rect, position))
                })
        })
        .map(|(span_index, span)| article_cursor_in_span(span_index, span, position))
}

fn article_cursor_in_span(
    span_index: usize,
    span: &RenderedArticleSpan,
    position: egui::Pos2,
) -> ArticleDocCursor {
    let local_position = span.global_from_galley.inverse() * position;
    let galley_cursor = span.galley.cursor_from_pos(local_position.to_vec2());
    let span_len = span.chars.end.saturating_sub(span.chars.start);
    let local_char = usize::from(galley_cursor.index).min(span_len);
    ArticleDocCursor {
        span_index,
        local_char,
        char_index: span.chars.start + local_char,
    }
}

fn article_cursor_anchor(
    frame: &ArticleSelectionFrame,
    cursor: ArticleDocCursor,
) -> Option<(egui::Rect, egui::LayerId)> {
    let span = frame.spans.get(cursor.span_index)?;
    let local_char = cursor
        .local_char
        .min(span.chars.end.saturating_sub(span.chars.start));
    let cursor_rect = span
        .galley
        .pos_from_cursor(egui::text::CCursor::new(local_char));
    let global_rect = (span.global_from_galley * cursor_rect).expand(3.0);
    Some((global_rect, span.source_layer))
}

fn horizontal_distance(rect: egui::Rect, position: egui::Pos2) -> f32 {
    if position.x < rect.left() {
        rect.left() - position.x
    } else if position.x > rect.right() {
        position.x - rect.right()
    } else {
        0.0
    }
}

fn vertical_distance(rect: egui::Rect, position: egui::Pos2) -> f32 {
    if position.y < rect.top() {
        rect.top() - position.y
    } else if position.y > rect.bottom() {
        position.y - rect.bottom()
    } else {
        0.0
    }
}

fn selected_quote_from_article_text(
    article_id: i64,
    text: &str,
    start: usize,
    end: usize,
) -> Option<SelectedQuote> {
    let chars: Vec<char> = text.chars().collect();
    let mut lo = start.min(end).min(chars.len());
    let mut hi = start.max(end).min(chars.len());

    while lo < hi && chars[lo].is_whitespace() {
        lo += 1;
    }
    while hi > lo && chars[hi - 1].is_whitespace() {
        hi -= 1;
    }
    if lo >= hi {
        return None;
    }

    let anchor = TextAnchor::capture(text, lo, hi, 64);
    Some(SelectedQuote {
        article_id,
        text: chars[lo..hi].iter().collect(),
        start_offset: anchor.start_offset,
        end_offset: anchor.end_offset,
        anchor_prefix: anchor.prefix,
        anchor_suffix: anchor.suffix,
    })
}

fn rect_changed(a: egui::Rect, b: egui::Rect) -> bool {
    (a.min - b.min).length_sq() > 0.25 || (a.max - b.max).length_sq() > 0.25
}

fn is_bullet_text(text: &str) -> bool {
    matches!(
        text.trim_start().chars().next(),
        Some('▪' | '•' | '·' | '‣' | '◦')
    )
}

fn body_block_separator(
    previous: &str,
    next: &str,
    previous_was_strong: bool,
    previous_was_link: bool,
    next_is_strong: bool,
    next_is_link: bool,
    next_link_has_prefix: bool,
) -> &'static str {
    // A marker such as `（1）` can be followed by an inline `<strong>` run in
    // the same HTML paragraph. Keep the marker and its label on one line.
    if text::is_numbered_marker_only(previous.trim()) {
        return " ";
    }
    // A citation paragraph is often followed by a bare numbered paragraph
    // (`2、...`, `3、...`) or by another citation paragraph. Those are
    // separate source paragraphs even though the HTML parser exposes them as
    // adjacent Link/Text blocks.
    if next_is_link && !next_link_has_prefix {
        // A link that starts at offset zero belongs to a new source
        // paragraph.  This covers citation lists such as
        // `稳定币的博弈（#357）` followed by `不要看重 Product Hunt（#307）`;
        // the previous block is the plain suffix `（#357）`, so checking
        // only `previous_was_link` would incorrectly join the two entries.
        return "\n\n";
    }
    if previous_was_link && text::is_numbered_heading(next) {
        return "\n\n";
    }
    if !previous_was_strong && !previous_was_link && !next_is_strong && !next_is_link {
        return "\n\n";
    }
    let Some(previous_char) = previous.chars().rev().find(|ch| !ch.is_whitespace()) else {
        return "";
    };
    let Some(next_char) = next.chars().find(|ch| !ch.is_whitespace()) else {
        return "";
    };
    // Some sites place only part of a word inside an anchor, e.g.
    // `<a>modif</a>y`. The link and suffix remain separate semantic blocks so
    // the click range is exact, but they must render as one visible word.
    if previous_was_link && is_ascii_word_char(previous_char) && is_ascii_word_char(next_char) {
        return "";
    }
    if is_closing_punctuation(next_char) {
        return "";
    }
    if is_sentence_ending(previous_char) || (previous_was_strong && next_is_strong) {
        return "\n\n";
    }
    // Citation prefixes such as `----` are kept in the same Link block now,
    // but this also handles an inline link that follows a plain dash prefix.
    if next_is_link && matches!(previous_char, '-' | '—' | '–') {
        return " ";
    }
    if needs_typographic_space(previous_char, next_char) {
        " "
    } else {
        ""
    }
}

#[allow(clippy::too_many_arguments)]
fn body_fragment_separator(
    previous: &str,
    next: &str,
    previous_was_strong: bool,
    previous_was_link: bool,
    previous_was_inline_code: bool,
    next_is_strong: bool,
    next_is_link: bool,
    next_is_inline_code: bool,
    next_link_has_prefix: bool,
) -> &'static str {
    if previous_was_inline_code || next_is_inline_code {
        let Some(previous_char) = previous.chars().rev().find(|ch| !ch.is_whitespace()) else {
            return "";
        };
        let Some(next_char) = next.chars().find(|ch| !ch.is_whitespace()) else {
            return "";
        };
        if is_closing_punctuation(next_char)
            || matches!(previous_char, '(' | '[' | '{' | '<' | '/' | '\\')
        {
            return "";
        }
        if needs_typographic_space(previous_char, next_char)
            || (previous_was_inline_code && next_char.is_alphanumeric())
            || (next_is_inline_code && previous_char.is_alphanumeric())
        {
            return " ";
        }
        return "";
    }
    body_block_separator(
        previous,
        next,
        previous_was_strong,
        previous_was_link,
        next_is_strong,
        next_is_link,
        next_link_has_prefix,
    )
}

fn is_ascii_word_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || ch == '_'
}

fn is_closing_punctuation(ch: char) -> bool {
    matches!(
        ch,
        '。' | '．'
            | '.'
            | '，'
            | ','
            | '！'
            | '!'
            | '？'
            | '?'
            | '：'
            | ':'
            | '；'
            | ';'
            | '、'
            | ')'
            | ']'
            | '}'
            | '）'
            | '】'
            | '》'
            | '”'
            | '’'
    )
}

fn is_sentence_ending(ch: char) -> bool {
    matches!(ch, '。' | '．' | '.' | '！' | '!' | '？' | '?')
}

#[cfg(test)]
fn is_punctuation_only(text: &str) -> bool {
    let mut chars = text.trim().chars();
    let Some(first) = chars.next() else {
        return false;
    };
    is_closing_punctuation(first) && chars.all(is_closing_punctuation)
}

fn needs_typographic_space(left: char, right: char) -> bool {
    let left_word = left.is_alphanumeric() || left == '_';
    let right_word = right.is_alphanumeric() || right == '_';
    left_word && right_word && (left.is_ascii() || right.is_ascii())
}

/// 为图片预留稳定空间，只在接近可视区域时才启动 HTTP 请求。
/// 这样打开包含几十张图片的长文章时，不会瞬间发出全部请求。
/// Render one semantic text block and register it with egui's cross-label
/// selection plugin. Each block keeps its own typography while the app-level
/// selection model makes the whole article behave like one continuous page.
#[derive(Debug, Clone, Copy)]
enum ArticleTextStyle {
    Title,
    Body,
    Heading,
    List,
    Quote,
    Code,
}

#[allow(clippy::too_many_arguments)]
fn selectable_text_block_with_style(
    ui: &mut egui::Ui,
    article_id: i64,
    block_index: usize,
    text: &str,
    strong_ranges: &[Range<usize>],
    link_ranges: &[ArticleLinkRange],
    style: ArticleTextStyle,
    selection_frame: &mut ArticleSelectionFrame,
) -> egui::Response {
    selectable_text_block_with_inline_style(
        ui,
        article_id,
        block_index,
        text,
        strong_ranges,
        &[],
        link_ranges,
        style,
        selection_frame,
    )
}

#[allow(clippy::too_many_arguments)]
fn selectable_text_block_with_inline_style(
    ui: &mut egui::Ui,
    article_id: i64,
    block_index: usize,
    text: &str,
    strong_ranges: &[Range<usize>],
    inline_code_ranges: &[Range<usize>],
    link_ranges: &[ArticleLinkRange],
    style: ArticleTextStyle,
    selection_frame: &mut ArticleSelectionFrame,
) -> egui::Response {
    let heading_inset = if matches!(style, ArticleTextStyle::Heading) {
        15.0
    } else {
        0.0
    };
    let available_width = ui.available_width().max(1.0);
    let job = article_layout_job(
        style,
        text,
        strong_ranges,
        inline_code_ranges,
        link_ranges,
        (available_width - heading_inset).max(1.0),
    );
    let galley = ui.fonts_mut(|fonts| fonts.layout_job(job));
    let mut selection_sense = egui::Sense::click_and_drag();
    selection_sense -= egui::Sense::FOCUSABLE;
    let (row_rect, mut response) = ui
        .push_id(
            ("article-selectable-label", article_id, block_index),
            |ui| {
                ui.allocate_exact_size(
                    egui::vec2(available_width, galley.size().y),
                    selection_sense,
                )
            },
        )
        .inner;
    response.set_intrinsic_size(galley.intrinsic_size());
    let galley_pos = row_rect.left_top() + egui::vec2(heading_inset, 0.0);
    if matches!(style, ArticleTextStyle::Heading) {
        let theme = ReaderTheme::sspai();
        ui.painter().rect_filled(
            egui::Rect::from_min_size(row_rect.left_top(), egui::vec2(6.0, galley.size().y)),
            egui::CornerRadius::same(1),
            theme.accent,
        );
    }
    response.widget_info(|| {
        egui::WidgetInfo::labeled(egui::WidgetType::Label, ui.is_enabled(), galley.text())
    });
    let layer_to_global = ui
        .ctx()
        .layer_transform_to_global(response.layer_id)
        .unwrap_or_default();
    let global_from_galley =
        layer_to_global * egui::emath::TSTransform::from_translation(galley_pos.to_vec2());
    let global_text_rect =
        global_from_galley * egui::Rect::from_min_size(egui::Pos2::ZERO, galley.size());
    let global_rect = layer_to_global * response.interact_rect;
    let pointer_local_char = response
        .contains_pointer()
        .then(|| response.interact_pointer_pos())
        .flatten()
        .map(|pointer| {
            let local = global_from_galley.inverse() * pointer;
            usize::from(galley.cursor_from_pos(local.to_vec2()).index)
        });

    // Keep anchors interactive even though the paragraph is rendered through
    // a selectable label. A plain click on a link opens it; dragging belongs
    // to LabelSelectionState and never opens a browser tab.
    if response.clicked()
        && !response.double_clicked()
        && !response.triple_clicked()
        && !link_ranges.is_empty()
        && let Some(pointer) = response.interact_pointer_pos()
        && global_text_rect.contains(pointer)
    {
        let local = global_from_galley.inverse() * pointer;
        let cursor = galley.cursor_from_pos(local.to_vec2());
        let char_index: usize = cursor.index.into();
        let byte_index = text
            .char_indices()
            .nth(char_index)
            .map(|(offset, _)| offset)
            .unwrap_or(text.len());
        let exact = link_ranges
            .iter()
            .find(|link| link.range.contains(&byte_index));
        let link = exact.or_else(|| {
            (byte_index > 0)
                .then(|| {
                    link_ranges
                        .iter()
                        .find(|link| link.range.contains(&(byte_index - 1)))
                })
                .flatten()
        });
        if let Some(link) = link {
            open_in_browser(&link.url);
        }
    }

    // Register every label with the cross-widget selection plugin, including
    // labels outside the current clip. The painter clips them, while the
    // plugin still sees both endpoints during long selections.
    egui::text_selection::LabelSelectionState::label_text_selection(
        ui,
        &response,
        galley_pos,
        galley.clone(),
        article_text_color(style),
        egui::Stroke::NONE,
    );
    selection_frame.push_span(
        text,
        RenderedArticleSpan {
            chars: 0..0,
            galley,
            global_from_galley,
            global_rect,
            source_layer: response.layer_id,
            pointer_local_char,
        },
    );
    response
}

fn article_text_color(style: ArticleTextStyle) -> egui::Color32 {
    let theme = ReaderTheme::sspai();
    match style {
        ArticleTextStyle::Title
        | ArticleTextStyle::Body
        | ArticleTextStyle::Heading
        | ArticleTextStyle::List => theme.text,
        ArticleTextStyle::Quote => theme.muted,
        ArticleTextStyle::Code => egui::Color32::from_rgb(102, 102, 102),
    }
}

fn article_layout_job(
    style: ArticleTextStyle,
    text: &str,
    strong_ranges: &[Range<usize>],
    inline_code_ranges: &[Range<usize>],
    link_ranges: &[ArticleLinkRange],
    wrap_width: f32,
) -> egui::text::LayoutJob {
    let mut job = egui::text::LayoutJob::default();
    let theme = ReaderTheme::sspai();
    let (font_size, line_height, normal_color) = match style {
        ArticleTextStyle::Title => (30.0, 38.0, theme.text),
        ArticleTextStyle::Body => (15.0, 27.0, theme.text),
        ArticleTextStyle::Heading => (21.0, 29.4, theme.text),
        ArticleTextStyle::List => (15.0, 27.0, theme.text),
        ArticleTextStyle::Quote => (15.0, 27.0, theme.muted),
        ArticleTextStyle::Code => (13.0, 18.6, egui::Color32::from_rgb(102, 102, 102)),
    };
    let normal = egui::text::TextFormat {
        font_id: egui::FontId::new(
            font_size,
            if matches!(style, ArticleTextStyle::Code) {
                egui::FontFamily::Monospace
            } else {
                egui::FontFamily::Proportional
            },
        ),
        line_height: Some(line_height),
        color: normal_color,
        ..Default::default()
    };
    let heading = egui::text::TextFormat {
        font_id: egui::FontId::new(font_size, egui::FontFamily::Name("cjk-bold".into())),
        line_height: Some(line_height),
        color: theme.text,
        ..normal.clone()
    };
    if matches!(style, ArticleTextStyle::Code) {
        job.append(text, 0.0, normal);
    } else if matches!(style, ArticleTextStyle::List) {
        append_list_layout(
            &mut job,
            text,
            strong_ranges,
            inline_code_ranges,
            link_ranges,
            &normal,
            &heading,
        );
    } else if matches!(style, ArticleTextStyle::Title) {
        job.append(text, 0.0, heading);
    } else if matches!(style, ArticleTextStyle::Heading) {
        append_body_layout(
            &mut job,
            text,
            &[],
            inline_code_ranges,
            link_ranges,
            &heading,
            &heading,
        );
    } else {
        append_body_layout(
            &mut job,
            text,
            strong_ranges,
            inline_code_ranges,
            link_ranges,
            &normal,
            &heading,
        );
    }
    job.wrap.max_width = wrap_width.max(1.0);
    job.keep_trailing_whitespace = true;
    job
}

fn append_body_layout(
    job: &mut egui::text::LayoutJob,
    text: &str,
    strong_ranges: &[Range<usize>],
    inline_code_ranges: &[Range<usize>],
    link_ranges: &[ArticleLinkRange],
    normal: &egui::text::TextFormat,
    strong: &egui::text::TextFormat,
) {
    let mut boundaries = vec![0, text.len()];
    boundaries.extend(
        strong_ranges
            .iter()
            .flat_map(|range| [range.start, range.end]),
    );
    boundaries.extend(
        inline_code_ranges
            .iter()
            .flat_map(|range| [range.start, range.end]),
    );
    boundaries.extend(
        link_ranges
            .iter()
            .flat_map(|link| [link.range.start, link.range.end]),
    );
    boundaries.sort_unstable();
    boundaries.dedup();

    let mut link_format = normal.clone();
    link_format.color = ReaderTheme::sspai().link;
    link_format.underline = egui::Stroke::NONE;
    let mut inline_code_format = normal.clone();
    inline_code_format.font_id = egui::FontId::new(
        (normal.font_id.size * 0.92).max(12.0),
        egui::FontFamily::Monospace,
    );
    inline_code_format.background = ReaderTheme::sspai().code_bg;

    for pair in boundaries.windows(2) {
        let start = pair[0].min(text.len());
        let end = pair[1].max(start).min(text.len());
        if start >= end || !text.is_char_boundary(start) || !text.is_char_boundary(end) {
            continue;
        }
        let format = if link_ranges
            .iter()
            .any(|link| link.range.start <= start && end <= link.range.end)
        {
            link_format.clone()
        } else if inline_code_ranges
            .iter()
            .any(|range| range.start <= start && end <= range.end)
        {
            inline_code_format.clone()
        } else if strong_ranges
            .iter()
            .any(|range| range.start <= start && end <= range.end)
        {
            strong.clone()
        } else {
            normal.clone()
        };
        job.append(&text[start..end], 0.0, format);
    }
}

fn append_list_layout(
    job: &mut egui::text::LayoutJob,
    text: &str,
    explicit_strong_ranges: &[Range<usize>],
    inline_code_ranges: &[Range<usize>],
    link_ranges: &[ArticleLinkRange],
    normal: &egui::text::TextFormat,
    strong: &egui::text::TextFormat,
) {
    // The value after the final colon is the part users normally scan for in
    // a comparison list, so give it the installed bold CJK face. Build those
    // ranges first, then use the same range compositor as body paragraphs so
    // links retain their blue/underlined treatment inside a list card.
    let mut strong_ranges = explicit_strong_ranges.to_vec();
    let mut offset = 0;
    for line in text.split('\n') {
        if let Some(colon) = line.rfind(['：', ':']) {
            let split_at = colon + line[colon..].chars().next().unwrap().len_utf8();
            if split_at < line.len() {
                strong_ranges.push(offset + split_at..offset + line.len());
            }
        }
        offset += line.len() + 1;
    }
    append_body_layout(
        job,
        text,
        &strong_ranges,
        inline_code_ranges,
        link_ranges,
        normal,
        strong,
    );
}

fn formula_block(
    ui: &mut egui::Ui,
    source: &str,
    display: bool,
    cache: &mut HashMap<String, FormulaState>,
    jobs: &std_mpsc::Sender<FormulaJob>,
) {
    let theme = ReaderTheme::sspai();
    let key = format!("{}\n{source}", if display { "display" } else { "inline" });
    if !cache.contains_key(&key) {
        let job = FormulaJob {
            key: key.clone(),
            source: source.to_owned(),
            display,
        };
        if jobs.send(job).is_ok() {
            cache.insert(key.clone(), FormulaState::Loading);
        } else {
            cache.insert(
                key.clone(),
                FormulaState::Failed("公式排版服务没有响应".to_owned()),
            );
        }
    }

    egui::Frame::new()
        .fill(theme.code_bg)
        .corner_radius(egui::CornerRadius::same(4))
        .inner_margin(egui::Margin::symmetric(18, 12))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            match cache.get(&key) {
                Some(FormulaState::Ready(bytes)) => {
                    let mut hasher = std::collections::hash_map::DefaultHasher::new();
                    key.hash(&mut hasher);
                    let image = egui::Image::from_bytes(
                        format!("bytes://formula/{:016x}.svg", hasher.finish()),
                        bytes.clone(),
                    )
                    .max_width(ui.available_width())
                    .max_height(if display { 180.0 } else { 72.0 })
                    .maintain_aspect_ratio(true)
                    .show_loading_spinner(false);
                    if display {
                        ui.with_layout(egui::Layout::top_down(egui::Align::Center), |ui| {
                            ui.add(image).on_hover_text(format!("TeX：{source}"));
                        });
                    } else {
                        ui.add(image).on_hover_text(format!("TeX：{source}"));
                    }
                }
                Some(FormulaState::Loading) => {
                    ui.horizontal(|ui| {
                        ui.add(egui::Spinner::new().size(16.0));
                        ui.label(
                            egui::RichText::new("正在排版公式…")
                                .size(13.0)
                                .color(theme.muted),
                        );
                    });
                }
                Some(FormulaState::Failed(error)) => {
                    ui.label(
                        egui::RichText::new(source)
                            .monospace()
                            .size(if display { 17.0 } else { 15.0 })
                            .color(theme.text),
                    )
                    .on_hover_text(format!("公式排版失败，已保留 TeX 源文本：{error}"));
                }
                None => {}
            }
        });
}

fn article_image(
    ui: &mut egui::Ui,
    viewport: &egui::Rect,
    uri: &str,
    link_url: Option<&str>,
    cache: &mut HashMap<String, ImageState>,
    job_tx: &std_mpsc::Sender<String>,
) {
    let available_width = ui.available_width();
    let width = available_width;
    let theme = ReaderTheme::sspai();
    let natural_dimensions = cache.get(uri).and_then(|state| match state {
        ImageState::Ready { dimensions, .. } => *dimensions,
        _ => None,
    });
    let height = natural_dimensions
        .filter(|(w, h)| *w > 0 && *h > 0)
        .map(|(w, h)| (width * h as f32 / w as f32).clamp(160.0, 900.0))
        .unwrap_or_else(|| match cache.get(uri) {
            Some(ImageState::Failed(_)) => 180.0,
            _ => (width * 0.42).clamp(200.0, 340.0),
        });
    let left_margin = ((available_width - width) * 0.5).max(0.0);
    let allocated = ui.allocate_ui_with_layout(
        egui::vec2(available_width, height),
        egui::Layout::left_to_right(egui::Align::Center),
        |ui| {
            ui.add_space(left_margin);
            ui.allocate_exact_size(egui::vec2(width, height), egui::Sense::click())
        },
    );
    let (rect, response) = allocated.inner;

    // 提前一屏开始下载，通常滚动到图片时已经加载完成。
    let content_origin = ui.max_rect().min;
    let content_rect = rect.translate(-content_origin.to_vec2());
    let preload = viewport.expand2(egui::vec2(0.0, 600.0));
    if content_rect.intersects(preload) {
        if !cache.contains_key(uri) {
            if let Err(error) = queue_image_download(uri, job_tx) {
                cache.insert(uri.to_owned(), ImageState::Failed(error));
            } else {
                cache.insert(
                    uri.to_owned(),
                    ImageState::Loading {
                        started: Instant::now(),
                        attempt: 1,
                    },
                );
            }
        }
        match cache.get(uri) {
            Some(ImageState::Ready { bytes, .. }) => {
                ui.put(
                    rect,
                    egui::Image::from_bytes(format!("bytes://{uri}"), bytes.clone())
                        .fit_to_exact_size(rect.size())
                        .maintain_aspect_ratio(true)
                        .corner_radius(egui::CornerRadius::same(5))
                        .show_loading_spinner(false),
                );
            }
            Some(ImageState::Failed(error)) => {
                ui.painter()
                    .rect_filled(rect, egui::CornerRadius::same(5), theme.code_bg);
                let attempts = if error.attempts > 1 {
                    format!("，已自动尝试 {} 次", error.attempts)
                } else {
                    String::new()
                };
                ui.painter().text(
                    rect.center(),
                    egui::Align2::CENTER_CENTER,
                    format!(
                        "图片暂时无法加载\n{}{}\n单击重新加载 · 右键可在浏览器中打开",
                        error.message, attempts
                    ),
                    egui::FontId::proportional(13.0),
                    ui.visuals().error_fg_color,
                );
                response.clone().on_hover_text(&error.detail);
            }
            Some(ImageState::Loading { started, attempt }) => {
                ui.painter()
                    .rect_filled(rect, egui::CornerRadius::same(5), theme.code_bg);
                let spinner_rect =
                    egui::Rect::from_center_size(rect.center(), egui::vec2(28.0, 28.0));
                ui.put(spinner_rect, egui::Spinner::new().size(24.0));
                ui.painter().text(
                    rect.center() + egui::vec2(0.0, 34.0),
                    egui::Align2::CENTER_CENTER,
                    format!(
                        "{}… {:.0}s",
                        if *attempt > 1 {
                            format!("正在自动重试 {attempt}/{IMAGE_MAX_ATTEMPTS}")
                        } else {
                            "正在下载".to_owned()
                        },
                        started.elapsed().as_secs_f32(),
                    ),
                    egui::FontId::proportional(13.0),
                    ui.visuals().weak_text_color(),
                );
            }
            None => {}
        }
    } else {
        ui.painter()
            .rect_filled(rect, egui::CornerRadius::same(5), theme.code_bg);
        ui.painter().text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            "滚动到这里后加载图片",
            egui::FontId::proportional(14.0),
            ui.visuals().weak_text_color(),
        );
    }

    let mut retry = false;
    if response.clicked() {
        if matches!(cache.get(uri), Some(ImageState::Failed(_))) {
            retry = true;
        } else if let Some(url) = link_url {
            open_in_browser(url);
        }
    }
    response.context_menu(|ui| {
        if ui.button("重新加载图片").clicked() {
            retry = true;
            ui.close();
        }
        if ui.button("在浏览器中打开图片").clicked() {
            open_in_browser(uri);
            ui.close();
        }
        if let Some(url) = link_url
            && ui.button("打开图片链接").clicked()
        {
            open_in_browser(url);
            ui.close();
        }
    });
    if retry {
        let can_retry = cache
            .get(uri)
            .is_some_and(|state| matches!(state, ImageState::Failed(_)));
        if can_retry {
            ui.ctx().forget_image(&format!("bytes://{uri}"));
            if let Err(error) = queue_image_download(uri, job_tx) {
                cache.insert(uri.to_owned(), ImageState::Failed(error));
            } else {
                cache.insert(
                    uri.to_owned(),
                    ImageState::Loading {
                        started: Instant::now(),
                        attempt: 1,
                    },
                );
            }
            ui.ctx().request_repaint();
        }
    }
    ui.add_space(15.0);
}

fn queue_image_download(uri: &str, job_tx: &std_mpsc::Sender<String>) -> Result<(), ImageFailure> {
    job_tx.send(uri.to_owned()).map_err(|error| ImageFailure {
        message: "图片下载服务没有响应".to_owned(),
        detail: error.to_string(),
        attempts: 0,
        retryable: true,
    })
}

fn spawn_formula_worker(
    jobs: std_mpsc::Receiver<FormulaJob>,
    events: std_mpsc::Sender<FormulaEvent>,
) {
    std::thread::Builder::new()
        .name("shiyue-mathjax".to_owned())
        .spawn(move || {
            let renderer = match std::panic::catch_unwind(mathjax_svg_rs::MathJax::new) {
                Ok(renderer) => renderer,
                Err(_) => {
                    while let Ok(job) = jobs.recv() {
                        let _ = events.send(FormulaEvent::Complete {
                            key: job.key,
                            result: Err("MathJax 初始化失败".to_owned()),
                        });
                    }
                    return;
                }
            };
            while let Ok(job) = jobs.recv() {
                let options = mathjax_svg_rs::Options {
                    font_size: if job.display { 19.0 } else { 16.0 },
                    horizontal_align: if job.display {
                        mathjax_svg_rs::HorizontalAlign::Center
                    } else {
                        mathjax_svg_rs::HorizontalAlign::Left
                    },
                };
                let result = renderer
                    .render_tex(&job.source, &options)
                    .map(|svg| Arc::<[u8]>::from(svg.into_bytes()));
                let _ = events.send(FormulaEvent::Complete {
                    key: job.key,
                    result,
                });
            }
        })
        .expect("公式排版线程创建失败");
}

fn spawn_image_workers(
    client: reqwest::blocking::Client,
    job_rx: std_mpsc::Receiver<String>,
    event_tx: std_mpsc::Sender<ImageEvent>,
    store: Arc<ImageStore>,
) {
    let job_rx = Arc::new(Mutex::new(job_rx));
    for worker in 0..IMAGE_WORKER_COUNT {
        let client = client.clone();
        let job_rx = job_rx.clone();
        let event_tx = event_tx.clone();
        let store = store.clone();
        std::thread::Builder::new()
            .name(format!("shiyue-image-{worker}"))
            .spawn(move || {
                loop {
                    // std::mpsc has one consumer, so only hold the mutex while
                    // receiving a job. The network request itself remains fully
                    // concurrent across the bounded worker pool.
                    let uri = {
                        let Ok(receiver) = job_rx.lock() else {
                            return;
                        };
                        let Ok(uri) = receiver.recv() else {
                            return;
                        };
                        uri
                    };
                    let result = load_cached_or_download_image(&client, &store, &uri, &event_tx);
                    if event_tx.send(ImageEvent::Complete { uri, result }).is_err() {
                        return;
                    }
                }
            })
            .expect("failed to spawn image worker");
    }
}

fn load_cached_or_download_image(
    client: &reqwest::blocking::Client,
    store: &ImageStore,
    uri: &str,
    event_tx: &std_mpsc::Sender<ImageEvent>,
) -> Result<Arc<[u8]>, ImageFailure> {
    match store.get(uri) {
        Ok(Some(bytes)) if image::load_from_memory(&bytes).is_ok() => {
            return Ok(Arc::from(bytes));
        }
        Ok(_) => {}
        Err(error) => tracing::warn!("读取图片缓存失败：{error:#}"),
    }

    let bytes = download_image_with_retry(client, uri, event_tx)?;
    image::load_from_memory(bytes.as_ref()).map_err(|error| ImageFailure {
        message: "图片格式无法解码".to_owned(),
        detail: error.to_string(),
        attempts: 1,
        retryable: false,
    })?;
    if let Err(error) = store.put(uri, bytes.as_ref()) {
        tracing::warn!("写入图片缓存失败：{error:#}");
    } else if let Err(error) = store.prune_to(DEFAULT_LIMIT_BYTES) {
        tracing::warn!("清理图片缓存失败：{error:#}");
    }
    Ok(bytes)
}

fn download_image_with_retry(
    client: &reqwest::blocking::Client,
    uri: &str,
    event_tx: &std_mpsc::Sender<ImageEvent>,
) -> Result<Arc<[u8]>, ImageFailure> {
    let mut last_failure = None;
    for attempt in 1..=IMAGE_MAX_ATTEMPTS {
        if attempt > 1 {
            let _ = event_tx.send(ImageEvent::Progress {
                uri: uri.to_owned(),
                attempt,
            });
            std::thread::sleep(match attempt {
                2 => Duration::from_millis(500),
                _ => Duration::from_millis(1_500),
            });
        }

        match download_image_once(client, uri, attempt) {
            Ok(bytes) => return Ok(bytes),
            Err(failure) => {
                let should_retry = failure.retryable && attempt < IMAGE_MAX_ATTEMPTS;
                last_failure = Some(failure);
                if !should_retry {
                    break;
                }
            }
        }
    }

    Err(last_failure.unwrap_or_else(|| ImageFailure {
        message: "图片下载未完成".to_owned(),
        detail: "下载任务意外结束".to_owned(),
        attempts: 0,
        retryable: true,
    }))
}

fn download_image_once(
    client: &reqwest::blocking::Client,
    uri: &str,
    attempt: u8,
) -> Result<Arc<[u8]>, ImageFailure> {
    let url = reqwest::Url::parse(uri).map_err(|error| ImageFailure {
        message: "图片地址无效，已停止加载".to_owned(),
        detail: error.to_string(),
        attempts: attempt,
        retryable: false,
    })?;
    crate::web_clip::validate_public_url(&url).map_err(|detail| ImageFailure {
        message: "为保护本机数据，已阻止加载该图片".to_owned(),
        detail,
        attempts: attempt,
        retryable: false,
    })?;

    let response = client
        .get(url)
        // The bundled decoder supports WebP/PNG/JPEG/GIF. Do not advertise
        // AVIF: a CDN may otherwise return a healthy image we cannot decode.
        .header(
            reqwest::header::ACCEPT,
            "image/webp,image/png,image/jpeg,image/gif,*/*",
        )
        .send()
        .map_err(|error| image_request_failure(error, attempt))?;
    if let Some(peer) = response.remote_addr()
        && !crate::web_clip::is_public_ip(peer.ip())
    {
        return Err(ImageFailure {
            message: "为保护本机数据，已阻止加载该图片".to_owned(),
            detail: format!("图片服务器连接到了本机或内网地址：{}", peer.ip()),
            attempts: attempt,
            retryable: false,
        });
    }
    let response = response
        .error_for_status()
        .map_err(|error| image_request_failure(error, attempt))?;
    if response
        .content_length()
        .is_some_and(|length| length > IMAGE_MAX_BYTES)
    {
        return Err(ImageFailure {
            message: "图片文件过大，已停止下载".to_owned(),
            detail: format!(
                "图片超过 {} MB 的安全上限：{uri}",
                IMAGE_MAX_BYTES / 1024 / 1024
            ),
            attempts: attempt,
            retryable: false,
        });
    }
    let mut bytes = Vec::new();
    response
        .take(IMAGE_MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| ImageFailure {
            message: "图片传输中断".to_owned(),
            detail: error.to_string(),
            attempts: attempt,
            retryable: true,
        })?;
    if bytes.is_empty() {
        return Err(ImageFailure {
            message: "服务器返回了空图片".to_owned(),
            detail: format!("{uri} returned an empty response body"),
            attempts: attempt,
            retryable: true,
        });
    }
    if bytes.len() as u64 > IMAGE_MAX_BYTES {
        return Err(ImageFailure {
            message: "图片文件过大，已停止显示".to_owned(),
            detail: format!(
                "图片超过 {} MB 的安全上限：{uri}",
                IMAGE_MAX_BYTES / 1024 / 1024
            ),
            attempts: attempt,
            retryable: false,
        });
    }
    Ok(Arc::from(bytes))
}

fn image_request_failure(error: reqwest::Error, attempt: u8) -> ImageFailure {
    let status = error.status();
    let retryable = !error.is_redirect()
        && (error.is_timeout()
        || error.is_connect()
        // TLS renegotiation and HTTP framing failures are classified as
        // request errors rather than connect errors by reqwest. Image GETs
        // are idempotent, so retrying this transport category is safe.
        || error.is_request()
        || error.is_body()
        || status.is_some_and(image_http_status_retryable));
    let message = if error.is_timeout() {
        "连接图片服务器超时".to_owned()
    } else if error.is_connect() {
        "无法连接图片服务器".to_owned()
    } else if error.is_redirect() {
        "图片重定向不安全，已停止加载".to_owned()
    } else if let Some(status) = status {
        format!("图片服务器返回 HTTP {}", status.as_u16())
    } else if error.is_body() {
        "图片传输中断".to_owned()
    } else {
        "网络请求失败".to_owned()
    };
    ImageFailure {
        message,
        detail: format!(
            "{}\n请求阶段：{}；可自动重试：{}",
            reqwest_error_chain(&error),
            if error.is_builder() {
                "构造请求"
            } else if error.is_redirect() {
                "重定向"
            } else if error.is_body() {
                "读取响应"
            } else {
                "发送请求"
            },
            retryable
        ),
        attempts: attempt,
        retryable,
    }
}

fn image_http_status_retryable(status: reqwest::StatusCode) -> bool {
    matches!(status.as_u16(), 408 | 425 | 429) || status.is_server_error()
}

fn reqwest_error_chain(error: &reqwest::Error) -> String {
    let mut messages = vec![error.to_string()];
    let mut source = error.source();
    while let Some(cause) = source {
        let message = cause.to_string();
        if messages.last().is_none_or(|previous| previous != &message) {
            messages.push(message);
        }
        source = cause.source();
    }
    messages.join("\n原因：")
}

fn non_empty_owned(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_owned())
}

fn is_http_url(value: &str) -> bool {
    reqwest::Url::parse(value.trim())
        .ok()
        .is_some_and(|url| matches!(url.scheme(), "http" | "https"))
}

fn normalized_web_url(value: &str) -> Option<String> {
    let value = value.trim();
    if is_http_url(value) {
        return Some(value.to_owned());
    }
    if value.is_empty()
        || value.chars().any(char::is_whitespace)
        || value.starts_with('<')
        || value.contains("://")
    {
        return None;
    }
    let host = value.split(['/', '?', '#']).next().unwrap_or_default();
    if !host.contains('.') {
        return None;
    }
    let candidate = format!("https://{value}");
    is_http_url(&candidate).then_some(candidate)
}

fn resolve_http_url(value: &str, document_url: Option<&str>) -> Option<String> {
    if let Ok(url) = reqwest::Url::parse(value.trim()) {
        return matches!(url.scheme(), "http" | "https").then(|| url.to_string());
    }
    let document = reqwest::Url::parse(document_url?).ok()?;
    let joined = document.join(value.trim()).ok()?;
    matches!(joined.scheme(), "http" | "https").then(|| joined.to_string())
}

fn escape_html_attribute(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn with_html_base(content: &str, base: Option<&str>) -> String {
    match base.map(str::trim).filter(|value| !value.is_empty()) {
        Some(base) => format!(
            "<base href=\"{}\">\n{}",
            escape_html_attribute(base),
            content
        ),
        None => content.to_owned(),
    }
}

fn prepare_pasted_web_clip(
    html: &str,
    explicit_base: Option<&str>,
) -> std::result::Result<(Option<String>, String), String> {
    let snapshot = text::prepare_html_snapshot(html);
    if snapshot.content.trim().is_empty() {
        return Err("HTML 中没有识别到可阅读正文".to_owned());
    }
    let base = explicit_base
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| {
            resolve_http_url(value, None)
                .ok_or_else(|| "基础网址必须是 http:// 或 https:// 地址".to_owned())
        })
        .transpose()?
        .or_else(|| {
            snapshot
                .base_href
                .as_deref()
                .and_then(|value| resolve_http_url(value, None))
        });
    Ok((
        snapshot.title,
        with_html_base(&snapshot.content, base.as_deref()),
    ))
}

/// 用系统默认浏览器打开链接，不经过 shell/cmd 字符串解释。
fn open_in_browser(url: &str) {
    let _ = open::that_detached(url);
}

fn resource_card<R>(
    ui: &mut egui::Ui,
    add_contents: impl FnOnce(&mut egui::Ui) -> R,
) -> egui::InnerResponse<R> {
    egui::Frame::group(ui.style()).show(ui, |ui| {
        ui.set_width(ui.available_width());
        add_contents(ui)
    })
}

#[cfg(test)]
mod tests {
    use super::{
        ArticleTextStyle, FeedSettingsPanel, IMAGE_MAX_ATTEMPTS, ModalPayload, ModalState,
        PanelPayload, PanelState, ResourceEditDialog, ResourceEditValues, TagDialog, WebClipDialog,
        adopt_article_projection_data, article_layout_job, body_block_separator,
        body_fragment_separator, download_image_with_retry, image_http_status_retryable,
        is_punctuation_only, load_cached_or_download_image, normalized_web_url,
        prepare_pasted_web_clip, projection_scope_for_route, resource_card, search_match_ranges,
        search_preview, selected_quote_from_article_text,
    };
    use crate::article_library_lifecycle::{
        ArticleLibraryCounts, ArticleLibraryProjection, ProjectionScope,
    };
    use crate::gui_state::{ArticleCollection, Route};
    use crate::image_store::ImageStore;
    use crate::model::{Article, Feed};
    use eframe::egui;
    use std::collections::{HashMap, HashSet};
    use std::sync::mpsc;
    use std::time::Duration;

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
            projection_scope_for_route(Route::Archive),
            Some(ProjectionScope::Archive)
        );
        assert_eq!(projection_scope_for_route(Route::Resources), None);
    }

    #[test]
    fn successful_projection_replaces_rows_tags_and_authoritative_counts() {
        let article = Article {
            id: 11,
            feed_id: 7,
            entry_id: "entry".into(),
            url: None,
            title: Some("Title".into()),
            author: None,
            published: None,
            content: None,
            is_read: false,
            starred: true,
            read_later: false,
            archived: false,
            fetched_at: 1,
        };
        let projection = ArticleLibraryProjection {
            scope: ProjectionScope::Feed(7),
            articles: vec![article],
            tags: HashMap::from([(11, vec!["Rust".into()])]),
            fixed_bookmark_ids: HashSet::from([11]),
            counts: ArticleLibraryCounts {
                bookmarks: 4,
                read_later: 2,
                archived: 1,
            },
            feed_unread: vec![(7, 3)],
        };
        let mut articles = Vec::new();
        let mut tags = HashMap::new();
        let mut fixed = HashSet::new();
        let (mut saved, mut later, mut archived) = (0, 0, 0);
        let feed = Feed {
            id: 7,
            url: "https://example.com/feed.xml".into(),
            title: None,
            interval_secs: None,
            last_fetch: None,
            next_fetch: 0,
            last_error: None,
            fail_count: 0,
            disabled: false,
        };
        let mut feeds = vec![(feed, 99)];

        let scope = adopt_article_projection_data(
            projection,
            &mut articles,
            &mut tags,
            &mut fixed,
            &mut saved,
            &mut later,
            &mut archived,
            &mut feeds,
        );

        assert_eq!(scope, ProjectionScope::Feed(7));
        assert_eq!(articles[0].id, 11);
        assert_eq!(tags[&11], vec!["Rust"]);
        assert!(fixed.contains(&11));
        assert_eq!((saved, later, archived), (4, 2, 1));
        assert_eq!(feeds[0].1, 3);
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

        let mut web = WebClipDialog::default();
        assert!(!ModalState::SaveWebPage(web).is_dirty());
        web = WebClipDialog {
            source: "https://example.com".into(),
            ..Default::default()
        };
        assert!(ModalState::SaveWebPage(web).is_dirty());
    }

    #[test]
    fn resource_panel_compares_edits_with_its_opening_snapshot() {
        let original = ResourceEditValues {
            title: "Tool".into(),
            purpose_zh: "用途".into(),
            note: String::new(),
            private: false,
            rating: 0,
        };
        let mut dialog = ResourceEditDialog {
            id: 3,
            title: original.title.clone(),
            purpose_zh: original.purpose_zh.clone(),
            note: original.note.clone(),
            private: original.private,
            rating: original.rating,
            original,
        };
        assert!(!PanelState::ResourceEditor(dialog.clone()).is_dirty());
        dialog.note = "remember".into();
        assert!(PanelState::ResourceEditor(dialog).is_dirty());
    }

    #[test]
    fn feed_settings_panel_is_owned_by_its_feed_route_and_tracks_edits() {
        let mut panel = FeedSettingsPanel {
            feed_id: 7,
            title: "Rust Blog".into(),
            url: "https://example.com/feed.xml".into(),
            disabled: false,
            original_disabled: false,
            interval_draft: "1h".into(),
            original_interval: "1h".into(),
            error: None,
        };
        assert!(!PanelState::FeedSettings(panel.clone()).is_dirty());
        assert!(
            PanelState::FeedSettings(panel.clone())
                .is_compatible(Route::Articles(ArticleCollection::Feed(Some(7))))
        );
        assert!(
            !PanelState::FeedSettings(panel.clone())
                .is_compatible(Route::Articles(ArticleCollection::Feed(Some(8))))
        );
        panel.interval_draft = "2h".into();
        assert!(PanelState::FeedSettings(panel).is_dirty());
    }

    #[test]
    fn article_quote_uses_unicode_character_offsets_and_trims_edges() {
        let quote = selected_quote_from_article_text(42, "甲乙\n\n😀丙丁", 1, 6).unwrap();
        assert_eq!(quote.text, "乙\n\n😀丙");
        assert_eq!(quote.start_offset, Some(1));
        assert_eq!(quote.end_offset, Some(6));

        let reverse = selected_quote_from_article_text(42, "  前文 后文  ", 9, 2).unwrap();
        assert_eq!(reverse.text, "前文 后文");
        assert_eq!(reverse.start_offset, Some(2));
        assert_eq!(reverse.end_offset, Some(7));
    }

    #[test]
    fn article_quote_rejects_whitespace_only_ranges() {
        assert!(selected_quote_from_article_text(42, "甲 \n\n 乙", 1, 5).is_none());
    }

    #[test]
    fn web_clip_input_accepts_http_and_common_bare_hosts() {
        assert_eq!(
            normalized_web_url("https://example.com/a"),
            Some("https://example.com/a".to_owned())
        );
        assert_eq!(
            normalized_web_url("example.com/a"),
            Some("https://example.com/a".to_owned())
        );
        assert_eq!(normalized_web_url("<p>example.com</p>"), None);
        assert_eq!(normalized_web_url("一段普通文字"), None);
    }

    #[test]
    fn pasted_web_clip_keeps_local_base_without_using_it_as_identity() {
        let (title, html) = prepare_pasted_web_clip(
            "<title>保存页</title><article><img src='cover.webp'><p>正文</p></article>",
            Some("https://example.com/posts/1/"),
        )
        .unwrap();
        assert_eq!(title.as_deref(), Some("保存页"));
        assert!(html.starts_with("<base href=\"https://example.com/posts/1/\">"));
        assert!(html.contains("<p>正文</p>"));
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

    #[test]
    fn joins_inline_emphasis_without_breaking_chinese_punctuation() {
        assert_eq!(
            body_block_separator("（1）", "is-", false, false, false, true, false),
            " "
        );
        assert_eq!(
            body_block_separator("RTX", "5090", false, false, true, false, false),
            " "
        );
        assert_eq!(
            body_block_separator("统一内存", "。它的好处", true, false, false, false, false,),
            ""
        );
        assert_eq!(
            body_block_separator("这是完整一句。", "下一段", true, false, false, false, false,),
            "\n\n"
        );
        assert_eq!(
            body_block_separator("第一段。", "第二段。", false, false, false, false, false,),
            "\n\n"
        );
        assert_eq!(
            body_block_separator("---- 来源", "2、下一段", false, true, false, false, false,),
            "\n\n"
        );
        assert_eq!(
            body_block_separator("来源一", "来源二", false, true, false, true, false),
            "\n\n"
        );
        assert_eq!(
            body_block_separator("本杂志开源", "，欢迎投稿", false, true, false, true, true,),
            ""
        );
        assert_eq!(
            body_block_separator(
                "harder to modif",
                "y, leading",
                false,
                true,
                false,
                false,
                false
            ),
            ""
        );
        // `space_after` on Block::Link bypasses this fallback and inserts a
        // literal space for ordinary `<a>docs</a> and` markup.
        assert_eq!(
            body_block_separator(
                "（#357）",
                "不要看重 Product Hunt",
                false,
                false,
                false,
                true,
                false,
            ),
            "\n\n"
        );
    }

    #[test]
    fn inline_code_layout_stays_in_the_sentence_and_uses_monospace() {
        let text = "Installed via rustup, then update.";
        let start = text.find("rustup").unwrap();
        let end = start + "rustup".len();
        let inline_code_range = start..end;
        let job = article_layout_job(
            ArticleTextStyle::Body,
            text,
            &[],
            std::slice::from_ref(&inline_code_range),
            &[],
            1200.0,
        );

        assert_eq!(job.text, text);
        assert!(!job.text.contains('\n'));
        assert!(job.sections.iter().any(|section| {
            usize::from(section.byte_range.start) == start
                && usize::from(section.byte_range.end) == end
                && section.format.font_id.family == egui::FontFamily::Monospace
                && section.format.background != egui::Color32::TRANSPARENT
        }));
    }

    #[test]
    fn inline_code_fragments_do_not_create_paragraph_breaks() {
        assert_eq!(
            body_fragment_separator(
                "If Rust is installed via",
                "rustup",
                false,
                false,
                false,
                false,
                false,
                true,
                false,
            ),
            " "
        );
        assert_eq!(
            body_fragment_separator(
                "rustup",
                ", you can update it",
                false,
                false,
                true,
                false,
                false,
                false,
                false,
            ),
            ""
        );
    }

    #[test]
    fn list_continuation_accepts_only_punctuation() {
        assert!(is_punctuation_only("。"));
        assert!(is_punctuation_only("。）"));
        assert!(!is_punctuation_only("。下一段"));
    }

    #[test]
    fn keeps_quote_and_review_entries_on_separate_lines() {
        let blocks = [
            ("1、如果你是太阳，我就是黑洞。", false, false, false),
            ("---- 史蒂芬·霍金", false, true, true),
            ("2、AI 模型的世界就像一个城市。", false, false, false),
            ("-- 《奇点越来越近了》", false, true, true),
            ("3、AI 是一个完全的黑箱。", false, false, false),
            ("-- 《AI 是一个糟糕的工具》", false, true, true),
            ("稳定币的博弈", false, true, false),
            ("（#357）", false, false, false),
            ("不要看重 Product Hunt", false, true, false),
            ("（#307）", false, false, false),
        ];
        let mut run = String::new();
        let mut previous_was_strong = false;
        let mut previous_was_link = false;
        for (value, is_strong, is_link, link_has_prefix) in blocks {
            if !run.is_empty() {
                run.push_str(body_block_separator(
                    &run,
                    value,
                    previous_was_strong,
                    previous_was_link,
                    is_strong,
                    is_link,
                    link_has_prefix,
                ));
            }
            run.push_str(value);
            previous_was_strong = is_strong;
            previous_was_link = is_link;
        }
        assert!(run.contains("---- 史蒂芬·霍金\n\n2、"));
        assert!(run.contains("-- 《奇点越来越近了》\n\n3、"));
        assert!(run.contains("稳定币的博弈（#357）\n\n不要看重 Product Hunt"));
    }

    #[test]
    fn image_retry_policy_only_retries_transient_http_statuses() {
        assert!(image_http_status_retryable(
            reqwest::StatusCode::TOO_MANY_REQUESTS
        ));
        assert!(image_http_status_retryable(
            reqwest::StatusCode::REQUEST_TIMEOUT
        ));
        assert!(image_http_status_retryable(reqwest::StatusCode::TOO_EARLY));
        assert!(image_http_status_retryable(
            reqwest::StatusCode::BAD_GATEWAY
        ));
        assert!(!image_http_status_retryable(reqwest::StatusCode::NOT_FOUND));
        assert!(!image_http_status_retryable(reqwest::StatusCode::FORBIDDEN));
        assert_eq!(IMAGE_MAX_ATTEMPTS, 3);
    }

    #[test]
    fn persistent_image_cache_serves_a_valid_image_without_network() {
        let root = std::env::temp_dir().join(format!(
            "shiyue-gui-offline-image-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        let store = ImageStore::open(&root).unwrap();
        let mut encoded = std::io::Cursor::new(Vec::new());
        image::DynamicImage::new_rgba8(1, 1)
            .write_to(&mut encoded, image::ImageFormat::Png)
            .unwrap();
        let uri = "https://offline-cache.invalid/image.png";
        store.put(uri, encoded.get_ref()).unwrap();

        let client = reqwest::blocking::Client::builder().build().unwrap();
        let (tx, rx) = mpsc::channel();
        let bytes = load_cached_or_download_image(&client, &store, uri, &tx).unwrap();
        assert!(image::load_from_memory(bytes.as_ref()).is_ok());
        assert!(rx.try_recv().is_err(), "缓存命中不应进入网络重试流程");

        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[ignore = "live CDN smoke test; run explicitly before packaging"]
    fn downloads_reported_beekka_webp() {
        let client = reqwest::blocking::Client::builder()
            .http1_only()
            .connect_timeout(Duration::from_secs(8))
            .timeout(Duration::from_secs(30))
            .build()
            .unwrap();
        let (tx, _rx) = mpsc::channel();
        let bytes = download_image_with_retry(
            &client,
            "https://cdn.beekka.com/blogimg/asset/202608/bg2026080619.webp",
            &tx,
        )
        .unwrap();
        let decoded = image::load_from_memory(bytes.as_ref()).unwrap();
        assert!(bytes.len() > 100_000);
        assert!(decoded.width() > 0 && decoded.height() > 0);
    }
}
