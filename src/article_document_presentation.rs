//! Article document presentation pipeline.
//!
//! This module owns the complete HTML -> prepared document -> egui rendering
//! path.  Callers provide immutable article data and render their surrounding
//! controls through one interstitial closure; they never inspect parser
//! blocks, media jobs, or selection coordinates.

mod parser;

use anyhow::Result;
use eframe::egui;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, VecDeque};
use std::error::Error as _;
use std::hash::{Hash, Hasher};
use std::io::Read as _;
use std::ops::Range;
use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use self::parser::Block;
use crate::gui_theme::ReaderTheme;
use crate::image_store::{DEFAULT_LIMIT_BYTES, ImageStore};
use crate::model::{TextAnchor, resolve_excerpt_anchor};

/// Bounded capture projection for callers that need to store readable Article
/// HTML. The semantic parser grammar remains private to this Module.
pub(crate) struct PreparedArticleHtml {
    pub(crate) title: Option<String>,
    pub(crate) content: String,
    pub(crate) base_href: Option<String>,
}

pub(crate) fn prepare_article_html(html: &str) -> PreparedArticleHtml {
    let snapshot = parser::prepare_html_snapshot(html);
    PreparedArticleHtml {
        title: snapshot.title,
        content: snapshot.content,
        base_href: snapshot.base_href,
    }
}

/// Return only the readable text projection needed by search-result snippets.
/// No parser grammar or prepared-document state crosses the Module Interface.
pub(crate) fn article_visible_text(html: &str, base_url: Option<&str>) -> String {
    parser::visible_text(html, base_url)
}

/// Return the exact character stream used by article selection anchors.
/// Parser blocks remain private; lifecycle modules only receive the canonical
/// text projection needed to validate or resolve a persisted anchor.
pub(crate) fn article_selection_text(html: &str, base_url: Option<&str>) -> String {
    parser::selection_text(html, base_url)
}

const PREPARED_DOCUMENT_CACHE_LIMIT: usize = 24;
const IMAGE_WORKER_COUNT: usize = 4;
const IMAGE_MAX_ATTEMPTS: u8 = 3;
const IMAGE_MAX_BYTES: u64 = 25 * 1024 * 1024;
const BODY_GALLEY_MAX_PARAGRAPHS: usize = 16;
const BODY_GALLEY_MAX_CHARS: usize = 1_600;

#[derive(Debug, Clone, Copy)]
pub(crate) struct ArticleDocumentSource<'a> {
    pub(crate) article_id: i64,
    pub(crate) title: &'a str,
    pub(crate) html: &'a str,
    pub(crate) base_url: Option<&'a str>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ContentFingerprint([u8; 32]);

impl ContentFingerprint {
    fn of(source: ArticleDocumentSource<'_>) -> Self {
        let mut digest = Sha256::new();
        for value in [Some(source.title), Some(source.html), source.base_url] {
            match value {
                Some(value) => {
                    digest.update([1]);
                    digest.update((value.len() as u64).to_le_bytes());
                    digest.update(value.as_bytes());
                }
                None => digest.update([0]),
            }
        }
        Self(digest.finalize().into())
    }
}

#[derive(Debug)]
struct PreparedDocument {
    fingerprint: ContentFingerprint,
    title: Arc<str>,
    blocks: Arc<[Block]>,
}

#[derive(Debug, Clone)]
pub(crate) struct RestoreSelection {
    pub(crate) selected_text: String,
    pub(crate) anchor: TextAnchor,
}

pub(crate) struct PresentRequest<'a> {
    pub(crate) source: ArticleDocumentSource<'a>,
    pub(crate) viewport: egui::Rect,
    pub(crate) restore_selection: Option<RestoreSelection>,
    pub(crate) scroll_title_into_view: bool,
}

struct RenderBlocksRequest<'a> {
    viewport: &'a egui::Rect,
    article_id: i64,
    blocks: &'a [Block],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SelectedQuote {
    pub(crate) article_id: i64,
    pub(crate) text: String,
    pub(crate) start_offset: Option<i64>,
    pub(crate) end_offset: Option<i64>,
    pub(crate) anchor_prefix: String,
    pub(crate) anchor_suffix: String,
}

pub(crate) enum PresentationIntent {
    SelectionStarted,
    SelectedQuote {
        quote: SelectedQuote,
        anchor_rect: egui::Rect,
        source_layer: egui::LayerId,
    },
    OpenUrl(String),
}

#[derive(Default)]
pub(crate) struct PresentOutcome {
    pub(crate) intents: Vec<PresentationIntent>,
    pub(crate) restored_span_top: Option<f32>,
    pub(crate) restore_failed: bool,
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
    fingerprint: ContentFingerprint,
    anchor: ArticleDocCursor,
    focus: ArticleDocCursor,
}

struct RenderedArticleSpan {
    chars: Range<usize>,
    galley: Arc<egui::Galley>,
    global_from_galley: egui::emath::TSTransform,
    global_rect: egui::Rect,
    source_layer: egui::LayerId,
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
        result: std::result::Result<Arc<[u8]>, ImageFailure>,
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

#[derive(Clone)]
struct FormulaJob {
    key: String,
    source: String,
    display: bool,
}

enum FormulaEvent {
    Complete {
        key: String,
        result: std::result::Result<Arc<[u8]>, String>,
    },
}

enum FormulaState {
    Loading,
    Ready(Arc<[u8]>),
    Failed(String),
}

pub(crate) struct ArticleDocumentPresenter {
    active: Option<Arc<PreparedDocument>>,
    prepared_by_fingerprint: HashMap<ContentFingerprint, Arc<PreparedDocument>>,
    recency: VecDeque<ContentFingerprint>,
    selection_drag: Option<ArticleSelectionDrag>,
    image_cache: HashMap<String, ImageState>,
    image_job_tx: std_mpsc::Sender<String>,
    image_event_rx: std_mpsc::Receiver<ImageEvent>,
    formula_cache: HashMap<String, FormulaState>,
    formula_job_tx: std_mpsc::Sender<FormulaJob>,
    formula_event_rx: std_mpsc::Receiver<FormulaEvent>,
    #[cfg(test)]
    last_frame_max_layout_chars: usize,
    #[cfg(test)]
    last_frame_layout_calls: usize,
}

impl ArticleDocumentPresenter {
    pub(crate) fn new(image_store: Arc<ImageStore>) -> Result<Self> {
        let (image_job_tx, image_job_rx) = std_mpsc::channel();
        let (image_event_tx, image_event_rx) = std_mpsc::channel();
        let (formula_job_tx, formula_job_rx) = std_mpsc::channel();
        let (formula_event_tx, formula_event_rx) = std_mpsc::channel();
        let image_fetch: Arc<dyn ImageFetch> = Arc::new(HttpImageFetch(image_client()?));
        spawn_image_workers(image_fetch, image_job_rx, image_event_tx, image_store);
        spawn_formula_worker(formula_job_rx, formula_event_tx);
        Ok(Self {
            active: None,
            prepared_by_fingerprint: HashMap::new(),
            recency: VecDeque::new(),
            selection_drag: None,
            image_cache: HashMap::new(),
            image_job_tx,
            image_event_rx,
            formula_cache: HashMap::new(),
            formula_job_tx,
            formula_event_rx,
            #[cfg(test)]
            last_frame_max_layout_chars: 0,
            #[cfg(test)]
            last_frame_layout_calls: 0,
        })
    }

    pub(crate) fn show<F>(
        &mut self,
        ui: &mut egui::Ui,
        request: PresentRequest<'_>,
        interstitial: F,
    ) -> PresentOutcome
    where
        F: FnOnce(&mut egui::Ui),
    {
        #[cfg(test)]
        {
            self.last_frame_max_layout_chars = 0;
            self.last_frame_layout_calls = 0;
        }
        self.receive_media(ui.ctx());
        let document = self.prepare(request.source);
        let mut frame = ArticleSelectionFrame::default();
        let (title_response, _) = selectable_text_block_with_style(
            ui,
            request.source.article_id,
            usize::MAX,
            &document.title,
            &[],
            &[],
            ArticleTextStyle::Title,
            &mut frame,
            None,
        );
        if request.scroll_title_into_view {
            title_response.scroll_to_me(Some(egui::Align::Min));
        }
        interstitial(ui);
        if document.blocks.is_empty() {
            ui.label("（此源未提供正文，点上方按钮看原文）");
        } else {
            self.render_blocks(
                ui,
                RenderBlocksRequest {
                    viewport: &request.viewport,
                    article_id: request.source.article_id,
                    blocks: &document.blocks,
                },
                &mut frame,
            );
        }

        let mut outcome = PresentOutcome::default();
        if let Some(restore) = request.restore_selection {
            if let Some(range) =
                resolve_excerpt_anchor(&frame.plain_text, &restore.selected_text, &restore.anchor)
            {
                outcome.restored_span_top = frame
                    .spans
                    .iter()
                    .find(|span| span.chars.start <= range.start && span.chars.end >= range.start)
                    .map(|span| span.global_rect.top());
            } else {
                outcome.restore_failed = true;
            }
        }
        outcome.intents = self.selection_intents(
            ui.ctx(),
            request.source.article_id,
            &document.fingerprint,
            &frame,
        );
        outcome
    }

    fn prepare(&mut self, source: ArticleDocumentSource<'_>) -> Arc<PreparedDocument> {
        let fingerprint = ContentFingerprint::of(source);
        if let Some(active) = &self.active
            && active.fingerprint == fingerprint
        {
            return active.clone();
        }
        let prepared = if let Some(prepared) = self.prepared_by_fingerprint.get(&fingerprint) {
            prepared.clone()
        } else {
            let blocks: Arc<[Block]> = if source.html.trim().is_empty() {
                Arc::from([])
            } else {
                Arc::from(parser::content_blocks(source.html, source.base_url))
            };
            let prepared = Arc::new(PreparedDocument {
                fingerprint: fingerprint.clone(),
                title: Arc::from(source.title),
                blocks,
            });
            self.prepared_by_fingerprint
                .insert(fingerprint.clone(), prepared.clone());
            prepared
        };
        self.touch(fingerprint);
        self.active = Some(prepared.clone());
        prepared
    }

    fn touch(&mut self, fingerprint: ContentFingerprint) {
        if let Some(position) = self.recency.iter().position(|value| value == &fingerprint) {
            self.recency.remove(position);
        }
        self.recency.push_back(fingerprint);
        while self.recency.len() > PREPARED_DOCUMENT_CACHE_LIMIT {
            if let Some(expired) = self.recency.pop_front() {
                self.prepared_by_fingerprint.remove(&expired);
            }
        }
    }

    fn receive_media(&mut self, ctx: &egui::Context) {
        while let Ok(event) = self.image_event_rx.try_recv() {
            match event {
                ImageEvent::Progress { uri, attempt } => {
                    if let Some(ImageState::Loading { attempt: value, .. }) =
                        self.image_cache.get_mut(&uri)
                    {
                        *value = attempt;
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
}

impl ArticleDocumentPresenter {
    fn render_blocks(
        &mut self,
        ui: &mut egui::Ui,
        request: RenderBlocksRequest<'_>,
        selection: &mut ArticleSelectionFrame,
    ) {
        let RenderBlocksRequest {
            viewport,
            article_id,
            blocks,
        } = request;
        let theme = ReaderTheme::sspai();
        let mut index = 0;
        while index < blocks.len() {
            match &blocks[index] {
                Block::Quote(text) => {
                    egui::Frame::new()
                        .inner_margin(egui::Margin::symmetric(38, 24))
                        .show(ui, |ui| {
                            selectable_text_block_with_style(
                                ui,
                                article_id,
                                index,
                                text,
                                &[],
                                &[],
                                ArticleTextStyle::Quote,
                                selection,
                                None,
                            );
                        });
                    ui.add_space(15.0);
                    index += 1;
                }
                Block::Code(text) | Block::CodeBlock { text, .. } => {
                    egui::Frame::new()
                        .fill(theme.code_bg)
                        .corner_radius(egui::CornerRadius::same(4))
                        .inner_margin(egui::Margin::symmetric(20, 10))
                        .show(ui, |ui| {
                            selectable_text_block_with_style(
                                ui,
                                article_id,
                                index,
                                text,
                                &[],
                                &[],
                                ArticleTextStyle::Code,
                                selection,
                                None,
                            );
                        });
                    ui.add_space(25.0);
                    index += 1;
                }
                Block::Image(uri) => {
                    self.article_image(ui, viewport, uri, None);
                    index += 1;
                }
                Block::LinkedImage { uri, url, alt } => {
                    if let Some(open) = self.article_image(ui, viewport, uri, Some(url)) {
                        ui.ctx().data_mut(|data| {
                            data.insert_temp(
                                egui::Id::new(("article-document-open-url", article_id)),
                                open,
                            );
                        });
                    }
                    if let Some(alt) = alt {
                        ui.label(egui::RichText::new(alt).size(15.0).color(theme.muted));
                        ui.add_space(8.0);
                    }
                    index += 1;
                }
                Block::Caption(caption) => {
                    ui.with_layout(egui::Layout::top_down(egui::Align::Center), |ui| {
                        ui.add(
                            egui::Label::new(
                                egui::RichText::new(caption).size(15.0).color(theme.muted),
                            )
                            .selectable(true)
                            .wrap(),
                        );
                    });
                    ui.add_space(16.0);
                    index += 1;
                }
                Block::DefinitionList(items) => {
                    egui::Frame::new()
                        .fill(theme.code_bg)
                        .stroke(egui::Stroke::new(1.0, theme.border))
                        .corner_radius(egui::CornerRadius::same(5))
                        .inner_margin(egui::Margin::symmetric(18, 14))
                        .show(ui, |ui| {
                            for item in items {
                                ui.strong(&item.term);
                                for definition in &item.definitions {
                                    ui.label(format!("• {definition}"));
                                }
                                ui.add_space(8.0);
                            }
                        });
                    ui.add_space(22.0);
                    index += 1;
                }
                Block::Table {
                    rows,
                    header_rows,
                    column_count,
                } => {
                    let _logical_columns = parser::table_cell_columns(rows, *column_count);
                    egui::ScrollArea::horizontal()
                        .id_salt(("article-table", article_id, index))
                        .show(ui, |ui| {
                            egui::Grid::new(("article-table-grid", article_id, index))
                                .striped(true)
                                .show(ui, |ui| {
                                    for (row_index, row) in rows.iter().enumerate() {
                                        for cell in row {
                                            if row_index < *header_rows || cell.header {
                                                ui.strong(&cell.text);
                                            } else {
                                                ui.label(&cell.text);
                                            }
                                        }
                                        ui.end_row();
                                    }
                                });
                        });
                    ui.add_space(22.0);
                    index += 1;
                }
                Block::Math { source, display } => {
                    self.formula_block(ui, source, *display);
                    ui.add_space(18.0);
                    index += 1;
                }
                Block::Heading(text) => {
                    selectable_text_block_with_style(
                        ui,
                        article_id,
                        index,
                        text,
                        &[],
                        &[],
                        ArticleTextStyle::Heading,
                        selection,
                        None,
                    );
                    ui.add_space(20.0);
                    index += 1;
                }
                Block::HeadingWithInlineCode {
                    text,
                    inline_code_ranges,
                } => {
                    let ranges = inline_code_ranges
                        .iter()
                        .map(|range| range.start..range.end)
                        .collect::<Vec<_>>();
                    selectable_text_block_with_inline_style(
                        ui,
                        article_id,
                        index,
                        text,
                        &[],
                        &ranges,
                        &[],
                        ArticleTextStyle::Heading,
                        selection,
                        None,
                    );
                    ui.add_space(20.0);
                    index += 1;
                }
                Block::HeadingLink { text, links } => {
                    let links = links
                        .iter()
                        .map(|link| ArticleLinkRange {
                            range: link.start..link.end,
                            url: link.url.clone(),
                        })
                        .collect::<Vec<_>>();
                    let open = selectable_text_block_with_style(
                        ui,
                        article_id,
                        index,
                        text,
                        &[],
                        &links,
                        ArticleTextStyle::Heading,
                        selection,
                        None,
                    )
                    .1;
                    if let Some(open) = open {
                        ui.ctx().data_mut(|data| {
                            data.insert_temp(
                                egui::Id::new(("article-document-open-url", article_id)),
                                open,
                            );
                        });
                    }
                    ui.add_space(20.0);
                    index += 1;
                }
                Block::Strong(text) if parser::is_numbered_heading(text) => {
                    selectable_text_block_with_style(
                        ui,
                        article_id,
                        index,
                        text,
                        &[],
                        &[],
                        ArticleTextStyle::Heading,
                        selection,
                        None,
                    );
                    ui.add_space(20.0);
                    index += 1;
                }
                Block::ListItemStart { depth } => {
                    let start = index;
                    let item_depth = *depth;
                    let mut list_text = String::from("▪ ");
                    let mut strong_ranges = Vec::new();
                    let mut inline_code_ranges = Vec::new();
                    let mut link_ranges = Vec::new();
                    let mut previous_was_strong = false;
                    let mut previous_was_link = false;
                    let mut previous_was_inline_code = false;
                    let mut previous_link_had_space_after = false;
                    let mut images = Vec::new();
                    index += 1;
                    while index < blocks.len() {
                        if matches!(&blocks[index], Block::ListItemEnd { depth } if *depth == item_depth)
                        {
                            index += 1;
                            break;
                        }
                        let block = &blocks[index];
                        match block {
                            Block::Image(uri) => {
                                images.push((uri.clone(), None, None));
                                index += 1;
                                continue;
                            }
                            Block::LinkedImage { uri, url, alt } => {
                                images.push((uri.clone(), Some(url.clone()), alt.clone()));
                                index += 1;
                                continue;
                            }
                            _ => {}
                        }
                        let Some(value) = inline_text(block) else {
                            break;
                        };
                        let next_is_strong = matches!(block, Block::Strong(_));
                        let next_is_link = matches!(block, Block::Link { .. });
                        let next_is_inline_code = matches!(block, Block::InlineCode(_));
                        let next_link_has_prefix =
                            matches!(block, Block::Link { link_start, .. } if *link_start > 0);
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
                                    next_is_strong,
                                    next_is_link,
                                    next_is_inline_code,
                                    next_link_has_prefix,
                                ));
                            }
                        }
                        let value_start = list_text.len();
                        list_text.push_str(value);
                        if next_is_strong {
                            strong_ranges.push(value_start..list_text.len());
                        }
                        if next_is_inline_code {
                            inline_code_ranges.push(value_start..list_text.len());
                        }
                        if let Block::Link {
                            url, link_start, ..
                        } = block
                        {
                            link_ranges.push(ArticleLinkRange {
                                range: value_start + *link_start..list_text.len(),
                                url: url.clone(),
                            });
                        }
                        previous_was_strong = next_is_strong;
                        previous_was_link = next_is_link;
                        previous_was_inline_code = next_is_inline_code;
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
                        ui.add_space(22.0 + item_depth.saturating_sub(1) as f32 * 24.0);
                        ui.vertical(|ui| {
                            ui.set_width(ui.available_width());
                            if list_text != "▪ " {
                                let (_, open) = selectable_text_block_with_inline_style(
                                    ui,
                                    article_id,
                                    start,
                                    &list_text,
                                    &strong_ranges,
                                    &inline_code_ranges,
                                    &link_ranges,
                                    ArticleTextStyle::List,
                                    selection,
                                    None,
                                );
                                if let Some(open) = open {
                                    ui.ctx().data_mut(|data| {
                                        data.insert_temp(
                                            egui::Id::new((
                                                "article-document-open-url",
                                                article_id,
                                            )),
                                            open,
                                        );
                                    });
                                }
                            }
                            for (uri, link_url, alt) in &images {
                                if let Some(open) =
                                    self.article_image(ui, viewport, uri, link_url.as_deref())
                                {
                                    ui.ctx().data_mut(|data| {
                                        data.insert_temp(
                                            egui::Id::new((
                                                "article-document-open-url",
                                                article_id,
                                            )),
                                            open,
                                        );
                                    });
                                }
                                if let Some(alt) = alt {
                                    ui.label(
                                        egui::RichText::new(alt).size(15.0).color(theme.muted),
                                    );
                                }
                            }
                        });
                    });
                    ui.add_space(20.0);
                }
                Block::ListItemEnd { .. } => index += 1,
                _ => {
                    let start = index;
                    let mut run = String::new();
                    let mut strong = Vec::new();
                    let mut inline_code = Vec::new();
                    let mut links = Vec::new();
                    let mut previous_was_strong = false;
                    let mut previous_was_link = false;
                    let mut previous_was_inline_code = false;
                    let mut previous_link_had_space_after = false;
                    let mut paragraph_count = 1_usize;
                    let mut run_char_count = 0_usize;
                    while index < blocks.len() {
                        let block = &blocks[index];
                        let Some(value) = inline_text(block) else {
                            break;
                        };
                        let next_is_strong = matches!(block, Block::Strong(_));
                        let next_is_link = matches!(block, Block::Link { .. });
                        let next_is_inline_code = matches!(block, Block::InlineCode(_));
                        let next_link_has_prefix =
                            matches!(block, Block::Link { link_start, .. } if *link_start > 0);
                        if !run.is_empty() {
                            if previous_link_had_space_after {
                                run.push(' ');
                                run_char_count += 1;
                            } else {
                                let separator = body_fragment_separator(
                                    &run,
                                    value,
                                    previous_was_strong,
                                    previous_was_link,
                                    previous_was_inline_code,
                                    next_is_strong,
                                    next_is_link,
                                    next_is_inline_code,
                                    next_link_has_prefix,
                                );
                                let starts_new_paragraph = separator == "\n\n";
                                let value_char_count = value.chars().count();
                                if starts_new_paragraph
                                    && (paragraph_count >= BODY_GALLEY_MAX_PARAGRAPHS
                                        || run_char_count + 2 + value_char_count
                                            > BODY_GALLEY_MAX_CHARS)
                                {
                                    break;
                                }
                                run.push_str(separator);
                                run_char_count += separator.chars().count();
                                if starts_new_paragraph {
                                    paragraph_count += 1;
                                }
                            }
                        }
                        let value_start = run.len();
                        run.push_str(value);
                        run_char_count += value.chars().count();
                        match block {
                            Block::Strong(_) => strong.push(value_start..run.len()),
                            Block::InlineCode(_) => inline_code.push(value_start..run.len()),
                            Block::Link {
                                url, link_start, ..
                            } => links.push(ArticleLinkRange {
                                range: value_start + *link_start..run.len(),
                                url: url.clone(),
                            }),
                            _ => {}
                        }
                        previous_was_strong = next_is_strong;
                        previous_was_link = next_is_link;
                        previous_was_inline_code = next_is_inline_code;
                        previous_link_had_space_after = matches!(
                            block,
                            Block::Link {
                                space_after: true,
                                ..
                            }
                        );
                        index += 1;
                    }
                    if run.trim().is_empty() {
                        index += 1;
                        continue;
                    }
                    #[cfg(test)]
                    {
                        self.last_frame_max_layout_chars =
                            self.last_frame_max_layout_chars.max(run.chars().count());
                        self.last_frame_layout_calls += 1;
                    }
                    let (_, open) = selectable_text_block_with_inline_style(
                        ui,
                        article_id,
                        start,
                        &run,
                        &strong,
                        &inline_code,
                        &links,
                        ArticleTextStyle::Body,
                        selection,
                        None,
                    );
                    if let Some(open) = open {
                        ui.ctx().data_mut(|data| {
                            data.insert_temp(
                                egui::Id::new(("article-document-open-url", article_id)),
                                open,
                            );
                        });
                    }
                    ui.add_space(20.0);
                }
            }
        }
    }

    fn selection_intents(
        &mut self,
        ctx: &egui::Context,
        article_id: i64,
        fingerprint: &ContentFingerprint,
        frame: &ArticleSelectionFrame,
    ) -> Vec<PresentationIntent> {
        let (pointer_pos, pressed, down, released) = ctx.input(|input| {
            (
                input.pointer.interact_pos(),
                input.pointer.primary_pressed(),
                input.pointer.primary_down(),
                input.pointer.primary_released(),
            )
        });
        let mut intents = Vec::new();
        let was_active = self.selection_drag.is_some();
        if self
            .selection_drag
            .as_ref()
            .is_some_and(|drag| drag.article_id != article_id || drag.fingerprint != *fingerprint)
        {
            self.selection_drag = None;
        }
        if pressed || (down && self.selection_drag.is_none()) {
            if let Some(cursor) =
                pointer_pos.and_then(|position| article_cursor_for_pointer(frame, position))
            {
                self.selection_drag = Some(ArticleSelectionDrag {
                    article_id,
                    fingerprint: fingerprint.clone(),
                    anchor: cursor,
                    focus: cursor,
                });
                intents.push(PresentationIntent::SelectionStarted);
            } else if pressed {
                self.selection_drag = None;
            }
        }
        if (down || released)
            && let Some(position) = pointer_pos
            && let Some(cursor) = article_cursor_nearest(frame, position)
            && let Some(drag) = self.selection_drag.as_mut()
            && drag.article_id == article_id
        {
            drag.focus = cursor;
        }
        if (released || (was_active && !down && !pressed))
            && let Some(drag) = self.selection_drag.take()
            && let Some(quote) = selected_quote_from_article_text(
                article_id,
                &frame.plain_text,
                drag.anchor.char_index,
                drag.focus.char_index,
            )
            && let Some((anchor_rect, source_layer)) = article_cursor_anchor(frame, drag.focus)
        {
            intents.push(PresentationIntent::SelectedQuote {
                quote,
                anchor_rect,
                source_layer,
            });
        }
        if let Some(url) = ctx.data_mut(|data| {
            data.remove_temp::<String>(egui::Id::new(("article-document-open-url", article_id)))
        }) {
            intents.push(PresentationIntent::OpenUrl(url));
        }
        intents
    }
}

fn inline_text(block: &Block) -> Option<&str> {
    match block {
        Block::Text(text)
        | Block::Strong(text)
        | Block::InlineCode(text)
        | Block::Link { text, .. } => Some(text),
        _ => None,
    }
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
    if parser::is_numbered_marker_only(previous.trim()) {
        return " ";
    }
    if next_is_link && !next_link_has_prefix {
        return "\n\n";
    }
    if previous_was_link && parser::is_numbered_heading(next) {
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
    if previous_was_link && is_ascii_word_char(previous_char) && is_ascii_word_char(next_char) {
        return "";
    }
    if is_closing_punctuation(next_char) {
        return "";
    }
    if is_sentence_ending(previous_char) || (previous_was_strong && next_is_strong) {
        return "\n\n";
    }
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

fn needs_typographic_space(left: char, right: char) -> bool {
    let left_word = left.is_alphanumeric() || left == '_';
    let right_word = right.is_alphanumeric() || right == '_';
    left_word && right_word && (left.is_ascii() || right.is_ascii())
}

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
    inline_code_ranges: Option<&[Range<usize>]>,
) -> (egui::Response, Option<String>) {
    selectable_text_block_with_inline_style(
        ui,
        article_id,
        block_index,
        text,
        strong_ranges,
        inline_code_ranges.unwrap_or_default(),
        link_ranges,
        style,
        selection_frame,
        None,
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
    _reserved: Option<()>,
) -> (egui::Response, Option<String>) {
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
    let mut sense = egui::Sense::click_and_drag();
    sense -= egui::Sense::FOCUSABLE;
    let (row_rect, mut response) = ui
        .push_id(("article-document-label", article_id, block_index), |ui| {
            ui.allocate_exact_size(egui::vec2(available_width, galley.size().y), sense)
        })
        .inner;
    response.set_intrinsic_size(galley.intrinsic_size());
    let galley_pos = row_rect.left_top() + egui::vec2(heading_inset, 0.0);
    if matches!(style, ArticleTextStyle::Heading) {
        ui.painter().rect_filled(
            egui::Rect::from_min_size(row_rect.left_top(), egui::vec2(6.0, galley.size().y)),
            egui::CornerRadius::same(1),
            ReaderTheme::sspai().accent,
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
    let mut open_url = None;
    if response.clicked()
        && !response.double_clicked()
        && !response.triple_clicked()
        && !link_ranges.is_empty()
        && let Some(pointer) = response.interact_pointer_pos()
        && global_text_rect.contains(pointer)
    {
        let local = global_from_galley.inverse() * pointer;
        let char_index: usize = galley.cursor_from_pos(local.to_vec2()).index.into();
        let byte_index = text
            .char_indices()
            .nth(char_index)
            .map(|(offset, _)| offset)
            .unwrap_or(text.len());
        open_url = link_ranges
            .iter()
            .find(|link| {
                link.range.contains(&byte_index)
                    || (byte_index > 0 && link.range.contains(&(byte_index - 1)))
            })
            .map(|link| link.url.clone());
    }
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
    (response, open_url)
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
    let theme = ReaderTheme::sspai();
    let (font_size, line_height, color) = match style {
        ArticleTextStyle::Title => (32.0, 41.0, theme.text),
        ArticleTextStyle::Body | ArticleTextStyle::List => (17.0, 29.0, theme.text),
        ArticleTextStyle::Heading => (22.0, 31.0, theme.text),
        ArticleTextStyle::Quote => (17.0, 29.0, theme.muted),
        ArticleTextStyle::Code => (14.0, 21.0, egui::Color32::from_rgb(85, 85, 85)),
    };
    let family = if matches!(style, ArticleTextStyle::Code) {
        egui::FontFamily::Monospace
    } else {
        egui::FontFamily::Proportional
    };
    let normal = egui::text::TextFormat {
        font_id: egui::FontId::new(font_size, family),
        line_height: Some(line_height),
        color,
        ..Default::default()
    };
    let strong = egui::text::TextFormat {
        font_id: egui::FontId::new(font_size, egui::FontFamily::Name("cjk-bold".into())),
        ..normal.clone()
    };
    let mut job = egui::text::LayoutJob::default();
    if matches!(style, ArticleTextStyle::Code) {
        job.append(text, 0.0, normal);
    } else if matches!(style, ArticleTextStyle::Title | ArticleTextStyle::Heading) {
        append_inline_layout(
            &mut job,
            text,
            &[],
            inline_code_ranges,
            link_ranges,
            &strong,
            &strong,
        );
    } else {
        append_inline_layout(
            &mut job,
            text,
            strong_ranges,
            inline_code_ranges,
            link_ranges,
            &normal,
            &strong,
        );
    }
    job.wrap.max_width = wrap_width.max(1.0);
    job.keep_trailing_whitespace = true;
    job
}

#[allow(clippy::too_many_arguments)]
fn append_inline_layout(
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
    let mut link = normal.clone();
    link.color = ReaderTheme::sspai().link;
    let mut code = normal.clone();
    code.font_id = egui::FontId::new(
        (normal.font_id.size * 0.92).max(12.0),
        egui::FontFamily::Monospace,
    );
    code.background = ReaderTheme::sspai().code_bg;
    for pair in boundaries.windows(2) {
        let start = pair[0].min(text.len());
        let end = pair[1].max(start).min(text.len());
        if start >= end || !text.is_char_boundary(start) || !text.is_char_boundary(end) {
            continue;
        }
        let format = if link_ranges
            .iter()
            .any(|range| range.range.start <= start && end <= range.range.end)
        {
            link.clone()
        } else if inline_code_ranges
            .iter()
            .any(|range| range.start <= start && end <= range.end)
        {
            code.clone()
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

fn article_cursor_for_pointer(
    frame: &ArticleSelectionFrame,
    position: egui::Pos2,
) -> Option<ArticleDocCursor> {
    article_cursor_under_pointer(frame, position).or_else(|| {
        let bounds = frame
            .spans
            .iter()
            .map(|span| span.global_rect)
            .reduce(|left, right| left.union(right))?;
        bounds
            .expand(10.0)
            .contains(position)
            .then(|| article_cursor_nearest(frame, position))
            .flatten()
    })
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
        .find(|(_, span)| span.global_rect.contains(position))
        .map(|(index, span)| article_cursor_in_span(index, span, position))
}

fn article_cursor_nearest(
    frame: &ArticleSelectionFrame,
    position: egui::Pos2,
) -> Option<ArticleDocCursor> {
    frame
        .spans
        .iter()
        .enumerate()
        .min_by(|(_, left), (_, right)| {
            rect_distance(left.global_rect, position)
                .total_cmp(&rect_distance(right.global_rect, position))
        })
        .map(|(index, span)| article_cursor_in_span(index, span, position))
}

fn rect_distance(rect: egui::Rect, position: egui::Pos2) -> f32 {
    let x = if position.x < rect.left() {
        rect.left() - position.x
    } else if position.x > rect.right() {
        position.x - rect.right()
    } else {
        0.0
    };
    let y = if position.y < rect.top() {
        rect.top() - position.y
    } else if position.y > rect.bottom() {
        position.y - rect.bottom()
    } else {
        0.0
    };
    x * x + y * y
}

fn article_cursor_in_span(
    span_index: usize,
    span: &RenderedArticleSpan,
    position: egui::Pos2,
) -> ArticleDocCursor {
    let local = span.global_from_galley.inverse() * position;
    let span_len = span.chars.end.saturating_sub(span.chars.start);
    let local_char = usize::from(span.galley.cursor_from_pos(local.to_vec2()).index).min(span_len);
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
    let local = cursor
        .local_char
        .min(span.chars.end.saturating_sub(span.chars.start));
    let rect = span.galley.pos_from_cursor(egui::text::CCursor::new(local));
    Some((
        (span.global_from_galley * rect).expand(3.0),
        span.source_layer,
    ))
}

fn selected_quote_from_article_text(
    article_id: i64,
    text: &str,
    start: usize,
    end: usize,
) -> Option<SelectedQuote> {
    let chars = text.chars().collect::<Vec<_>>();
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
    let anchor = TextAnchor::capture(text, lo, hi, 32);
    Some(SelectedQuote {
        article_id,
        text: chars[lo..hi].iter().collect(),
        start_offset: anchor.start_offset,
        end_offset: anchor.end_offset,
        anchor_prefix: anchor.prefix,
        anchor_suffix: anchor.suffix,
    })
}

impl ArticleDocumentPresenter {
    fn formula_block(&mut self, ui: &mut egui::Ui, source: &str, display: bool) {
        let key = format!("{}:{source}", if display { "display" } else { "inline" });
        if !self.formula_cache.contains_key(&key) {
            let job = FormulaJob {
                key: key.clone(),
                source: source.to_owned(),
                display,
            };
            if self.formula_job_tx.send(job).is_ok() {
                self.formula_cache
                    .insert(key.clone(), FormulaState::Loading);
            } else {
                self.formula_cache.insert(
                    key.clone(),
                    FormulaState::Failed("公式排版线程没有响应".to_owned()),
                );
            }
        }
        let theme = ReaderTheme::sspai();
        egui::Frame::new()
            .fill(theme.code_bg)
            .corner_radius(egui::CornerRadius::same(4))
            .inner_margin(egui::Margin::symmetric(18, 12))
            .show(ui, |ui| match self.formula_cache.get(&key) {
                Some(FormulaState::Ready(bytes)) => {
                    let mut hasher = std::collections::hash_map::DefaultHasher::new();
                    key.hash(&mut hasher);
                    ui.add(
                        egui::Image::from_bytes(
                            format!("bytes://formula/{:016x}.svg", hasher.finish()),
                            bytes.clone(),
                        )
                        .max_width(ui.available_width())
                        .max_height(if display { 180.0 } else { 72.0 })
                        .maintain_aspect_ratio(true)
                        .show_loading_spinner(false),
                    )
                    .on_hover_text(format!("TeX：{source}"));
                }
                Some(FormulaState::Loading) => {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label("正在排版公式…");
                    });
                }
                Some(FormulaState::Failed(error)) => {
                    ui.monospace(source)
                        .on_hover_text(format!("公式排版失败，保留 TeX：{error}"));
                }
                None => {}
            });
    }

    fn article_image(
        &mut self,
        ui: &mut egui::Ui,
        viewport: &egui::Rect,
        uri: &str,
        link_url: Option<&str>,
    ) -> Option<String> {
        let width = ui.available_width();
        let natural = self.image_cache.get(uri).and_then(|state| match state {
            ImageState::Ready { dimensions, .. } => *dimensions,
            _ => None,
        });
        let height = natural
            .filter(|(w, h)| *w > 0 && *h > 0)
            .map(|(w, h)| (width * h as f32 / w as f32).clamp(160.0, 900.0))
            .unwrap_or_else(|| match self.image_cache.get(uri) {
                Some(ImageState::Failed(_)) => 180.0,
                _ => (width * 0.42).clamp(200.0, 340.0),
            });
        let (rect, response) =
            ui.allocate_exact_size(egui::vec2(width, height), egui::Sense::click());
        let content_rect = rect.translate(-ui.max_rect().min.to_vec2());
        let visible = content_rect.intersects(viewport.expand2(egui::vec2(0.0, 600.0)));
        if visible && !self.image_cache.contains_key(uri) {
            if self.image_job_tx.send(uri.to_owned()).is_ok() {
                self.image_cache.insert(
                    uri.to_owned(),
                    ImageState::Loading {
                        started: Instant::now(),
                        attempt: 1,
                    },
                );
            } else {
                self.image_cache.insert(
                    uri.to_owned(),
                    ImageState::Failed(ImageFailure {
                        message: "图片下载线程没有响应".to_owned(),
                        detail: "image job channel disconnected".to_owned(),
                        attempts: 0,
                        retryable: true,
                    }),
                );
            }
        }
        let theme = ReaderTheme::sspai();
        match self.image_cache.get(uri) {
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
            Some(ImageState::Loading { started, attempt }) => {
                ui.painter().rect_filled(rect, 5.0, theme.code_bg);
                let spinner = egui::Rect::from_center_size(rect.center(), egui::vec2(28.0, 28.0));
                ui.put(spinner, egui::Spinner::new().size(24.0));
                ui.painter().text(
                    rect.center() + egui::vec2(0.0, 34.0),
                    egui::Align2::CENTER_CENTER,
                    format!(
                        "正在下载 {attempt}/{IMAGE_MAX_ATTEMPTS} · {:.0}s",
                        started.elapsed().as_secs_f32()
                    ),
                    egui::FontId::proportional(15.0),
                    theme.muted,
                );
            }
            Some(ImageState::Failed(error)) => {
                ui.painter().rect_filled(rect, 5.0, theme.code_bg);
                ui.painter().text(
                    rect.center(),
                    egui::Align2::CENTER_CENTER,
                    format!("图片暂时无法加载\n{}\n单击重试", error.message),
                    egui::FontId::proportional(15.0),
                    ui.visuals().error_fg_color,
                );
                response.clone().on_hover_text(format!(
                    "{}\n尝试次数：{}；{}",
                    error.detail,
                    error.attempts,
                    if error.retryable {
                        "可重试"
                    } else {
                        "不可重试"
                    }
                ));
            }
            None => {
                ui.painter().rect_filled(rect, 5.0, theme.code_bg);
                ui.painter().text(
                    rect.center(),
                    egui::Align2::CENTER_CENTER,
                    "滚动到这里后加载图片",
                    egui::FontId::proportional(15.0),
                    theme.muted,
                );
            }
        }
        let mut retry = false;
        let mut open = None;
        if response.clicked() {
            if matches!(self.image_cache.get(uri), Some(ImageState::Failed(_))) {
                retry = true;
            } else {
                open = link_url.map(str::to_owned);
            }
        }
        response.context_menu(|ui| {
            if ui.button("重新加载图片").clicked() {
                retry = true;
                ui.close();
            }
            if ui.button("在浏览器中打开图片").clicked() {
                open = Some(uri.to_owned());
                ui.close();
            }
            if let Some(url) = link_url
                && ui.button("打开图片链接").clicked()
            {
                open = Some(url.to_owned());
                ui.close();
            }
        });
        if retry {
            ui.ctx().forget_image(&format!("bytes://{uri}"));
            if self.image_job_tx.send(uri.to_owned()).is_ok() {
                self.image_cache.insert(
                    uri.to_owned(),
                    ImageState::Loading {
                        started: Instant::now(),
                        attempt: 1,
                    },
                );
                ui.ctx().request_repaint();
            }
        }
        ui.add_space(15.0);
        open
    }
}

fn image_client() -> Result<reqwest::blocking::Client> {
    Ok(reqwest::blocking::Client::builder()
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
        .build()?)
}

/// Private seam around the only remote dependency in document presentation.
///
/// The desktop application deliberately has one production implementation;
/// the trait exists so retry and cache behavior can be verified without real
/// network timing or a public extension contract.
trait ImageFetch: Send + Sync {
    fn fetch_once(&self, uri: &str, attempt: u8) -> std::result::Result<Arc<[u8]>, ImageFailure>;
}

struct HttpImageFetch(reqwest::blocking::Client);

impl ImageFetch for HttpImageFetch {
    fn fetch_once(&self, uri: &str, attempt: u8) -> std::result::Result<Arc<[u8]>, ImageFailure> {
        download_image_once(&self.0, uri, attempt)
    }
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
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    renderer.render_tex(&job.source, &options)
                }))
                .map_err(|_| "公式排版失败".to_owned())
                .and_then(|value| value.map_err(|error| error.to_string()))
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
    fetch: Arc<dyn ImageFetch>,
    jobs: std_mpsc::Receiver<String>,
    events: std_mpsc::Sender<ImageEvent>,
    store: Arc<ImageStore>,
) {
    let jobs = Arc::new(Mutex::new(jobs));
    for worker in 0..IMAGE_WORKER_COUNT {
        let fetch = fetch.clone();
        let jobs = jobs.clone();
        let events = events.clone();
        let store = store.clone();
        std::thread::Builder::new()
            .name(format!("shiyue-image-{worker}"))
            .spawn(move || {
                loop {
                    let uri = {
                        let Ok(receiver) = jobs.lock() else { return };
                        let Ok(uri) = receiver.recv() else { return };
                        uri
                    };
                    let result =
                        load_cached_or_download_image(fetch.as_ref(), &store, &uri, &events);
                    if events.send(ImageEvent::Complete { uri, result }).is_err() {
                        return;
                    }
                }
            })
            .expect("failed to spawn image worker");
    }
}

fn load_cached_or_download_image(
    fetch: &dyn ImageFetch,
    store: &ImageStore,
    uri: &str,
    events: &std_mpsc::Sender<ImageEvent>,
) -> std::result::Result<Arc<[u8]>, ImageFailure> {
    match store.get(uri) {
        Ok(Some(bytes)) if image::load_from_memory(&bytes).is_ok() => {
            return Ok(Arc::from(bytes));
        }
        Ok(_) => {}
        Err(error) => tracing::warn!("读取图片缓存失败：{error:#}"),
    }
    let bytes = download_image_with_retry(fetch, uri, events)?;
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
    fetch: &dyn ImageFetch,
    uri: &str,
    events: &std_mpsc::Sender<ImageEvent>,
) -> std::result::Result<Arc<[u8]>, ImageFailure> {
    let mut last_failure = None;
    for attempt in 1..=IMAGE_MAX_ATTEMPTS {
        if attempt > 1 {
            let _ = events.send(ImageEvent::Progress {
                uri: uri.to_owned(),
                attempt,
            });
            std::thread::sleep(if attempt == 2 {
                Duration::from_millis(500)
            } else {
                Duration::from_millis(1_500)
            });
        }
        match fetch.fetch_once(uri, attempt) {
            Ok(bytes) => return Ok(bytes),
            Err(failure) => {
                let retry = failure.retryable && attempt < IMAGE_MAX_ATTEMPTS;
                last_failure = Some(failure);
                if !retry {
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
) -> std::result::Result<Arc<[u8]>, ImageFailure> {
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
            detail: format!("图片超过 {} MB 的安全上限", IMAGE_MAX_BYTES / 1024 / 1024),
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
    if bytes.is_empty() || bytes.len() as u64 > IMAGE_MAX_BYTES {
        return Err(ImageFailure {
            message: if bytes.is_empty() {
                "服务器返回了空图片"
            } else {
                "图片文件过大，已停止显示"
            }
            .to_owned(),
            detail: uri.to_owned(),
            attempts: attempt,
            retryable: bytes.is_empty(),
        });
    }
    Ok(Arc::from(bytes))
}

fn image_request_failure(error: reqwest::Error, attempt: u8) -> ImageFailure {
    let status = error.status();
    let retryable = error.is_timeout()
        || error.is_connect()
        || error.is_body()
        || status.is_some_and(|status| {
            status.is_server_error()
                || status == reqwest::StatusCode::REQUEST_TIMEOUT
                || status == reqwest::StatusCode::TOO_MANY_REQUESTS
        });
    let message = if error.is_timeout() {
        "图片服务器响应超时"
    } else if error.is_connect() {
        "无法连接图片服务器"
    } else if let Some(status) = status {
        if status == reqwest::StatusCode::NOT_FOUND {
            "图片已被服务器删除"
        } else if status == reqwest::StatusCode::FORBIDDEN {
            "图片服务器拒绝访问"
        } else {
            "图片服务器返回错误"
        }
    } else {
        "图片下载失败"
    };
    ImageFailure {
        message: message.to_owned(),
        detail: reqwest_error_chain(&error),
        attempts: attempt,
        retryable,
    }
}

fn reqwest_error_chain(error: &reqwest::Error) -> String {
    let mut detail = error.to_string();
    let mut source = error.source();
    while let Some(error) = source {
        let value = error.to_string();
        if !detail.contains(&value) {
            detail.push_str(": ");
            detail.push_str(&value);
        }
        source = error.source();
    }
    detail
}

#[cfg(test)]
mod tests {
    use super::{
        ArticleDocumentPresenter, ArticleDocumentSource, ArticleTextStyle, BODY_GALLEY_MAX_CHARS,
        ContentFingerprint, FormulaEvent, FormulaJob, IMAGE_MAX_ATTEMPTS, ImageEvent, ImageFailure,
        ImageFetch, PresentRequest, RestoreSelection, article_layout_job, body_block_separator,
        body_fragment_separator, download_image_with_retry, load_cached_or_download_image,
        selected_quote_from_article_text,
    };
    use crate::image_store::ImageStore;
    use crate::model::TextAnchor;
    use eframe::egui;
    use std::sync::atomic::{AtomicU8, Ordering};
    use std::sync::{Arc, mpsc};

    fn presenter_without_workers() -> ArticleDocumentPresenter {
        let (image_job_tx, _image_jobs) = mpsc::channel::<String>();
        let (_image_events, image_event_rx) = mpsc::channel::<ImageEvent>();
        let (formula_job_tx, _formula_jobs) = mpsc::channel::<FormulaJob>();
        let (_formula_events, formula_event_rx) = mpsc::channel::<FormulaEvent>();
        ArticleDocumentPresenter {
            active: None,
            prepared_by_fingerprint: Default::default(),
            recency: Default::default(),
            selection_drag: None,
            image_cache: Default::default(),
            image_job_tx,
            image_event_rx,
            formula_cache: Default::default(),
            formula_job_tx,
            formula_event_rx,
            last_frame_max_layout_chars: 0,
            last_frame_layout_calls: 0,
        }
    }

    #[test]
    fn semantic_parser_is_private_to_article_document_presentation() {
        let crate_root = include_str!("lib.rs");
        let presentation = include_str!("article_document_presentation.rs");
        let parser = include_str!("article_document_presentation/parser.rs");
        let gui = include_str!("gui.rs");
        let knowledge = include_str!("knowledge_workflow.rs");
        let clipping = include_str!("web_clipping_lifecycle.rs");

        assert!(!crate_root.contains("mod text;"));
        assert!(presentation.contains("mod parser;"));
        assert!(parser.contains("pub(super) enum Block"));
        assert!(!parser.contains("pub(crate)"));
        for caller in [gui, knowledge, clipping] {
            assert!(!caller.contains("crate::text"));
            assert!(!caller.contains("parser::Block"));
            assert!(!caller.contains("content_blocks("));
        }
    }

    #[test]
    fn fingerprint_includes_title_body_and_base_url() {
        let source = |title, html, base_url| ArticleDocumentSource {
            article_id: 1,
            title,
            html,
            base_url,
        };
        let original = ContentFingerprint::of(source("Title", "<p>Body</p>", Some("https://a/")));
        assert_ne!(
            original,
            ContentFingerprint::of(source("Other", "<p>Body</p>", Some("https://a/")))
        );
        assert_ne!(
            original,
            ContentFingerprint::of(source("Title", "<p>Other</p>", Some("https://a/")))
        );
        assert_ne!(
            original,
            ContentFingerprint::of(source("Title", "<p>Body</p>", Some("https://b/")))
        );
        assert_ne!(
            ContentFingerprint::of(source("a\0b", "c", None)),
            ContentFingerprint::of(source("a", "b\0c", None)),
            "length-delimited fields must not alias"
        );
    }

    #[test]
    fn preparation_is_reused_by_exact_content_and_replaced_after_revision() {
        let mut presenter = presenter_without_workers();
        let source = |title, html| ArticleDocumentSource {
            article_id: 7,
            title,
            html,
            base_url: Some("https://example.com/post"),
        };

        let first = presenter.prepare(source("Title", "<p>Body</p>"));
        let repeated = presenter.prepare(source("Title", "<p>Body</p>"));
        assert!(Arc::ptr_eq(&first, &repeated));

        let revised = presenter.prepare(source("Title", "<p>Revised</p>"));
        assert!(!Arc::ptr_eq(&first, &revised));
        assert_ne!(first.fingerprint, revised.fingerprint);
    }

    #[test]
    fn external_interface_presents_a_mixed_document_at_multiple_widths() {
        let mut presenter = presenter_without_workers();
        let context = egui::Context::default();
        let mut fonts = egui::FontDefinitions::default();
        let fallback = fonts
            .families
            .get(&egui::FontFamily::Proportional)
            .cloned()
            .unwrap_or_default();
        fonts
            .families
            .insert(egui::FontFamily::Name("cjk-bold".into()), fallback);
        context.set_fonts(fonts);
        let html = r#"
            <h2>架构说明</h2>
            <p>中文正文包含 <strong>重点</strong>、<code>inline_code</code>
               和 <a href="/guide">relative link</a>。</p>
            <ul><li>第一项</li><li>第二项 <strong>加粗</strong></li></ul>
            <table><tr><th>名称</th><th>用途</th></tr><tr><td>Pipeline</td><td>呈现</td></tr></table>
            <dl><dt>Seam</dt><dd>稳定边界</dd></dl>
        "#;
        let source = ArticleDocumentSource {
            article_id: 9,
            title: "Mixed document",
            html,
            base_url: Some("https://example.com/article"),
        };

        for width in [900.0, 520.0] {
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(width, 900.0),
                )),
                ..Default::default()
            };
            let _ = context.run_ui(input, |ui| {
                let viewport = ui.clip_rect();
                let outcome = presenter.show(
                    ui,
                    PresentRequest {
                        source,
                        viewport,
                        restore_selection: None,
                        scroll_title_into_view: false,
                    },
                    |_| {},
                );
                assert!(!outcome.restore_failed);
            });
        }

        assert_eq!(presenter.prepared_by_fingerprint.len(), 1);
    }

    #[test]
    fn downward_scroll_bounds_galley_size_and_viewport_layout_calls() {
        let mut presenter = presenter_without_workers();
        let context = egui::Context::default();
        let mut fonts = egui::FontDefinitions::default();
        let fallback = fonts
            .families
            .get(&egui::FontFamily::Proportional)
            .cloned()
            .unwrap_or_default();
        fonts
            .families
            .insert(egui::FontFamily::Name("cjk-bold".into()), fallback);
        context.set_fonts(fonts);
        let html = (0..1_200)
            .map(|index| format!("<p>第 {index} 段包含足够长的正文，用于模拟真实长文章滚动。</p>"))
            .collect::<String>();
        let source = ArticleDocumentSource {
            article_id: 77,
            title: "Long document",
            html: &html,
            base_url: Some("https://example.com/long"),
        };

        for viewport_top in [0.0, 24_000.0] {
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(820.0, 900.0),
                )),
                ..Default::default()
            };
            let _ = context.run_ui(input, |ui| {
                presenter.show(
                    ui,
                    PresentRequest {
                        source,
                        viewport: egui::Rect::from_min_size(
                            egui::pos2(0.0, viewport_top),
                            egui::vec2(820.0, 900.0),
                        ),
                        restore_selection: None,
                        scroll_title_into_view: false,
                    },
                    |_| {},
                );
            });
        }

        assert!(
            presenter.last_frame_max_layout_chars <= BODY_GALLEY_MAX_CHARS,
            "向下滚动的一帧仍需处理包含 {} 个字符的单一巨型文本布局",
            presenter.last_frame_max_layout_chars
        );
        assert!(
            presenter.last_frame_layout_calls <= 80,
            "向下滚动的一帧仍完整布局了 {} 个段落",
            presenter.last_frame_layout_calls
        );
    }

    #[test]
    fn real_scroll_area_keeps_document_height_stable_across_scroll_offsets() {
        let mut presenter = presenter_without_workers();
        let context = egui::Context::default();
        let mut fonts = egui::FontDefinitions::default();
        let fallback = fonts
            .families
            .get(&egui::FontFamily::Proportional)
            .cloned()
            .unwrap_or_default();
        fonts
            .families
            .insert(egui::FontFamily::Name("cjk-bold".into()), fallback);
        context.set_fonts(fonts);
        let html = (0..600)
            .map(|index| {
                format!(
                    "<p>Paragraph {index} has enough text to wrap across multiple lines at the test width. This makes estimated and measured heights observably different.</p>"
                )
            })
            .collect::<String>();
        let source = ArticleDocumentSource {
            article_id: 79,
            title: "Stable document geometry",
            html: &html,
            base_url: Some("https://example.com/stable"),
        };
        let mut heights = Vec::new();

        for offset in [0.0, 8_000.0, 16_000.0, 8_000.0] {
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(820.0, 900.0),
                )),
                ..Default::default()
            };
            let _ = context.run_ui(input, |ui| {
                ui.set_width(820.0);
                ui.set_height(900.0);
                let output = egui::ScrollArea::vertical()
                    .id_salt("article-geometry-regression")
                    .scroll_offset(egui::vec2(0.0, offset))
                    .show_viewport(ui, |ui, viewport| {
                        presenter.show(
                            ui,
                            PresentRequest {
                                source,
                                viewport,
                                restore_selection: None,
                                scroll_title_into_view: false,
                            },
                            |_| {},
                        );
                    });
                heights.push(output.content_size.y);
            });
        }

        let min_height = heights.iter().copied().fold(f32::INFINITY, f32::min);
        let max_height = heights.iter().copied().fold(f32::NEG_INFINITY, f32::max);
        assert!(
            max_height - min_height <= 1.0,
            "scrolling changed document height from {min_height} to {max_height}: {heights:?}"
        );
    }

    #[test]
    fn excerpt_restore_forces_its_offscreen_span_to_receive_real_geometry() {
        let mut presenter = presenter_without_workers();
        let context = egui::Context::default();
        let mut fonts = egui::FontDefinitions::default();
        let fallback = fonts
            .families
            .get(&egui::FontFamily::Proportional)
            .cloned()
            .unwrap_or_default();
        fonts
            .families
            .insert(egui::FontFamily::Name("cjk-bold".into()), fallback);
        context.set_fonts(fonts);
        let html = (0..200)
            .map(|index| format!("<p>唯一段落 {index} 的正文内容。</p>"))
            .collect::<String>();
        let source = ArticleDocumentSource {
            article_id: 78,
            title: "Restore document",
            html: &html,
            base_url: Some("https://example.com/restore"),
        };
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(820.0, 900.0),
            )),
            ..Default::default()
        };
        let mut outcome = None;
        let _ = context.run_ui(input, |ui| {
            outcome = Some(presenter.show(
                ui,
                PresentRequest {
                    source,
                    viewport: egui::Rect::from_min_size(
                        egui::pos2(0.0, 20_000.0),
                        egui::vec2(820.0, 900.0),
                    ),
                    restore_selection: Some(RestoreSelection {
                        selected_text: "唯一段落 150 的正文内容。".to_owned(),
                        anchor: TextAnchor {
                            start_offset: None,
                            end_offset: None,
                            prefix: String::new(),
                            suffix: String::new(),
                        },
                    }),
                    scroll_title_into_view: false,
                },
                |_| {},
            ));
        });
        let outcome = outcome.unwrap();
        assert!(!outcome.restore_failed);
        assert!(outcome.restored_span_top.is_some());
    }

    #[test]
    fn quote_offsets_are_unicode_characters_and_trim_whitespace() {
        let quote = selected_quote_from_article_text(42, "甲乙\n\n😀丙丁", 1, 6).unwrap();
        assert_eq!(quote.text, "乙\n\n😀丙");
        assert_eq!(quote.start_offset, Some(1));
        assert_eq!(quote.end_offset, Some(6));

        let reverse = selected_quote_from_article_text(42, "  前文 后文  ", 9, 2).unwrap();
        assert_eq!(reverse.text, "前文 后文");
        assert_eq!(reverse.start_offset, Some(2));
        assert_eq!(reverse.end_offset, Some(7));
        assert!(selected_quote_from_article_text(42, "甲 \n\n 乙", 1, 5).is_none());
    }

    #[test]
    fn mixed_language_inline_fragments_keep_punctuation_and_code_attached() {
        assert_eq!(
            body_block_separator("统一内存", "。它的好处", true, false, false, false, false),
            ""
        );
        assert_eq!(
            body_block_separator("这是完整一句。", "下一段", true, false, false, false, false),
            "\n\n"
        );
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
    fn inline_code_layout_keeps_monospace_range_inside_sentence() {
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

    struct SequenceFetch {
        calls: AtomicU8,
        bytes: Arc<[u8]>,
    }

    impl ImageFetch for SequenceFetch {
        fn fetch_once(
            &self,
            _uri: &str,
            attempt: u8,
        ) -> std::result::Result<Arc<[u8]>, ImageFailure> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if attempt == 1 {
                Err(ImageFailure {
                    message: "temporary".into(),
                    detail: "fake transient failure".into(),
                    attempts: attempt,
                    retryable: true,
                })
            } else {
                Ok(self.bytes.clone())
            }
        }
    }

    #[test]
    fn private_image_adapter_drives_observable_retry_without_network() {
        let fetch = SequenceFetch {
            calls: AtomicU8::new(0),
            bytes: Arc::from([1_u8, 2, 3]),
        };
        let (events, received) = mpsc::channel();
        let result = download_image_with_retry(&fetch, "https://example.com/image", &events)
            .expect("second deterministic attempt succeeds");

        assert_eq!(result.as_ref(), &[1, 2, 3]);
        assert_eq!(fetch.calls.load(Ordering::SeqCst), 2);
        assert!(matches!(
            received.try_recv(),
            Ok(ImageEvent::Progress { attempt: 2, .. })
        ));
        assert_eq!(IMAGE_MAX_ATTEMPTS, 3);
    }

    struct PanicFetch;

    impl ImageFetch for PanicFetch {
        fn fetch_once(
            &self,
            _uri: &str,
            _attempt: u8,
        ) -> std::result::Result<Arc<[u8]>, ImageFailure> {
            panic!("cache hit must not call the network adapter")
        }
    }

    #[test]
    fn persistent_image_cache_bypasses_remote_adapter() {
        let root = std::env::temp_dir().join(format!(
            "shiyue-article-document-image-{}-{}",
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
        let (events, received) = mpsc::channel();

        let bytes = load_cached_or_download_image(&PanicFetch, &store, uri, &events).unwrap();
        assert!(image::load_from_memory(bytes.as_ref()).is_ok());
        assert!(received.try_recv().is_err());

        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }
}
