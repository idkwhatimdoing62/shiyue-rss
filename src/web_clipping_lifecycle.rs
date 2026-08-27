//! Authoritative lifecycle for capturing and deleting immutable Web Clippings.
//!
//! The desktop submits raw capture input and observes a revisioned lease. URL
//! interpretation, safe fetching, HTML preparation, persistence, deletion and
//! Article Library projection all stay inside this module.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Instant;

use anyhow::{Result, anyhow};
use chrono::Utc;
use reqwest::Url;
use rusqlite::{Connection, OptionalExtension, params};

use crate::article_document_presentation::prepare_article_html;
use crate::article_library_lifecycle::{ArticleLibraryProjection, ProjectionScope, project_on};
use crate::db::{Db, WEB_CLIPPINGS_FEED_URL};
use crate::library_projection_revision::{
    self, LibraryGeneration, ProjectionFamily, ProjectionImpact, ProjectionStamp,
};
use crate::local_data_maintenance::{FencedTransaction, MaintenanceFence, MaintenanceParticipant};

const MAX_INPUT_URL_BYTES: usize = 16 * 1024;
const MAX_TECHNICAL_DETAIL_CHARS: usize = 800;
const WEB_CLIPPINGS_FEED_TITLE: &str = "网页收藏";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct CaptureId(u64);

impl CaptureId {
    pub(crate) fn value(self) -> u64 {
        self.0
    }
}

#[derive(Debug, Clone)]
pub(crate) struct CaptureRequest {
    pub(crate) source: String,
    pub(crate) title_override: Option<String>,
    pub(crate) pasted_html_base_url: Option<String>,
    pub(crate) refresh_scope: ProjectionScope,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WebClippingIdentity {
    pub(crate) article_id: i64,
    pub(crate) title: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CaptureProvenance {
    Fetched {
        original_url: String,
        final_url: String,
    },
    PastedHtml {
        base_url: Option<String>,
    },
}

#[derive(Debug, Clone)]
pub(crate) struct CaptureSuccess {
    pub(crate) clipping: WebClippingIdentity,
    pub(crate) provenance: CaptureProvenance,
    pub(crate) projection: ArticleLibraryProjection,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CaptureFailureKind {
    Input,
    Security,
    Network,
    Content,
    Maintenance,
    Storage,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CaptureFailure {
    pub(crate) kind: CaptureFailureKind,
    pub(crate) user_message: String,
    pub(crate) technical_detail: String,
    pub(crate) retryable: bool,
}

impl CaptureFailure {
    fn new(
        kind: CaptureFailureKind,
        user_message: impl Into<String>,
        detail: impl std::fmt::Display,
        retryable: bool,
    ) -> Self {
        Self {
            kind,
            user_message: user_message.into(),
            technical_detail: bounded_detail(detail),
            retryable,
        }
    }

    fn cancelled(detail: impl std::fmt::Display) -> Self {
        Self::new(
            CaptureFailureKind::Cancelled,
            "已取消网页收藏",
            detail,
            false,
        )
    }

    fn maintenance(detail: impl std::fmt::Display) -> Self {
        Self::new(
            CaptureFailureKind::Maintenance,
            "资料维护期间不能保存网页",
            detail,
            false,
        )
    }

    fn storage(detail: impl std::fmt::Display) -> Self {
        let detail = detail.to_string();
        if detail.contains("MAINTENANCE_IN_PROGRESS") || detail.contains("STALE_LIBRARY_EPOCH") {
            return Self::maintenance(detail);
        }
        Self::new(CaptureFailureKind::Storage, "保存网页失败", detail, false)
    }
}

#[derive(Debug, Clone)]
pub(crate) enum CaptureState {
    Fetching,
    Preparing,
    Committing,
    Succeeded(Box<CaptureSuccess>),
    Failed(CaptureFailure),
    Cancelled(CaptureFailure),
}

impl CaptureState {
    pub(crate) fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Succeeded(_) | Self::Failed(_) | Self::Cancelled(_)
        )
    }
}

#[derive(Debug, Clone)]
pub(crate) struct CaptureSnapshot {
    pub(crate) id: CaptureId,
    pub(crate) revision: u64,
    pub(crate) state: CaptureState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CancelDisposition {
    CancelledBeforeCommit,
    CommitAlreadyStarted,
    AlreadyTerminal,
}

#[derive(Debug, Clone, thiserror::Error)]
#[error("{user_message}")]
pub(crate) struct BeginCaptureFailure {
    pub(crate) failure: CaptureFailure,
    pub(crate) user_message: String,
}

impl BeginCaptureFailure {
    fn from_failure(failure: CaptureFailure) -> Self {
        Self {
            user_message: failure.user_message.clone(),
            failure,
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct DeleteRequest {
    pub(crate) article_id: i64,
    pub(crate) refresh_scope: ProjectionScope,
}

#[derive(Debug, Clone)]
pub(crate) struct DeleteOutcome {
    pub(crate) deleted: WebClippingIdentity,
    pub(crate) detached_resource_ids: Vec<i64>,
    pub(crate) projection: ArticleLibraryProjection,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DeleteFailureKind {
    Input,
    NotFound,
    ArticleSummaryActive,
    Maintenance,
    Storage,
}

#[derive(Debug, Clone, thiserror::Error)]
#[error("{user_message}")]
pub(crate) struct DeleteFailure {
    pub(crate) kind: DeleteFailureKind,
    pub(crate) user_message: String,
    pub(crate) technical_detail: String,
}

impl DeleteFailure {
    fn new(
        kind: DeleteFailureKind,
        user_message: impl Into<String>,
        detail: impl std::fmt::Display,
    ) -> Self {
        Self {
            kind,
            user_message: user_message.into(),
            technical_detail: bounded_detail(detail),
        }
    }

    fn storage(error: impl std::fmt::Display) -> Self {
        let detail = error.to_string();
        if detail.contains("MAINTENANCE_IN_PROGRESS") || detail.contains("STALE_LIBRARY_EPOCH") {
            return Self::new(
                DeleteFailureKind::Maintenance,
                "资料维护期间不能删除网页收藏",
                detail,
            );
        }
        if detail.contains("ARTICLE_SUMMARY_ACTIVE") {
            return Self::new(
                DeleteFailureKind::ArticleSummaryActive,
                "该文章正在进行 AI 总结，请等待完成后再删除",
                detail,
            );
        }
        Self::new(DeleteFailureKind::Storage, "删除网页收藏失败", detail)
    }
}

#[derive(Debug, Clone)]
struct FetchRequest {
    url: String,
}

#[derive(Debug, Clone)]
struct FetchedPage {
    original_url: String,
    final_url: String,
    html: String,
}

trait WebPageFetch: Send + Sync {
    fn fetch(
        &self,
        request: FetchRequest,
        cancellation: &AtomicBool,
    ) -> std::result::Result<FetchedPage, CaptureFailure>;
}

struct ProductionHttpFetch;

impl WebPageFetch for ProductionHttpFetch {
    fn fetch(
        &self,
        request: FetchRequest,
        cancellation: &AtomicBool,
    ) -> std::result::Result<FetchedPage, CaptureFailure> {
        if cancellation.load(Ordering::Acquire) {
            return Err(CaptureFailure::cancelled("CANCELLED_BEFORE_FETCH"));
        }
        let client = crate::web_clip::client().map_err(classify_fetch_error)?;
        let fetched =
            crate::web_clip::fetch_html(&client, &request.url).map_err(classify_fetch_error)?;
        if cancellation.load(Ordering::Acquire) {
            return Err(CaptureFailure::cancelled("CANCELLED_AFTER_FETCH"));
        }
        Ok(FetchedPage {
            original_url: fetched.original_url,
            final_url: fetched.final_url,
            html: fetched.html,
        })
    }
}

fn classify_fetch_error(error: impl std::fmt::Display) -> CaptureFailure {
    let detail = error.to_string();
    let lower = detail.to_ascii_lowercase();
    if lower.contains("localhost")
        || lower.contains("private")
        || detail.contains("内网")
        || detail.contains("本机")
        || detail.contains("用户名")
    {
        CaptureFailure::new(
            CaptureFailureKind::Security,
            "为保护本机资料，已拒绝访问该网页地址",
            detail,
            false,
        )
    } else if lower.contains("content-type") || detail.contains("HTML") || detail.contains("8 MiB")
    {
        CaptureFailure::new(
            CaptureFailureKind::Content,
            "该地址没有返回可保存的 HTML 网页",
            detail,
            false,
        )
    } else {
        CaptureFailure::new(CaptureFailureKind::Network, "抓取网页失败", detail, true)
    }
}

enum CaptureInput {
    Url(String),
    Html(String),
}

struct CaptureSlot {
    snapshot: Mutex<CaptureSnapshot>,
}

struct ActiveCapture {
    slot: Arc<CaptureSlot>,
    cancellation: Arc<AtomicBool>,
    generation: u64,
}

struct LifecycleControl {
    quiesced: bool,
    generation: u64,
    next_id: u64,
    active: Option<ActiveCapture>,
    recent_terminal: Option<CaptureSnapshot>,
}

struct LifecycleInner {
    database: PathBuf,
    fetch: Arc<dyn WebPageFetch>,
    control: Mutex<LifecycleControl>,
    changed: Condvar,
}

pub(crate) struct WebClippingLifecycle {
    inner: Arc<LifecycleInner>,
}

pub(crate) struct CaptureLease {
    inner: Arc<LifecycleInner>,
    slot: Arc<CaptureSlot>,
}

impl std::fmt::Debug for CaptureLease {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CaptureLease")
            .field("snapshot", &self.snapshot())
            .finish()
    }
}

impl CaptureLease {
    pub(crate) fn id(&self) -> CaptureId {
        self.slot
            .snapshot
            .lock()
            .expect("capture snapshot poisoned")
            .id
    }

    pub(crate) fn snapshot(&self) -> CaptureSnapshot {
        self.slot
            .snapshot
            .lock()
            .expect("capture snapshot poisoned")
            .clone()
    }

    pub(crate) fn request_cancel(&self) -> CancelDisposition {
        request_cancel(&self.inner, self.id())
    }
}

impl Drop for CaptureLease {
    fn drop(&mut self) {
        let _ = request_cancel(&self.inner, self.id());
    }
}

impl WebClippingLifecycle {
    pub(crate) fn start(database: PathBuf) -> Self {
        Self::with_fetch(database, Arc::new(ProductionHttpFetch))
    }

    fn with_fetch(database: PathBuf, fetch: Arc<dyn WebPageFetch>) -> Self {
        Self {
            inner: Arc::new(LifecycleInner {
                database,
                fetch,
                control: Mutex::new(LifecycleControl {
                    quiesced: false,
                    generation: 1,
                    next_id: 1,
                    active: None,
                    recent_terminal: None,
                }),
                changed: Condvar::new(),
            }),
        }
    }

    pub(crate) fn begin_capture(
        &self,
        request: CaptureRequest,
    ) -> std::result::Result<CaptureLease, BeginCaptureFailure> {
        let input = classify_input(&request.source).map_err(BeginCaptureFailure::from_failure)?;
        validate_capture_request(&request, &input).map_err(BeginCaptureFailure::from_failure)?;
        if MaintenanceFence::observe(&self.inner.database)
            .map(|availability| availability.is_active())
            .unwrap_or(true)
        {
            return Err(BeginCaptureFailure::from_failure(
                CaptureFailure::maintenance("MAINTENANCE_IN_PROGRESS"),
            ));
        }

        let (slot, cancellation, generation) = {
            let mut control = self.inner.control.lock().expect("capture control poisoned");
            if control.quiesced {
                return Err(BeginCaptureFailure::from_failure(
                    CaptureFailure::maintenance("CAPTURE_LIFECYCLE_QUIESCED"),
                ));
            }
            if let Some(active) = &control.active {
                let active_id = active
                    .slot
                    .snapshot
                    .lock()
                    .expect("capture snapshot poisoned")
                    .id;
                return Err(BeginCaptureFailure::from_failure(CaptureFailure::new(
                    CaptureFailureKind::Input,
                    "已有一个网页正在保存",
                    format!("ACTIVE_CAPTURE: {}", active_id.value()),
                    false,
                )));
            }
            let id = CaptureId(control.next_id);
            control.next_id = control.next_id.wrapping_add(1).max(1);
            let state = match input {
                CaptureInput::Url(_) => CaptureState::Fetching,
                CaptureInput::Html(_) => CaptureState::Preparing,
            };
            let slot = Arc::new(CaptureSlot {
                snapshot: Mutex::new(CaptureSnapshot {
                    id,
                    revision: 1,
                    state,
                }),
            });
            let cancellation = Arc::new(AtomicBool::new(false));
            let generation = control.generation;
            control.active = Some(ActiveCapture {
                slot: Arc::clone(&slot),
                cancellation: Arc::clone(&cancellation),
                generation,
            });
            (slot, cancellation, generation)
        };

        let inner = Arc::clone(&self.inner);
        let slot_for_worker = Arc::clone(&slot);
        std::thread::Builder::new()
            .name(format!(
                "shiyue-web-clipping-{}",
                slot_for_worker
                    .snapshot
                    .lock()
                    .expect("capture snapshot poisoned")
                    .id
                    .value()
            ))
            .spawn(move || {
                run_capture(
                    inner,
                    slot_for_worker,
                    cancellation,
                    generation,
                    input,
                    request,
                )
            })
            .map_err(|error| {
                let failure = CaptureFailure::storage(format!("SPAWN_CAPTURE_WORKER: {error}"));
                publish_terminal(&self.inner, &slot, CaptureState::Failed(failure.clone()));
                BeginCaptureFailure::from_failure(failure)
            })?;

        Ok(CaptureLease {
            inner: Arc::clone(&self.inner),
            slot,
        })
    }

    pub(crate) fn recent_terminal(&self) -> Option<CaptureSnapshot> {
        self.inner
            .control
            .lock()
            .expect("capture control poisoned")
            .recent_terminal
            .clone()
    }

    pub(crate) fn delete(
        &self,
        request: DeleteRequest,
    ) -> std::result::Result<DeleteOutcome, DeleteFailure> {
        if request.article_id <= 0 {
            return Err(DeleteFailure::new(
                DeleteFailureKind::Input,
                "文章标识无效",
                format!("INVALID_ARTICLE_ID: {}", request.article_id),
            ));
        }
        {
            let control = self.inner.control.lock().expect("capture control poisoned");
            if control.quiesced {
                return Err(DeleteFailure::new(
                    DeleteFailureKind::Maintenance,
                    "资料维护期间不能删除网页收藏",
                    "CAPTURE_LIFECYCLE_QUIESCED",
                ));
            }
        }
        delete_clipping(&self.inner.database, request)
    }

    pub(crate) fn maintenance_participant(&self) -> Arc<dyn MaintenanceParticipant> {
        Arc::new(WebClippingMaintenance {
            inner: Arc::clone(&self.inner),
        })
    }
}

struct WebClippingMaintenance {
    inner: Arc<LifecycleInner>,
}

impl MaintenanceParticipant for WebClippingMaintenance {
    fn name(&self) -> &'static str {
        "web_clipping_lifecycle"
    }

    fn quiesce(&self, deadline: Instant, _epoch: &str) -> Result<()> {
        let mut control = self.inner.control.lock().expect("capture control poisoned");
        control.quiesced = true;
        control.generation = control.generation.wrapping_add(1);
        if let Some(active) = &control.active {
            active.cancellation.store(true, Ordering::Release);
            let slot = Arc::clone(&active.slot);
            let state = slot
                .snapshot
                .lock()
                .expect("capture snapshot poisoned")
                .state
                .clone();
            if matches!(state, CaptureState::Fetching | CaptureState::Preparing) {
                finish_terminal_locked(
                    &mut control,
                    &slot,
                    CaptureState::Cancelled(CaptureFailure::maintenance(
                        "CAPTURE_INTERRUPTED_BY_MAINTENANCE",
                    )),
                );
                self.inner.changed.notify_all();
            }
        }
        while control.active.is_some() {
            let now = Instant::now();
            if now >= deadline {
                return Err(anyhow!("WEB_CLIPPING_COMMIT_DRAIN_TIMEOUT"));
            }
            let timeout = deadline.saturating_duration_since(now);
            let (next, waited) = self
                .inner
                .changed
                .wait_timeout(control, timeout)
                .expect("capture control poisoned");
            control = next;
            if waited.timed_out() && control.active.is_some() {
                return Err(anyhow!("WEB_CLIPPING_COMMIT_DRAIN_TIMEOUT"));
            }
        }
        Ok(())
    }

    fn resume(&self, _epoch: &str) -> Result<()> {
        let mut control = self.inner.control.lock().expect("capture control poisoned");
        control.quiesced = false;
        Ok(())
    }
}

fn classify_input(source: &str) -> std::result::Result<CaptureInput, CaptureFailure> {
    let source = source.trim();
    if source.is_empty() {
        return Err(CaptureFailure::new(
            CaptureFailureKind::Input,
            "请粘贴网页地址或 HTML",
            "EMPTY_CAPTURE_SOURCE",
            false,
        ));
    }
    if source.starts_with('<') {
        return Ok(CaptureInput::Html(source.to_owned()));
    }
    if let Some(url) = normalized_web_url(source) {
        return Ok(CaptureInput::Url(url));
    }
    Err(CaptureFailure::new(
        CaptureFailureKind::Input,
        "网页地址格式不正确，或粘贴的不是 HTML",
        "UNRECOGNIZED_CAPTURE_INPUT",
        false,
    ))
}

fn validate_capture_request(
    request: &CaptureRequest,
    input: &CaptureInput,
) -> std::result::Result<(), CaptureFailure> {
    match input {
        CaptureInput::Url(url) if url.len() > MAX_INPUT_URL_BYTES => Err(CaptureFailure::new(
            CaptureFailureKind::Input,
            "网页地址过长",
            "CAPTURE_URL_TOO_LONG",
            false,
        )),
        CaptureInput::Html(html) if html.len() > crate::web_clip::MAX_HTML_BYTES => {
            Err(CaptureFailure::new(
                CaptureFailureKind::Content,
                "HTML 超过 8 MiB，未保存",
                "PASTED_HTML_TOO_LARGE",
                false,
            ))
        }
        _ => {
            if request
                .title_override
                .as_deref()
                .is_some_and(|title| title.len() > 4096)
            {
                return Err(CaptureFailure::new(
                    CaptureFailureKind::Input,
                    "标题过长",
                    "CAPTURE_TITLE_TOO_LONG",
                    false,
                ));
            }
            Ok(())
        }
    }
}

fn normalized_web_url(value: &str) -> Option<String> {
    let value = value.trim();
    if let Ok(url) = Url::parse(value)
        && matches!(url.scheme(), "http" | "https")
    {
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
    Url::parse(&candidate)
        .ok()
        .filter(|url| matches!(url.scheme(), "http" | "https"))
        .map(|_| candidate)
}

fn run_capture(
    inner: Arc<LifecycleInner>,
    slot: Arc<CaptureSlot>,
    cancellation: Arc<AtomicBool>,
    generation: u64,
    input: CaptureInput,
    request: CaptureRequest,
) {
    let result = (|| {
        let prepared = match input {
            CaptureInput::Url(url) => {
                let fetched = inner.fetch.fetch(FetchRequest { url }, &cancellation)?;
                if !transition_stage(&inner, &slot, generation, CaptureState::Preparing) {
                    return Err(CaptureFailure::cancelled("CAPTURE_NO_LONGER_ACTIVE"));
                }
                prepare_fetched(fetched, request.title_override.as_deref())?
            }
            CaptureInput::Html(html) => prepare_pasted(
                html,
                request.title_override.as_deref(),
                request.pasted_html_base_url.as_deref(),
            )?,
        };
        let mut db = Db::open(&inner.database).map_err(CaptureFailure::storage)?;
        let library_generation = db.library_generation();
        let tx = db
            .fenced_linearized_immediate_transaction()
            .map_err(CaptureFailure::storage)?;
        if !enter_committing(&inner, &slot, generation) {
            return Err(CaptureFailure::cancelled("CANCELLED_BEFORE_COMMIT"));
        }
        commit_capture(
            tx,
            library_generation,
            prepared,
            request.refresh_scope,
            Utc::now().timestamp(),
        )
    })();

    match result {
        Ok(success) => publish_terminal(&inner, &slot, CaptureState::Succeeded(Box::new(success))),
        Err(failure) => {
            let state = if failure.kind == CaptureFailureKind::Cancelled
                || failure.kind == CaptureFailureKind::Maintenance
            {
                CaptureState::Cancelled(failure)
            } else {
                CaptureState::Failed(failure)
            };
            publish_terminal(&inner, &slot, state);
        }
    }
}

struct PreparedCapture {
    title: String,
    content: String,
    article_url: Option<String>,
    provenance: CaptureProvenance,
    base_url: Option<String>,
}

fn prepare_fetched(
    fetched: FetchedPage,
    title_override: Option<&str>,
) -> std::result::Result<PreparedCapture, CaptureFailure> {
    let snapshot = prepare_article_html(&fetched.html);
    if snapshot.content.trim().is_empty() {
        return Err(CaptureFailure::new(
            CaptureFailureKind::Content,
            "网页抓取成功，但没有识别到可阅读正文",
            "EMPTY_PREPARED_CONTENT",
            false,
        ));
    }
    let title = non_empty(title_override)
        .or(snapshot.title)
        .unwrap_or_else(|| fetched.original_url.clone());
    let base_url = snapshot
        .base_href
        .as_deref()
        .and_then(|base| resolve_http_url(base, Some(&fetched.final_url)))
        .or_else(|| Some(fetched.final_url.clone()));
    let content = with_html_base(&snapshot.content, base_url.as_deref());
    Ok(PreparedCapture {
        title,
        content,
        article_url: Some(fetched.original_url.clone()),
        provenance: CaptureProvenance::Fetched {
            original_url: fetched.original_url,
            final_url: fetched.final_url,
        },
        base_url,
    })
}

fn prepare_pasted(
    html: String,
    title_override: Option<&str>,
    explicit_base: Option<&str>,
) -> std::result::Result<PreparedCapture, CaptureFailure> {
    let snapshot = prepare_article_html(&html);
    if snapshot.content.trim().is_empty() {
        return Err(CaptureFailure::new(
            CaptureFailureKind::Content,
            "HTML 中没有识别到可阅读正文",
            "EMPTY_PREPARED_CONTENT",
            false,
        ));
    }
    let base_url = explicit_base
        .and_then(|value| non_empty(Some(value)))
        .map(|base| {
            resolve_http_url(&base, None).ok_or_else(|| {
                CaptureFailure::new(
                    CaptureFailureKind::Input,
                    "基础网址必须是 http:// 或 https:// 地址",
                    "INVALID_PASTED_HTML_BASE_URL",
                    false,
                )
            })
        })
        .transpose()?
        .or_else(|| {
            snapshot
                .base_href
                .as_deref()
                .and_then(|value| resolve_http_url(value, None))
        });
    let title = non_empty(title_override)
        .or(snapshot.title)
        .unwrap_or_else(|| "未命名网页".to_owned());
    Ok(PreparedCapture {
        title,
        content: with_html_base(&snapshot.content, base_url.as_deref()),
        article_url: None,
        provenance: CaptureProvenance::PastedHtml {
            base_url: base_url.clone(),
        },
        base_url,
    })
}

fn non_empty(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn resolve_http_url(value: &str, document_url: Option<&str>) -> Option<String> {
    if let Ok(url) = Url::parse(value.trim()) {
        return matches!(url.scheme(), "http" | "https").then(|| url.to_string());
    }
    let document = Url::parse(document_url?).ok()?;
    let joined = document.join(value.trim()).ok()?;
    matches!(joined.scheme(), "http" | "https").then(|| joined.to_string())
}

fn with_html_base(content: &str, base: Option<&str>) -> String {
    match base.map(str::trim).filter(|value| !value.is_empty()) {
        Some(base) => format!(
            "<base href=\"{}\">\n{}",
            base.replace('&', "&amp;")
                .replace('"', "&quot;")
                .replace('<', "&lt;")
                .replace('>', "&gt;"),
            content
        ),
        None => content.to_owned(),
    }
}

fn transition_stage(
    inner: &LifecycleInner,
    slot: &Arc<CaptureSlot>,
    generation: u64,
    state: CaptureState,
) -> bool {
    let control = inner.control.lock().expect("capture control poisoned");
    let Some(active) = &control.active else {
        return false;
    };
    if !Arc::ptr_eq(&active.slot, slot)
        || active.generation != generation
        || control.generation != generation
        || control.quiesced
        || active.cancellation.load(Ordering::Acquire)
    {
        return false;
    }
    let mut snapshot = slot.snapshot.lock().expect("capture snapshot poisoned");
    snapshot.revision = snapshot.revision.wrapping_add(1);
    snapshot.state = state;
    true
}

fn enter_committing(inner: &LifecycleInner, slot: &Arc<CaptureSlot>, generation: u64) -> bool {
    transition_stage(inner, slot, generation, CaptureState::Committing)
}

fn request_cancel(inner: &LifecycleInner, id: CaptureId) -> CancelDisposition {
    let mut control = inner.control.lock().expect("capture control poisoned");
    let Some(active) = &control.active else {
        return CancelDisposition::AlreadyTerminal;
    };
    let slot = Arc::clone(&active.slot);
    let mut snapshot = slot.snapshot.lock().expect("capture snapshot poisoned");
    if snapshot.id != id {
        return CancelDisposition::AlreadyTerminal;
    }
    match snapshot.state {
        CaptureState::Committing => CancelDisposition::CommitAlreadyStarted,
        CaptureState::Fetching | CaptureState::Preparing => {
            active.cancellation.store(true, Ordering::Release);
            snapshot.revision = snapshot.revision.wrapping_add(1);
            snapshot.state =
                CaptureState::Cancelled(CaptureFailure::cancelled("CANCELLED_BY_CALLER"));
            let terminal = snapshot.clone();
            drop(snapshot);
            control.recent_terminal = Some(terminal);
            control.active = None;
            inner.changed.notify_all();
            CancelDisposition::CancelledBeforeCommit
        }
        CaptureState::Succeeded(_) | CaptureState::Failed(_) | CaptureState::Cancelled(_) => {
            CancelDisposition::AlreadyTerminal
        }
    }
}

fn publish_terminal(inner: &LifecycleInner, slot: &Arc<CaptureSlot>, state: CaptureState) {
    let mut control = inner.control.lock().expect("capture control poisoned");
    let Some(active) = &control.active else {
        return;
    };
    if !Arc::ptr_eq(&active.slot, slot) {
        return;
    }
    finish_terminal_locked(&mut control, slot, state);
    inner.changed.notify_all();
}

fn finish_terminal_locked(
    control: &mut LifecycleControl,
    slot: &Arc<CaptureSlot>,
    state: CaptureState,
) {
    let mut snapshot = slot.snapshot.lock().expect("capture snapshot poisoned");
    if snapshot.state.is_terminal() {
        return;
    }
    snapshot.revision = snapshot.revision.wrapping_add(1);
    snapshot.state = state;
    control.recent_terminal = Some(snapshot.clone());
    control.active = None;
}

fn commit_capture(
    tx: FencedTransaction<'_>,
    generation: LibraryGeneration,
    prepared: PreparedCapture,
    refresh_scope: ProjectionScope,
    now: i64,
) -> std::result::Result<CaptureSuccess, CaptureFailure> {
    let feed_id = ensure_web_clippings_feed_on(&tx, now).map_err(CaptureFailure::storage)?;
    tx.execute(
        "INSERT INTO articles
         (feed_id,entry_id,url,title,author,published,content,is_read,starred,archived,fetched_at)
         VALUES(?1,'clip:' || lower(hex(randomblob(16))),?2,?3,?4,?5,?6,1,1,0,?5)",
        params![
            feed_id,
            prepared.article_url,
            prepared.title,
            WEB_CLIPPINGS_FEED_TITLE,
            now,
            prepared.content
        ],
    )
    .map_err(CaptureFailure::storage)?;
    let article_id = tx.last_insert_rowid();
    let (input_kind, original_url, final_url) = match &prepared.provenance {
        CaptureProvenance::Fetched {
            original_url,
            final_url,
        } => (
            "fetched_url",
            Some(original_url.as_str()),
            Some(final_url.as_str()),
        ),
        CaptureProvenance::PastedHtml { .. } => ("pasted_html", None, None),
    };
    tx.execute(
        "INSERT INTO web_clippings
         (article_id,input_kind,original_url,final_url,base_url,captured_at,provenance_state)
         VALUES(?1,?2,?3,?4,?5,?6,'complete')",
        params![
            article_id,
            input_kind,
            original_url,
            final_url,
            prepared.base_url,
            now
        ],
    )
    .map_err(CaptureFailure::storage)?;
    let revisions = library_projection_revision::record(&tx, ProjectionImpact::article())
        .map_err(CaptureFailure::storage)?;
    let projection = project_on(
        &tx,
        refresh_scope,
        ProjectionStamp {
            generation,
            revision: revisions.article,
        },
    )
    .map_err(CaptureFailure::storage)?;
    tx.commit().map_err(CaptureFailure::storage)?;
    Ok(CaptureSuccess {
        clipping: WebClippingIdentity {
            article_id,
            title: prepared.title,
        },
        provenance: prepared.provenance,
        projection,
    })
}

fn ensure_web_clippings_feed_on(conn: &Connection, now: i64) -> Result<i64> {
    conn.execute(
        "INSERT INTO feeds(url,title,next_fetch,disabled)
         VALUES(?1,?2,?3,1)
         ON CONFLICT(url) DO UPDATE SET
           title=excluded.title,disabled=1,last_error=NULL,fail_count=0",
        params![WEB_CLIPPINGS_FEED_URL, WEB_CLIPPINGS_FEED_TITLE, now],
    )?;
    Ok(conn.query_row(
        "SELECT id FROM feeds WHERE url=?1",
        [WEB_CLIPPINGS_FEED_URL],
        |row| row.get(0),
    )?)
}

fn delete_clipping(
    database: &Path,
    request: DeleteRequest,
) -> std::result::Result<DeleteOutcome, DeleteFailure> {
    let mut db = Db::open(database).map_err(DeleteFailure::storage)?;
    let generation = db.library_generation();
    let tx = db
        .fenced_immediate_transaction()
        .map_err(DeleteFailure::storage)?;
    let identity = tx
        .query_row(
            "SELECT a.id,COALESCE(a.title,'未命名网页')
             FROM articles a JOIN web_clippings w ON w.article_id=a.id
             WHERE a.id=?1",
            [request.article_id],
            |row| {
                Ok(WebClippingIdentity {
                    article_id: row.get(0)?,
                    title: row.get(1)?,
                })
            },
        )
        .optional()
        .map_err(DeleteFailure::storage)?
        .ok_or_else(|| {
            DeleteFailure::new(
                DeleteFailureKind::NotFound,
                "网页收藏不存在或已经删除",
                format!("WEB_CLIPPING_NOT_FOUND: {}", request.article_id),
            )
        })?;
    crate::knowledge_workflow::article_target::prepare_delete(&tx, request.article_id)
        .map_err(DeleteFailure::storage)?;
    let mut statement = tx
        .prepare("SELECT id FROM resources WHERE linked_article_id=?1 ORDER BY id")
        .map_err(DeleteFailure::storage)?;
    let detached_resource_ids = statement
        .query_map([request.article_id], |row| row.get(0))
        .map_err(DeleteFailure::storage)?
        .collect::<rusqlite::Result<Vec<i64>>>()
        .map_err(DeleteFailure::storage)?;
    drop(statement);
    let affects_excerpt: bool = tx
        .query_row(
            "SELECT EXISTS(
               SELECT 1 FROM article_selections
               WHERE article_id=?1
                 AND (is_favorite=1 OR (comment IS NOT NULL AND length(trim(comment))>0))
             )",
            [request.article_id],
            |row| row.get(0),
        )
        .map_err(DeleteFailure::storage)?;
    tx.execute("DELETE FROM articles WHERE id=?1", [request.article_id])
        .map_err(DeleteFailure::storage)?;
    let mut impact = ProjectionImpact::article();
    if !detached_resource_ids.is_empty() {
        impact = impact.with(ProjectionFamily::Resource);
    }
    if affects_excerpt {
        impact = impact.with(ProjectionFamily::Excerpt);
    }
    let revisions =
        library_projection_revision::record(&tx, impact).map_err(DeleteFailure::storage)?;
    let projection = project_on(
        &tx,
        request.refresh_scope,
        ProjectionStamp {
            generation,
            revision: revisions.article,
        },
    )
    .map_err(DeleteFailure::storage)?;
    tx.commit().map_err(DeleteFailure::storage)?;
    Ok(DeleteOutcome {
        deleted: identity,
        detached_resource_ids,
        projection,
    })
}

pub(crate) fn migrate_to_v6(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS web_clippings (
           article_id       INTEGER PRIMARY KEY REFERENCES articles(id) ON DELETE CASCADE,
           input_kind       TEXT NOT NULL CHECK (input_kind IN ('fetched_url','pasted_html')),
           original_url     TEXT,
           final_url        TEXT,
           base_url         TEXT,
           captured_at      INTEGER NOT NULL,
           provenance_state TEXT NOT NULL DEFAULT 'complete'
                            CHECK (provenance_state IN ('complete','legacy')),
           CHECK (
             (input_kind='fetched_url' AND original_url IS NOT NULL AND
               (final_url IS NOT NULL OR provenance_state='legacy'))
             OR
             (input_kind='pasted_html' AND original_url IS NULL AND final_url IS NULL)
           )
         );",
    )?;
    conn.execute(
        "INSERT OR IGNORE INTO web_clippings
         (article_id,input_kind,original_url,final_url,base_url,captured_at,provenance_state)
         SELECT a.id,
                CASE WHEN a.url IS NULL THEN 'pasted_html' ELSE 'fetched_url' END,
                a.url,NULL,NULL,COALESCE(a.published,a.fetched_at),'legacy'
         FROM articles a JOIN feeds f ON f.id=a.feed_id
         WHERE f.url=?1",
        [WEB_CLIPPINGS_FEED_URL],
    )?;
    Ok(())
}

pub(crate) fn verify_schema_v6(conn: &Connection) -> Result<()> {
    let mut statement = conn.prepare("PRAGMA table_info(web_clippings)")?;
    let columns = statement
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<std::collections::HashSet<_>>>()?;
    for column in [
        "article_id",
        "input_kind",
        "original_url",
        "final_url",
        "base_url",
        "captured_at",
        "provenance_state",
    ] {
        anyhow::ensure!(
            columns.contains(column),
            "WEB_CLIPPING_SCHEMA_COLUMN_MISSING: {column}"
        );
    }
    let missing_provenance: i64 = conn.query_row(
        "SELECT COUNT(*)
         FROM articles a JOIN feeds f ON f.id=a.feed_id
         LEFT JOIN web_clippings w ON w.article_id=a.id
         WHERE f.url=?1 AND w.article_id IS NULL",
        [WEB_CLIPPINGS_FEED_URL],
        |row| row.get(0),
    )?;
    anyhow::ensure!(
        missing_provenance == 0,
        "WEB_CLIPPING_PROVENANCE_MISSING: {missing_provenance} articles"
    );
    Ok(())
}

fn bounded_detail(detail: impl std::fmt::Display) -> String {
    let detail = detail.to_string().replace(['\r', '\n'], " ");
    detail.chars().take(MAX_TECHNICAL_DETAIL_CHARS).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    struct FixedFetch {
        page: FetchedPage,
    }

    impl WebPageFetch for FixedFetch {
        fn fetch(
            &self,
            _request: FetchRequest,
            _cancellation: &AtomicBool,
        ) -> std::result::Result<FetchedPage, CaptureFailure> {
            Ok(self.page.clone())
        }
    }

    struct BlockingFetch {
        started: mpsc::Sender<()>,
        release: Mutex<mpsc::Receiver<()>>,
    }

    impl WebPageFetch for BlockingFetch {
        fn fetch(
            &self,
            _request: FetchRequest,
            cancellation: &AtomicBool,
        ) -> std::result::Result<FetchedPage, CaptureFailure> {
            let _ = self.started.send(());
            let _ = self
                .release
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(2));
            if cancellation.load(Ordering::Acquire) {
                return Err(CaptureFailure::cancelled("TEST_CANCELLED"));
            }
            Ok(FetchedPage {
                original_url: "https://example.com/a".into(),
                final_url: "https://example.com/final".into(),
                html: "<article><h1>A</h1><p>body</p></article>".into(),
            })
        }
    }

    fn temp_database(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "rrss-web-clipping-{name}-{}-{}.sqlite3",
            std::process::id(),
            Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        Db::open(&path).unwrap();
        path
    }

    fn wait_terminal(lease: &CaptureLease) -> CaptureSnapshot {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let snapshot = lease.snapshot();
            if snapshot.state.is_terminal() {
                return snapshot;
            }
            assert!(Instant::now() < deadline, "capture did not finish");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn repeated_url_captures_are_distinct_and_preserve_provenance() {
        let path = temp_database("repeated");
        let lifecycle = WebClippingLifecycle::with_fetch(
            path.clone(),
            Arc::new(FixedFetch {
                page: FetchedPage {
                    original_url: "https://example.com/a".into(),
                    final_url: "https://cdn.example.com/final".into(),
                    html: "<html><head><title>Page</title></head><body><article><p>body</p></article></body></html>".into(),
                },
            }),
        );
        let request = || CaptureRequest {
            source: "https://example.com/a".into(),
            title_override: None,
            pasted_html_base_url: None,
            refresh_scope: ProjectionScope::ArticleBookmarks,
        };
        let first = wait_terminal(&lifecycle.begin_capture(request()).unwrap());
        let second = wait_terminal(&lifecycle.begin_capture(request()).unwrap());
        let ids = [first, second].map(|snapshot| match snapshot.state {
            CaptureState::Succeeded(success) => success.clipping.article_id,
            state => panic!("unexpected state: {state:?}"),
        });
        assert_ne!(ids[0], ids[1]);
        let db = Db::open(&path).unwrap();
        let rows: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM web_clippings
                 WHERE original_url='https://example.com/a'
                   AND final_url='https://cdn.example.com/final'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(rows, 2);
    }

    #[test]
    fn cancellation_before_commit_guarantees_zero_writes() {
        let path = temp_database("cancel");
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let lifecycle = WebClippingLifecycle::with_fetch(
            path.clone(),
            Arc::new(BlockingFetch {
                started: started_tx,
                release: Mutex::new(release_rx),
            }),
        );
        let lease = lifecycle
            .begin_capture(CaptureRequest {
                source: "https://example.com/a".into(),
                title_override: None,
                pasted_html_base_url: None,
                refresh_scope: ProjectionScope::ArticleBookmarks,
            })
            .unwrap();
        started_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(
            lease.request_cancel(),
            CancelDisposition::CancelledBeforeCommit
        );
        let _ = release_tx.send(());
        assert!(matches!(
            wait_terminal(&lease).state,
            CaptureState::Cancelled(_)
        ));
        let db = Db::open(&path).unwrap();
        let count: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM web_clippings", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn pasted_html_persists_base_and_title_precedence() {
        let path = temp_database("pasted");
        let lifecycle = WebClippingLifecycle::start(path.clone());
        let lease = lifecycle
            .begin_capture(CaptureRequest {
                source: "<html><head><title>Page title</title></head><body><main><p>body</p></main></body></html>".into(),
                title_override: Some("User title".into()),
                pasted_html_base_url: Some("https://example.com/base/".into()),
                refresh_scope: ProjectionScope::ArticleBookmarks,
            })
            .unwrap();
        let success = match wait_terminal(&lease).state {
            CaptureState::Succeeded(success) => success,
            state => panic!("unexpected state: {state:?}"),
        };
        assert_eq!(success.clipping.title, "User title");
        assert_eq!(
            success.provenance,
            CaptureProvenance::PastedHtml {
                base_url: Some("https://example.com/base/".into())
            }
        );
    }

    #[test]
    fn delete_rejects_active_summary_then_cascades_material_and_detaches_resource() {
        let path = temp_database("delete");
        let lifecycle = WebClippingLifecycle::start(path.clone());
        let success = match wait_terminal(
            &lifecycle
                .begin_capture(CaptureRequest {
                    source: "<article><h1>Saved</h1><p>body</p></article>".into(),
                    title_override: None,
                    pasted_html_base_url: None,
                    refresh_scope: ProjectionScope::ArticleBookmarks,
                })
                .unwrap(),
        )
        .state
        {
            CaptureState::Succeeded(success) => success,
            state => panic!("unexpected state: {state:?}"),
        };
        let article_id = success.clipping.article_id;
        assert!(success.projection.stamp.revision > 0);
        let db = Db::open(&path).unwrap();
        db.conn
            .execute(
                "INSERT INTO article_selections(article_id,selected_text,comment,is_favorite,created_at,updated_at)
                 VALUES(?1,'quote','thought',1,0,0)",
                [article_id],
            )
            .unwrap();
        db.conn
            .execute(
                "INSERT INTO resources(url,canonical_url,linked_article_id,kind,title,
                  capabilities,limitations,languages,privacy,status,source,created_at,updated_at)
                 VALUES('https://example.com','https://example.com',?1,'page','resource',
                  '[]','[]','[]','public','active','import',0,0)",
                [article_id],
            )
            .unwrap();
        let resource_id = db.conn.last_insert_rowid();
        db.conn
            .execute(
                "INSERT INTO knowledge_tasks(kind,target_id,status,created_at,updated_at)
                 VALUES('article_summary',?1,'running',0,0)",
                [article_id],
            )
            .unwrap();
        drop(db);

        let request = || DeleteRequest {
            article_id,
            refresh_scope: ProjectionScope::ArticleBookmarks,
        };
        let failure = lifecycle.delete(request()).unwrap_err();
        assert_eq!(failure.kind, DeleteFailureKind::ArticleSummaryActive);

        let db = Db::open(&path).unwrap();
        db.conn
            .execute(
                "UPDATE knowledge_tasks SET status='failed' WHERE target_id=?1",
                [article_id],
            )
            .unwrap();
        let before_delete = library_projection_revision::read(&db.conn).unwrap();
        drop(db);
        let outcome = lifecycle.delete(request()).unwrap();
        assert_eq!(outcome.deleted.article_id, article_id);
        assert_eq!(outcome.detached_resource_ids, vec![resource_id]);
        assert!(outcome.projection.articles.is_empty());

        let db = Db::open(&path).unwrap();
        let facts: (i64, i64, i64, Option<i64>) = db
            .conn
            .query_row(
                "SELECT
                   (SELECT COUNT(*) FROM articles WHERE id=?1),
                   (SELECT COUNT(*) FROM article_selections WHERE article_id=?1),
                   (SELECT COUNT(*) FROM knowledge_tasks WHERE target_id=?1),
                   (SELECT linked_article_id FROM resources WHERE id=?2)",
                params![article_id, resource_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!(facts, (0, 0, 0, None));
        let after_delete = library_projection_revision::read(&db.conn).unwrap();
        assert_eq!(after_delete.article, before_delete.article + 1);
        assert_eq!(after_delete.resource, before_delete.resource + 1);
        assert_eq!(after_delete.excerpt, before_delete.excerpt + 1);
    }

    #[test]
    fn maintenance_cancels_fetching_capture_without_waiting_for_network_thread() {
        let path = temp_database("maintenance");
        let (started_tx, started_rx) = mpsc::channel();
        let (_release_tx, release_rx) = mpsc::channel();
        let lifecycle = WebClippingLifecycle::with_fetch(
            path,
            Arc::new(BlockingFetch {
                started: started_tx,
                release: Mutex::new(release_rx),
            }),
        );
        let lease = lifecycle
            .begin_capture(CaptureRequest {
                source: "https://example.com/a".into(),
                title_override: None,
                pasted_html_base_url: None,
                refresh_scope: ProjectionScope::ArticleBookmarks,
            })
            .unwrap();
        started_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        lifecycle
            .maintenance_participant()
            .quiesce(Instant::now() + Duration::from_secs(1), "next")
            .unwrap();
        assert!(matches!(lease.snapshot().state, CaptureState::Cancelled(_)));
    }

    #[test]
    fn schema_v5_migration_backfills_only_recoverable_legacy_provenance() {
        let path = std::env::temp_dir().join(format!(
            "rrss-web-clipping-migration-{}-{}.sqlite3",
            std::process::id(),
            Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        {
            let conn = Connection::open(&path).unwrap();
            conn.pragma_update(None, "foreign_keys", "ON").unwrap();
            crate::schema_evolution::evolve_fixture_to(&conn, 5).unwrap();
            conn.execute(
                "INSERT INTO feeds(url,title,next_fetch,disabled)
                 VALUES(?1,'clips',0,1)",
                [WEB_CLIPPINGS_FEED_URL],
            )
            .unwrap();
            let feed_id = conn.last_insert_rowid();
            conn.execute(
                "INSERT INTO articles(feed_id,entry_id,url,title,content,fetched_at)
                 VALUES(?1,'legacy','https://example.com/original','legacy','<p>x</p>',7)",
                [feed_id],
            )
            .unwrap();
        }
        let db = Db::open(&path).unwrap();
        let row: (String, Option<String>, Option<String>, String) = db
            .conn
            .query_row(
                "SELECT input_kind,original_url,final_url,provenance_state FROM web_clippings",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!(
            row,
            (
                "fetched_url".into(),
                Some("https://example.com/original".into()),
                None,
                "legacy".into()
            )
        );
        let version: i64 = db
            .conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, crate::schema_evolution::CURRENT_SCHEMA_VERSION);
    }
}
