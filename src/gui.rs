//! 桌面阅读器（egui/eframe，ADR-13）+ 内置抓取调度（ADR-14）+ 关窗到托盘（ADR-15）。
//! 三栏：源 | 文章 | 正文。正文按原文顺序穿插 文字/图片（ADR-16），图片原生纹理渲染。
//!
//! 进程模型：UI 在主线程（同步，自己一个 DB 连接只读）；一个后台线程起 tokio 跑调度循环
//! （另一个 DB 连接，按每源 next_fetch 到期抓取写库）。两边靠同一 WAL 库 + channel + repaint 协调。

use anyhow::Result;
use chrono::Utc;
use eframe::egui::{self, ViewportCommand};
use std::collections::{HashMap, HashSet};
use std::error::Error as _;
use std::io::Read as _;
use std::ops::Range;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tray_icon::menu::{Menu, MenuEvent, MenuId, MenuItem};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

use crate::config::{Config, Paths};
use crate::db::Db;
use crate::model::{Article, Feed};
use crate::text::{self, Block};
use crate::{daemon, fetch, notify};

const WINDOW_TITLE: &str = "拾阅 · RSS 阅读器";
const FEED_PANEL_WIDTH: f32 = 240.0;
const ARTICLE_PANEL_WIDTH: f32 = 340.0;
const ARTICLE_MAX_WIDTH: f32 = 820.0;
const IMAGE_WORKER_COUNT: usize = 4;
const IMAGE_MAX_ATTEMPTS: u8 = 3;
const IMAGE_MAX_BYTES: u64 = 25 * 1024 * 1024;

/// The reading theme mirrors the built-in “少数派经典” theme from the
/// companion markdown editor. Keeping the tokens together prevents the
/// navigation columns and the article renderer from drifting apart again.
#[derive(Clone, Copy)]
struct ReaderTheme {
    canvas: egui::Color32,
    panel: egui::Color32,
    text: egui::Color32,
    muted: egui::Color32,
    accent: egui::Color32,
    link: egui::Color32,
    border: egui::Color32,
    code_bg: egui::Color32,
    selected_bg: egui::Color32,
}

impl ReaderTheme {
    fn sspai() -> Self {
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

// ---------- 后台调度线程 ----------

/// UI 与调度线程共享的原子状态。
struct Shared {
    /// 窗口是否聚焦（UI 每帧写）；聚焦时不弹 toast。
    focused: AtomicBool,
    /// 是否正在抓取（调度线程写）；UI 据此禁用/改按钮文字。
    busy: AtomicBool,
    /// 有新文章落库（调度线程置位）；UI 见到就重读库并清零。
    dirty: AtomicBool,
}

/// UI → 调度线程的命令。
enum Cmd {
    /// 立刻抓所有启用源（工具栏/托盘"抓取"）。
    FetchNow,
}

fn spawn_scheduler(
    db_path: PathBuf,
    cfg: Config,
    ctx: egui::Context,
    shared: Arc<Shared>,
    rx: mpsc::UnboundedReceiver<Cmd>,
) {
    std::thread::spawn(move || {
        let rt = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(r) => r,
            Err(e) => {
                tracing::error!("调度线程运行时创建失败: {e}");
                return;
            }
        };
        rt.block_on(scheduler_loop(db_path, cfg, ctx, shared, rx));
    });
}

/// 调度循环（原 daemon::run 的等价物，改由 GUI 后台线程托管，ADR-14）。
async fn scheduler_loop(
    db_path: PathBuf,
    cfg: Config,
    ctx: egui::Context,
    shared: Arc<Shared>,
    mut rx: mpsc::UnboundedReceiver<Cmd>,
) {
    let db = match Db::open(&db_path) {
        Ok(d) => d,
        Err(e) => {
            tracing::error!("调度线程打开库失败: {e}");
            return;
        }
    };
    let client = match fetch::client() {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("调度线程建 client 失败: {e}");
            return;
        }
    };
    tracing::info!("GUI 后台调度线程启动");
    loop {
        let now = Utc::now().timestamp();
        match db.due_feeds(now) {
            Ok(due) if !due.is_empty() => run_round(&db, &cfg, &client, due, &ctx, &shared).await,
            Ok(_) => {}
            Err(e) => tracing::warn!("查到期源失败: {e}"),
        }
        // sleep 到最近到期时间，但收到命令则提前醒来（ADR-14 手动"抓取"）。
        let now = Utc::now().timestamp();
        let next = db
            .earliest_next_fetch()
            .ok()
            .flatten()
            .unwrap_or(now + cfg.default_interval_secs);
        let secs = (next - now).clamp(5, cfg.default_interval_secs.max(5));
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(secs as u64)) => {}
            cmd = rx.recv() => match cmd {
                Some(Cmd::FetchNow) => match db.enabled_feeds() {
                    Ok(feeds) => run_round(&db, &cfg, &client, feeds, &ctx, &shared).await,
                    Err(e) => tracing::warn!("查启用源失败: {e}"),
                },
                None => {
                    tracing::info!("命令通道关闭，调度线程退出");
                    return;
                }
            }
        }
    }
}

async fn run_round(
    db: &Db,
    cfg: &Config,
    client: &reqwest::Client,
    feeds: Vec<Feed>,
    ctx: &egui::Context,
    shared: &Arc<Shared>,
) {
    shared.busy.store(true, Ordering::Relaxed);
    ctx.request_repaint();
    match daemon::fetch_feeds(db, cfg, client, feeds).await {
        Ok((nf, nn)) if nn > 0 => {
            tracing::info!("{nf} 个源共 {nn} 篇新文章");
            // 聚焦时不打扰（ADR-15）：你正看着就别弹 toast。
            if cfg.notifications && !shared.focused.load(Ordering::Relaxed) {
                notify::notify_new(nf, nn);
            }
            shared.dirty.store(true, Ordering::Relaxed);
        }
        Ok(_) => {}
        Err(e) => tracing::warn!("抓取轮次出错: {e}"),
    }
    shared.busy.store(false, Ordering::Relaxed);
    ctx.request_repaint();
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
    db: Db,
    shared: Arc<Shared>,
    cmd_tx: mpsc::UnboundedSender<Cmd>,
    _tray: TrayIcon, // 持有，drop 即销毁托盘
    tray_toggle: MenuId,
    tray_fetch: MenuId,
    tray_quit: MenuId,
    feeds: Vec<(Feed, i64)>,
    articles: Vec<Article>,
    /// 中栏当前展示普通订阅文章，还是统一的文章收藏库。
    content_mode: ContentMode,
    /// 收藏库中的本地网页快照 id。用集合缓存，避免 UI 每帧逐条查库。
    web_clipping_ids: HashSet<i64>,
    saved_article_count: usize,
    // 选中态存 id 而非下标，后台刷新重排后也不跳（ADR-14）。
    sel_feed_id: Option<i64>,
    sel_article_id: Option<i64>,
    hidden: bool,
    quitting: bool,
    body_article_id: Option<i64>,
    image_cache: HashMap<String, ImageState>,
    image_job_tx: std_mpsc::Sender<String>,
    image_event_rx: std_mpsc::Receiver<ImageEvent>,
    /// 正在编辑的想法窗口。
    comment_dialog: Option<CommentDialog>,
    /// 给用户的轻量操作反馈。
    selection_notice: Option<(String, Instant)>,
    /// 全局“摘录与想法”资料库是否打开。
    show_saved_library: bool,
    /// 左栏入口显示的有效摘录数量；写入或删除后立即刷新。
    saved_selection_count: usize,
    /// 全局归档列表是否打开。
    show_archive_library: bool,
    /// 已归档文章数量。
    archived_article_count: usize,
    /// 快捷操作浮层中当前等待处理的选区。
    selection_popup: Option<SelectionPopup>,
    /// 每次新选区使用不同的浮层 id，避免旧浮层的点击关闭事件误伤新浮层。
    selection_popup_generation: u64,
    /// 跨标题、正文、列表和图片的文章级拖选状态。
    article_selection_drag: Option<ArticleSelectionDrag>,
    /// “导入网页”窗口及其后台抓取状态。
    web_clip_dialog: Option<WebClipDialog>,
    web_clip_event_tx: std_mpsc::Sender<WebClipEvent>,
    web_clip_event_rx: std_mpsc::Receiver<WebClipEvent>,
    web_clip_request_generation: u64,
    delete_web_clip_dialog: Option<DeleteWebClipDialog>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ContentMode {
    Feed,
    Saved,
}

#[derive(Debug, Default)]
struct WebClipDialog {
    /// 可以是 http(s) 地址，也可以是用户粘贴的完整 HTML / HTML 片段。
    source: String,
    title: String,
    /// 粘贴 HTML 时用于解析相对链接；网址抓取模式会自动使用最终地址。
    base_url: String,
    fetching: bool,
    active_request: Option<u64>,
    error: Option<String>,
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
}

struct CommentDialog {
    quote: SelectedQuote,
    draft: String,
}

#[derive(Debug, Clone)]
struct SelectionPopup {
    quote: SelectedQuote,
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

impl GuiApp {
    fn new(cc: &eframe::CreationContext, paths: &Paths, cfg: Config) -> Result<Self> {
        let db = Db::open(&paths.db_file)?;
        let shared = Arc::new(Shared {
            focused: AtomicBool::new(true),
            busy: AtomicBool::new(false),
            dirty: AtomicBool::new(false),
        });
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        spawn_scheduler(
            paths.db_file.clone(),
            cfg,
            cc.egui_ctx.clone(),
            shared.clone(),
            cmd_rx,
        );

        let (tray, tray_toggle, tray_fetch, tray_quit) = build_tray()?;
        let (image_job_tx, image_job_rx) = std_mpsc::channel();
        let (image_event_tx, image_event_rx) = std_mpsc::channel();
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
        spawn_image_workers(image_client, image_job_rx, image_event_tx);
        let mut app = GuiApp {
            db,
            shared,
            cmd_tx,
            _tray: tray,
            tray_toggle,
            tray_fetch,
            tray_quit,
            feeds: Vec::new(),
            articles: Vec::new(),
            content_mode: ContentMode::Feed,
            web_clipping_ids: HashSet::new(),
            saved_article_count: 0,
            sel_feed_id: None,
            sel_article_id: None,
            hidden: false,
            quitting: false,
            body_article_id: None,
            image_cache: HashMap::new(),
            image_job_tx,
            image_event_rx,
            comment_dialog: None,
            selection_notice: None,
            show_saved_library: false,
            saved_selection_count: 0,
            show_archive_library: false,
            archived_article_count: 0,
            selection_popup: None,
            selection_popup_generation: 0,
            article_selection_drag: None,
            web_clip_dialog: None,
            web_clip_event_tx,
            web_clip_event_rx,
            web_clip_request_generation: 0,
            delete_web_clip_dialog: None,
        };
        app.reload();
        app.refresh_saved_selection_count();
        app.refresh_archived_article_count();
        app.refresh_saved_article_count();
        Ok(app)
    }

    fn refresh_saved_selection_count(&mut self) {
        self.saved_selection_count = self.db.saved_selection_count().unwrap_or_default();
    }

    fn refresh_archived_article_count(&mut self) {
        self.archived_article_count = self.db.archived_article_count().unwrap_or_default();
    }

    fn refresh_saved_article_count(&mut self) {
        self.saved_article_count = self.db.saved_article_count().unwrap_or_default();
    }

    fn reload(&mut self) {
        self.feeds = self.db.feeds_with_unread().unwrap_or_default();
        if self
            .sel_feed_id
            .map_or(true, |id| !self.feeds.iter().any(|(f, _)| f.id == id))
        {
            self.sel_feed_id = self.feeds.first().map(|(f, _)| f.id);
        }
        self.load_articles();
    }

    fn load_articles(&mut self) {
        self.articles = match self.content_mode {
            ContentMode::Saved => self.db.saved_articles().unwrap_or_default(),
            ContentMode::Feed => match self.sel_feed_id {
                Some(id) => self.db.articles_for_feed(id).unwrap_or_default(),
                None => Vec::new(),
            },
        };
        self.web_clipping_ids = self
            .db
            .web_clippings()
            .unwrap_or_default()
            .into_iter()
            .map(|article| article.id)
            .collect();
        if self
            .sel_article_id
            .map_or(false, |id| !self.articles.iter().any(|a| a.id == id))
        {
            self.sel_article_id = None;
        }
    }

    fn select_feed(&mut self, id: i64) {
        if self.content_mode != ContentMode::Feed || self.sel_feed_id != Some(id) {
            self.content_mode = ContentMode::Feed;
            self.sel_feed_id = Some(id);
            self.sel_article_id = None;
            self.body_article_id = None;
            self.comment_dialog = None;
            self.selection_popup = None;
            self.article_selection_drag = None;
            self.load_articles();
        }
    }

    fn select_saved_articles(&mut self) {
        if self.content_mode != ContentMode::Saved {
            self.content_mode = ContentMode::Saved;
            self.sel_article_id = None;
            self.body_article_id = None;
            self.comment_dialog = None;
            self.selection_popup = None;
            self.article_selection_drag = None;
        }
        self.load_articles();
        self.refresh_saved_article_count();
    }

    /// 点开即已读（ADR-16），未读数同步减一。
    fn select_article(&mut self, id: i64) {
        if self.sel_article_id != Some(id) {
            self.body_article_id = None;
            self.comment_dialog = None;
            self.selection_popup = None;
            self.article_selection_drag = None;
        }
        self.sel_article_id = Some(id);
        let mut newly_read_feed = None;
        if let Some(a) = self.articles.iter_mut().find(|a| a.id == id) {
            if !a.is_read {
                let _ = self.db.mark_read(id);
                a.is_read = true;
                newly_read_feed = Some(a.feed_id);
            }
        }
        if let Some(fid) = newly_read_feed {
            if let Some((_, u)) = self.feeds.iter_mut().find(|(f, _)| f.id == fid) {
                *u = (*u - 1).max(0);
            }
        }
    }

    fn mark_unread(&mut self, id: i64) {
        let mut changed_feed = None;
        if let Some(a) = self.articles.iter_mut().find(|a| a.id == id) {
            if a.is_read {
                let _ = self.db.mark_unread(id);
                a.is_read = false;
                changed_feed = Some(a.feed_id);
            }
        }
        if let Some(fid) = changed_feed {
            if let Some((_, u)) = self.feeds.iter_mut().find(|(f, _)| f.id == fid) {
                *u += 1;
            }
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

        match self.db.toggle_star(id) {
            Ok(()) => {
                if self.content_mode == ContentMode::Saved && was_starred {
                    // Removing the open article from the saved view must not
                    // leave its body or selection overlays visible after the
                    // middle-column row disappears.
                    if self.sel_article_id == Some(id) {
                        self.sel_article_id = None;
                        self.body_article_id = None;
                        self.comment_dialog = None;
                        self.selection_popup = None;
                        self.article_selection_drag = None;
                    }
                    self.load_articles();
                } else if let Some(article) =
                    self.articles.iter_mut().find(|article| article.id == id)
                {
                    article.starred = !was_starred;
                }
                self.refresh_saved_article_count();
                self.selection_notice = Some((
                    if was_starred {
                        "已取消文章收藏".to_owned()
                    } else {
                        "已收藏文章，可在左侧「文章收藏」查看".to_owned()
                    },
                    Instant::now(),
                ));
            }
            Err(error) => {
                let action = if was_starred {
                    "取消文章收藏"
                } else {
                    "收藏文章"
                };
                self.selection_notice = Some((format!("{action}失败：{error}"), Instant::now()));
            }
        }
    }

    fn archive_article(&mut self, id: i64) {
        match self.db.set_article_archived(id, true) {
            Ok(changed) if changed > 0 => {
                let was_unread = self
                    .articles
                    .iter()
                    .find(|article| article.id == id)
                    .is_some_and(|article| !article.is_read);
                self.articles.retain(|article| article.id != id);
                if self.sel_article_id == Some(id) {
                    self.sel_article_id = None;
                    self.body_article_id = None;
                    self.comment_dialog = None;
                    self.selection_popup = None;
                    self.article_selection_drag = None;
                }
                if was_unread {
                    if let Some((_, unread)) = self
                        .feeds
                        .iter_mut()
                        .find(|(feed, _)| Some(feed.id) == self.sel_feed_id)
                    {
                        *unread = (*unread - 1).max(0);
                    }
                }
                self.refresh_archived_article_count();
                self.refresh_saved_article_count();
                self.selection_notice = Some((
                    "文章已归档，后续刷新不会重新出现".to_owned(),
                    Instant::now(),
                ));
            }
            Ok(_) => {}
            Err(error) => {
                self.selection_notice = Some((format!("归档失败：{error}"), Instant::now()));
            }
        }
    }

    fn selected_article(&self) -> Option<&Article> {
        let id = self.sel_article_id?;
        self.articles.iter().find(|a| a.id == id)
    }

    fn open_web_clip_dialog(&mut self) {
        self.web_clip_dialog
            .get_or_insert_with(WebClipDialog::default);
        self.selection_popup = None;
    }

    fn begin_web_clip_import(&mut self, ctx: &egui::Context) {
        let Some(dialog) = self.web_clip_dialog.as_mut() else {
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
            self.web_clip_request_generation = self.web_clip_request_generation.wrapping_add(1);
            let request_id = self.web_clip_request_generation;
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
            match event {
                WebClipEvent::Complete { request_id, result } => {
                    let Some(dialog) = self.web_clip_dialog.as_mut() else {
                        continue;
                    };
                    if dialog.active_request != Some(request_id) {
                        continue;
                    }
                    dialog.fetching = false;
                    dialog.active_request = None;
                    match result {
                        Ok(fetched) => {
                            let snapshot = text::prepare_html_snapshot(&fetched.html);
                            if snapshot.content.trim().is_empty() {
                                dialog.error =
                                    Some("网页抓取成功，但没有识别到可阅读正文".to_owned());
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
                            let content =
                                with_html_base(&snapshot.content, effective_base.as_deref());
                            self.finish_web_clip_save(
                                Some(&fetched.original_url),
                                &title,
                                &content,
                            );
                        }
                        Err(error) => {
                            dialog.error = Some(format!("抓取失败：{error}"));
                        }
                    }
                }
            }
            ctx.request_repaint();
        }
    }

    fn finish_web_clip_save(&mut self, source_url: Option<&str>, title: &str, content: &str) {
        match self
            .db
            .save_web_clipping(source_url, Some(title), content, Utc::now().timestamp())
        {
            Ok(article_id) => {
                self.web_clip_dialog = None;
                self.content_mode = ContentMode::Saved;
                self.load_articles();
                self.refresh_saved_article_count();
                self.select_article(article_id);
                self.selection_notice = Some((
                    "正文快照已保存到本机；网页图片仍需联网加载".to_owned(),
                    Instant::now(),
                ));
            }
            Err(error) => {
                if let Some(dialog) = self.web_clip_dialog.as_mut() {
                    dialog.error = Some(format!("保存失败：{error}"));
                } else {
                    self.selection_notice =
                        Some((format!("网页保存失败：{error}"), Instant::now()));
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
            self.delete_web_clip_dialog = Some(DeleteWebClipDialog {
                article_id: id,
                title,
            });
        } else {
            self.toggle_star(id);
        }
    }

    fn save_favorite_quote(&mut self, quote: SelectedQuote) {
        let result = self.db.add_favorite_selection(
            quote.article_id,
            &quote.text,
            quote.start_offset,
            quote.end_offset,
            Utc::now().timestamp(),
        );
        match result {
            Ok(_) => {
                self.refresh_saved_selection_count();
                self.selection_notice = Some((
                    "已摘录，可在左侧「摘录与想法」查看".to_owned(),
                    Instant::now(),
                ));
            }
            Err(error) => {
                self.selection_notice = Some((format!("摘录失败：{error}"), Instant::now()));
            }
        }
    }

    fn begin_comment(&mut self, quote: SelectedQuote) {
        self.comment_dialog = Some(CommentDialog {
            quote,
            draft: String::new(),
        });
    }

    fn submit_comment(&mut self) {
        let Some(dialog) = self.comment_dialog.take() else {
            return;
        };
        if dialog.draft.trim().is_empty() {
            self.selection_notice = Some(("想法内容不能为空".to_owned(), Instant::now()));
            self.comment_dialog = Some(dialog);
            return;
        }
        let quote = dialog.quote;
        let result = self.db.add_comment(
            quote.article_id,
            &quote.text,
            quote.start_offset,
            quote.end_offset,
            &dialog.draft,
            Utc::now().timestamp(),
        );
        match result {
            Ok(_) => {
                self.refresh_saved_selection_count();
                self.selection_notice = Some((
                    "想法已保存，可在左侧「摘录与想法」查看".to_owned(),
                    Instant::now(),
                ));
            }
            Err(error) => {
                self.selection_notice = Some((format!("想法保存失败：{error}"), Instant::now()));
                self.comment_dialog = Some(CommentDialog {
                    quote,
                    draft: dialog.draft,
                });
            }
        }
    }

    fn show_comment_dialog(&mut self, ctx: &egui::Context) {
        let Some(dialog) = self.comment_dialog.as_mut() else {
            return;
        };
        let mut open = true;
        let mut submit = false;
        let mut cancel = false;
        egui::Window::new("写想法")
            .open(&mut open)
            .collapsible(false)
            .resizable(true)
            .default_width(460.0)
            .show(ctx, |ui| {
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
                ui.add(
                    egui::TextEdit::multiline(&mut dialog.draft)
                        .desired_rows(4)
                        .desired_width(440.0)
                        .hint_text("写下你的想法…"),
                );
                ui.horizontal(|ui| {
                    if ui.button("保存想法").clicked() {
                        submit = true;
                    }
                    if ui.button("取消").clicked() {
                        cancel = true;
                    }
                });
            });
        if cancel || !open {
            self.comment_dialog = None;
        } else if submit {
            self.submit_comment();
        }
    }

    fn show_web_clip_dialog(&mut self, ctx: &egui::Context) {
        let Some(dialog) = self.web_clip_dialog.as_mut() else {
            return;
        };
        let theme = ReaderTheme::sspai();
        let mut open = true;
        let mut import = false;
        let mut cancel = false;
        egui::Window::new("保存网页")
            .id(egui::Id::new("web-clipping-import"))
            .open(&mut open)
            .collapsible(false)
            .resizable(true)
            .default_width(620.0)
            .min_width(480.0)
            .frame(
                egui::Frame::new()
                    .fill(theme.canvas)
                    .stroke(egui::Stroke::new(1.0, theme.border))
                    .corner_radius(egui::CornerRadius::same(9))
                    .inner_margin(egui::Margin::same(18)),
            )
            .show(ctx, |ui| {
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

        if cancel || !open {
            self.web_clip_dialog = None;
        } else if import {
            self.begin_web_clip_import(ctx);
        }
    }

    fn show_delete_web_clip_dialog(&mut self, ctx: &egui::Context) {
        let Some(dialog) = self.delete_web_clip_dialog.clone() else {
            return;
        };
        let mut open = true;
        let mut confirm = false;
        let mut cancel = false;
        egui::Window::new("删除本地网页")
            .id(egui::Id::new("delete-web-clipping-confirm"))
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .default_width(420.0)
            .show(ctx, |ui| {
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
        if confirm {
            match self.db.delete_web_clipping(dialog.article_id) {
                Ok(changed) if changed > 0 => {
                    if self.sel_article_id == Some(dialog.article_id) {
                        self.sel_article_id = None;
                        self.body_article_id = None;
                    }
                    self.load_articles();
                    self.refresh_saved_article_count();
                    self.refresh_saved_selection_count();
                    self.selection_notice = Some(("本地网页已永久删除".to_owned(), Instant::now()));
                }
                Ok(_) => {
                    self.selection_notice =
                        Some(("网页不存在或已经删除".to_owned(), Instant::now()));
                }
                Err(error) => {
                    self.selection_notice = Some((format!("删除失败：{error}"), Instant::now()));
                }
            }
            self.delete_web_clip_dialog = None;
        } else if cancel || !open {
            self.delete_web_clip_dialog = None;
        }
    }

    fn show_saved_library_window(&mut self, ctx: &egui::Context) {
        if !self.show_saved_library {
            return;
        }

        let theme = ReaderTheme::sspai();
        let (rows, load_error) = match self.db.saved_selections() {
            Ok(rows) => (rows, None),
            Err(error) => (Vec::new(), Some(error.to_string())),
        };
        let mut open = true;
        let mut open_article = None;
        let mut delete_selection = None;

        egui::Window::new(format!("摘录与想法 · {}", rows.len()))
            .id(egui::Id::new("saved-selections-library"))
            .open(&mut open)
            .collapsible(false)
            .resizable(true)
            .default_size(egui::vec2(680.0, 600.0))
            .min_width(520.0)
            .frame(
                egui::Frame::new()
                    .fill(theme.canvas)
                    .stroke(egui::Stroke::new(1.0, theme.border))
                    .corner_radius(egui::CornerRadius::same(9))
                    .inner_margin(egui::Margin::same(12))
                    .shadow(egui::Shadow {
                        offset: [0, 5],
                        blur: 18,
                        spread: 0,
                        color: egui::Color32::from_black_alpha(45),
                    }),
            )
            .show(ctx, |ui| {
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
                                                    open_article =
                                                        Some((*feed_id, selection.article_id));
                                                }
                                            },
                                        );
                                    });
                                });
                            ui.add_space(10.0);
                        }
                    });
            });

        self.show_saved_library = open;
        if let Some(selection_id) = delete_selection {
            match self.db.delete_selection(selection_id) {
                Ok(_) => {
                    self.refresh_saved_selection_count();
                    self.selection_notice = Some(("摘录已删除".to_owned(), Instant::now()));
                }
                Err(error) => {
                    self.selection_notice = Some((format!("删除失败：{error}"), Instant::now()));
                }
            }
        }
        if let Some((feed_id, article_id)) = open_article {
            if self.web_clipping_ids.contains(&article_id)
                || self.db.is_web_clipping(article_id).unwrap_or(false)
            {
                self.select_saved_articles();
            } else {
                self.select_feed(feed_id);
            }
            self.select_article(article_id);
            self.show_saved_library = false;
            self.selection_notice = Some(("已打开原文章".to_owned(), Instant::now()));
        }
    }

    fn show_archive_library_window(&mut self, ctx: &egui::Context) {
        if !self.show_archive_library {
            return;
        }

        let theme = ReaderTheme::sspai();
        let (articles, load_error) = match self.db.archived_articles() {
            Ok(articles) => (articles, None),
            Err(error) => (Vec::new(), Some(error.to_string())),
        };
        let mut open = true;
        let mut restore_article = None;

        egui::Window::new(format!("已归档文章 · {}", articles.len()))
            .id(egui::Id::new("archived-articles-library"))
            .open(&mut open)
            .collapsible(false)
            .resizable(true)
            .default_size(egui::vec2(650.0, 560.0))
            .min_width(500.0)
            .frame(
                egui::Frame::new()
                    .fill(theme.canvas)
                    .stroke(egui::Stroke::new(1.0, theme.border))
                    .corner_radius(egui::CornerRadius::same(9))
                    .inner_margin(egui::Margin::same(12))
                    .shadow(egui::Shadow {
                        offset: [0, 5],
                        blur: 18,
                        spread: 0,
                        color: egui::Color32::from_black_alpha(45),
                    }),
            )
            .show(ctx, |ui| {
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

                if let Some(error) = &load_error {
                    ui.colored_label(ui.visuals().error_fg_color, format!("读取失败：{error}"));
                    return;
                }
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

        self.show_archive_library = open;
        if let Some((feed_id, article_id)) = restore_article {
            match self.db.set_article_archived(article_id, false) {
                Ok(changed) if changed > 0 => {
                    self.refresh_archived_article_count();
                    self.refresh_saved_article_count();
                    // Re-read feed unread counts before opening the restored
                    // article. `select_article` will mark it read, so this
                    // keeps the badge exact even when the feed already had
                    // other unread entries.
                    self.reload();
                    self.select_feed(feed_id);
                    self.select_article(article_id);
                    self.show_archive_library = false;
                    self.selection_notice = Some(("文章已恢复并打开".to_owned(), Instant::now()));
                }
                Ok(_) => {}
                Err(error) => {
                    self.selection_notice = Some((format!("恢复失败：{error}"), Instant::now()));
                }
            }
        }
    }

    fn show_selection_notice(&mut self, ctx: &egui::Context) {
        let Some((message, created)) = self.selection_notice.as_ref() else {
            return;
        };
        if created.elapsed() > Duration::from_secs(4) {
            self.selection_notice = None;
            return;
        }
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
                        ui.horizontal(|ui| {
                            ui.label(
                                egui::RichText::new(if failed { "!" } else { "✓" })
                                    .size(16.0)
                                    .color(if failed { theme.accent } else { theme.link })
                                    .family(egui::FontFamily::Name("cjk-bold".into())),
                            );
                            ui.label(egui::RichText::new(message).size(13.0).color(theme.text));
                        });
                    });
            });
        ctx.request_repaint_after(Duration::from_millis(100));
    }

    fn show_selection_popup(&mut self, ctx: &egui::Context) {
        let Some(popup) = self.selection_popup.clone() else {
            return;
        };
        let quote = popup.quote;
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
            self.selection_popup = None;
        }
        if let Some(action) = action {
            match action {
                SelectionAction::Copy => {
                    ctx.copy_text(quote.text);
                    self.selection_notice = Some(("已复制选中的文字".to_owned(), Instant::now()));
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

    fn handle_tray_events(&mut self, ctx: &egui::Context) {
        while let Ok(ev) = MenuEvent::receiver().try_recv() {
            if ev.id == self.tray_toggle {
                self.hidden = !self.hidden;
                ctx.send_viewport_cmd(ViewportCommand::Visible(!self.hidden));
                if !self.hidden {
                    ctx.send_viewport_cmd(ViewportCommand::Focus);
                }
            } else if ev.id == self.tray_fetch {
                self.shared.busy.store(true, Ordering::Relaxed);
                let _ = self.cmd_tx.send(Cmd::FetchNow);
            } else if ev.id == self.tray_quit {
                self.quitting = true;
                ctx.send_viewport_cmd(ViewportCommand::Close);
            }
        }
    }
}

impl eframe::App for GuiApp {
    // eframe 0.35：App 入口是 ui(&mut Ui)，panel 在根 Ui 内 show。
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.shared.focused.store(
            ctx.input(|i| i.viewport().focused.unwrap_or(true)),
            Ordering::Relaxed,
        );
        self.receive_images(&ctx);
        self.receive_web_clip_events(&ctx);
        self.handle_tray_events(&ctx);

        // 关窗 → 隐藏到托盘（除非托盘"退出"已置 quitting，ADR-15）。
        if !self.quitting && ctx.input(|i| i.viewport().close_requested()) {
            ctx.send_viewport_cmd(ViewportCommand::CancelClose);
            ctx.send_viewport_cmd(ViewportCommand::Visible(false));
            self.hidden = true;
        }
        // 后台抓到新文章 → 重读库（自动刷新，选中态按 id 保住，ADR-14）。
        if self.shared.dirty.swap(false, Ordering::Relaxed) {
            self.reload();
        }
        // 心跳：即便隐藏/空闲也定期醒来轮询托盘事件与 dirty。
        // ponytail: 250ms 轮询够跟手；想省这点空转再上 MenuEvent::set_event_handler + proxy。
        ctx.request_repaint_after(Duration::from_millis(250));

        let busy = self.shared.busy.load(Ordering::Relaxed);
        let theme = ReaderTheme::sspai();

        // 源栏
        let mut feed_click = None;
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
                        {
                            self.shared.busy.store(true, Ordering::Relaxed); // 即时反馈
                            let _ = self.cmd_tx.send(Cmd::FetchNow);
                        }
                    });
                });
                ui.separator();
                ui.add_space(4.0);
                let saved_articles_response = ui.add(
                    egui::Button::new(
                        egui::RichText::new(format!("★ 文章收藏  {}", self.saved_article_count))
                            .size(13.0)
                            .color(if self.content_mode == ContentMode::Saved {
                                theme.text
                            } else {
                                theme.accent
                            }),
                    )
                    .fill(if self.content_mode == ContentMode::Saved {
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
                    self.show_saved_library = false;
                    self.show_archive_library = false;
                    self.selection_popup = None;
                }
                ui.add_space(4.0);
                let library_response = ui.add(
                    egui::Button::new(
                        egui::RichText::new(format!(
                            "✦ 摘录与想法  {}",
                            self.saved_selection_count
                        ))
                        .size(13.0)
                        .color(if self.show_saved_library {
                            theme.text
                        } else {
                            theme.accent
                        }),
                    )
                    .fill(if self.show_saved_library {
                        theme.selected_bg
                    } else {
                        egui::Color32::TRANSPARENT
                    })
                    .stroke(egui::Stroke::NONE)
                    .corner_radius(egui::CornerRadius::same(4))
                    .min_size(egui::vec2(ui.available_width(), 34.0)),
                );
                if library_response.clicked() {
                    self.show_saved_library = true;
                    self.show_archive_library = false;
                    self.selection_popup = None;
                }
                ui.add_space(4.0);
                let archive_response = ui.add(
                    egui::Button::new(
                        egui::RichText::new(format!("▣ 已归档  {}", self.archived_article_count))
                            .size(13.0)
                            .color(if self.show_archive_library {
                                theme.text
                            } else {
                                theme.muted
                            }),
                    )
                    .fill(if self.show_archive_library {
                        theme.selected_bg
                    } else {
                        egui::Color32::TRANSPARENT
                    })
                    .stroke(egui::Stroke::NONE)
                    .corner_radius(egui::CornerRadius::same(4))
                    .min_size(egui::vec2(ui.available_width(), 34.0)),
                );
                if archive_response.clicked() {
                    self.show_archive_library = true;
                    self.show_saved_library = false;
                    self.selection_popup = None;
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
                            let sel = self.content_mode == ContentMode::Feed
                                && self.sel_feed_id == Some(fd.id);
                            let fill = if sel {
                                theme.selected_bg
                            } else {
                                egui::Color32::TRANSPARENT
                            };
                            let response = ui.add(
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

        // 文章栏
        let mut open_article = None;
        let mut unread_article = None;
        let mut star_article = None;
        let mut archive_article = None;
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
                        egui::RichText::new(if self.content_mode == ContentMode::Saved {
                            "文章收藏"
                        } else {
                            "文章"
                        })
                        .size(18.0)
                        .family(egui::FontFamily::Name("cjk-bold".into())),
                    );
                    ui.label(
                        egui::RichText::new(format!("{} 篇", self.articles.len()))
                            .size(12.0)
                            .color(theme.muted),
                    );
                    if self.content_mode == ContentMode::Saved {
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
                });
                ui.separator();
                ui.add_space(4.0);
                if self.content_mode == ContentMode::Saved && self.articles.is_empty() {
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
                            let resp = ui.add(
                                egui::Button::new(
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
                                .min_size(egui::vec2(ui.available_width(), 42.0)),
                            );
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
                                open_article = Some(a.id);
                            }
                            resp.context_menu(|ui| {
                                if self.content_mode == ContentMode::Saved {
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
        if let Some(id) = open_article {
            self.select_article(id);
        }
        if let Some(id) = unread_article {
            self.mark_unread(id);
        }
        if let Some(id) = star_article {
            if self.content_mode == ContentMode::Saved {
                self.remove_saved_article(id);
            } else {
                self.toggle_star(id);
            }
        }
        if let Some(id) = archive_article {
            self.archive_article(id);
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
                let mut selection_frame = ArticleSelectionFrame::default();
                let mut body_scroll = egui::ScrollArea::vertical()
                    .id_salt(("article-body-v2", article_id))
                    .hscroll(false);
                if reset_body_scroll {
                    body_scroll = body_scroll.scroll_offset(egui::Vec2::ZERO);
                }
                let body_scroll_output = body_scroll.show_viewport(ui, |ui, viewport| {
                    // Keep the scroll viewport full width while centering a
                    // readable 820 px column on the white reading canvas.
                    ui.painter().rect_filled(ui.max_rect(), 0.0, theme.canvas);
                    let available = ui.available_width();
                    let content_width = available.min(ARTICLE_MAX_WIDTH).max(0.0);
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
                                    if let Some(u) = &url {
                                        if ui
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
                                });
                                ui.separator();
                                ui.add_space(18.0);
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
                                        Block::Image(uri) => {
                                            article_image(
                                                ui,
                                                &viewport,
                                                uri,
                                                &mut self.image_cache,
                                                &self.image_job_tx,
                                            );
                                            index += 1;
                                        }
                                        Block::ListItemStart { depth } => {
                                            let start = index;
                                            let item_depth = *depth;
                                            let mut list_text = String::from("▪ ");
                                            let mut list_strong_ranges = Vec::new();
                                            let mut list_link_ranges = Vec::new();
                                            let mut previous_was_strong = false;
                                            let mut previous_was_link = false;
                                            let mut previous_link_had_space_after = false;
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
                                                let value = match block {
                                                    Block::Text(text)
                                                    | Block::Strong(text)
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
                                                        list_text.push_str(body_block_separator(
                                                            &list_text,
                                                            value,
                                                            previous_was_strong,
                                                            previous_was_link,
                                                            matches!(block, Block::Strong(_)),
                                                            matches!(block, Block::Link { .. }),
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
                                                ui.add_space(22.0);
                                                ui.vertical(|ui| {
                                                    ui.set_width(ui.available_width());
                                                    selectable_text_block_with_style(
                                                        ui,
                                                        article_id,
                                                        start,
                                                        &list_text,
                                                        &list_strong_ranges,
                                                        &list_link_ranges,
                                                        ArticleTextStyle::List,
                                                        &mut selection_frame,
                                                    );
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
                                            let mut link_ranges = Vec::new();
                                            let mut previous_was_strong = false;
                                            let mut previous_was_link = false;
                                            let mut previous_link_had_space_after = false;
                                            while index < blocks.len() {
                                                let block = &blocks[index];
                                                if matches!(
                                                    block,
                                                    Block::Image(_)
                                                        | Block::Heading(_)
                                                        | Block::HeadingLink { .. }
                                                        | Block::Quote(_)
                                                        | Block::Code(_)
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
                                                    | Block::Link { text, .. } => text,
                                                    Block::Image(_)
                                                    | Block::Heading(_)
                                                    | Block::HeadingLink { .. }
                                                    | Block::Quote(_)
                                                    | Block::Code(_)
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
                                                        run.push_str(body_block_separator(
                                                            &run,
                                                            value,
                                                            previous_was_strong,
                                                            previous_was_link,
                                                            matches!(block, Block::Strong(_)),
                                                            matches!(block, Block::Link { .. }),
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
                                                if is_link {
                                                    if let Block::Link {
                                                        url, link_start, ..
                                                    } = block
                                                    {
                                                        link_ranges.push(ArticleLinkRange {
                                                            range: value_start + *link_start
                                                                ..run.len(),
                                                            url: url.clone(),
                                                        });
                                                    }
                                                }
                                                previous_was_strong = is_strong;
                                                previous_was_link = is_link;
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
                                                selectable_text_block_with_style(
                                                    ui,
                                                    article_id,
                                                    start,
                                                    &run,
                                                    &strong_ranges,
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
                let selection_result =
                    self.update_article_selection(&ctx, article_id, &selection_frame);
                let selection_drag_started = selection_result.drag_started;
                let selection_popup_request = selection_result.popup_request;
                let scroll_offset = body_scroll_output.state.offset;
                let popup_moved_away_from_selection = self
                    .selection_popup
                    .as_ref()
                    .filter(|popup| popup.quote.article_id == article_id)
                    .is_some_and(|popup| (popup.scroll_offset - scroll_offset).length_sq() > 0.25);
                let popup_layout_changed = self
                    .selection_popup
                    .as_ref()
                    .filter(|popup| popup.quote.article_id == article_id)
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
                    self.selection_popup = None;
                }
                if let Some(request) = selection_popup_request {
                    self.selection_popup_generation =
                        self.selection_popup_generation.wrapping_add(1);
                    self.selection_popup = Some(SelectionPopup {
                        quote: request.quote,
                        anchor_rect: request.anchor_rect,
                        source_layer: request.source_layer,
                        scroll_offset,
                        viewport_rect: body_scroll_output.inner_rect,
                        generation: self.selection_popup_generation,
                    });
                }
                if let Some(selection_id) = delete_selection {
                    match self.db.delete_selection(selection_id) {
                        Ok(_) => {
                            self.refresh_saved_selection_count();
                            self.selection_notice = Some(("已删除摘录".to_owned(), Instant::now()));
                        }
                        Err(error) => {
                            self.selection_notice =
                                Some((format!("删除失败：{error}"), Instant::now()));
                        }
                    }
                }
                if toggle_article_star {
                    self.toggle_star(article_id);
                }
            });
        self.show_saved_library_window(&ctx);
        self.show_archive_library_window(&ctx);
        self.show_web_clip_dialog(&ctx);
        self.show_delete_web_clip_dialog(&ctx);
        self.show_selection_popup(&ctx);
        self.show_comment_dialog(&ctx);
        self.show_selection_notice(&ctx);
    }
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

    Some(SelectedQuote {
        article_id,
        text: chars[lo..hi].iter().collect(),
        start_offset: Some(lo as i64),
        end_offset: Some(hi as i64),
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
            link_ranges,
            &normal,
            &heading,
        );
    } else if matches!(style, ArticleTextStyle::Title) {
        job.append(text, 0.0, heading);
    } else if matches!(style, ArticleTextStyle::Heading) {
        append_body_layout(&mut job, text, &[], link_ranges, &heading, &heading);
    } else {
        append_body_layout(
            &mut job,
            text,
            strong_ranges,
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
        link_ranges
            .iter()
            .flat_map(|link| [link.range.start, link.range.end]),
    );
    boundaries.sort_unstable();
    boundaries.dedup();

    let mut link_format = normal.clone();
    link_format.color = ReaderTheme::sspai().link;
    link_format.underline = egui::Stroke::NONE;

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
    append_body_layout(job, text, &strong_ranges, link_ranges, normal, strong);
}

fn article_image(
    ui: &mut egui::Ui,
    viewport: &egui::Rect,
    uri: &str,
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
    if matches!(cache.get(uri), Some(ImageState::Failed(_))) && response.clicked() {
        retry = true;
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

fn spawn_image_workers(
    client: reqwest::blocking::Client,
    job_rx: std_mpsc::Receiver<String>,
    event_tx: std_mpsc::Sender<ImageEvent>,
) {
    let job_rx = Arc::new(Mutex::new(job_rx));
    for worker in 0..IMAGE_WORKER_COUNT {
        let client = client.clone();
        let job_rx = job_rx.clone();
        let event_tx = event_tx.clone();
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
                    let result = download_image_with_retry(&client, &uri, &event_tx);
                    if event_tx.send(ImageEvent::Complete { uri, result }).is_err() {
                        return;
                    }
                }
            })
            .expect("failed to spawn image worker");
    }
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

#[cfg(test)]
mod tests {
    use super::{
        IMAGE_MAX_ATTEMPTS, body_block_separator, download_image_with_retry,
        image_http_status_retryable, is_punctuation_only, normalized_web_url,
        prepare_pasted_web_clip, selected_quote_from_article_text,
    };
    use std::sync::mpsc;
    use std::time::Duration;

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
