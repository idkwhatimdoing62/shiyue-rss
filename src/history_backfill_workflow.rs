//! Session-bound historical import with automatic archive discovery.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use anyhow::{Result, anyhow};
use chrono::Utc;

use crate::article_document_presentation::prepare_article_html;
use crate::config::NetworkMode;
use crate::db::Db;
use crate::history_discovery::{self, Entry, Pager, SourceKind};
use crate::model::NewArticle;
const BATCH_SIZE: usize = 50;
use crate::web_clip;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BackfillStatus {
    Idle,
    Fetching,
    Paused,
    WaitingNextBatch,
    Completed,
    Failed,
}

#[derive(Debug, Clone)]
pub(crate) struct BackfillSnapshot {
    pub(crate) status: BackfillStatus,
    pub(crate) feed_id: Option<i64>,
    pub(crate) discovered: usize,
    pub(crate) processed: usize,
    pub(crate) inserted: usize,
    pub(crate) failed: usize,
    pub(crate) batch_size: usize,
    pub(crate) has_more: bool,
    pub(crate) last_error: Option<String>,
    pub(crate) source_description: String,
}

#[derive(Debug, Clone)]
pub(crate) enum BackfillNotice {
    Changed,
}

enum Command {
    Start { feed_id: i64, feed_url: String },
    Next,
    Pause,
    Resume,
    Retry,
    Shutdown,
}

pub(crate) struct HistoryBackfillWorkflow {
    command_tx: Sender<Command>,
    notice_rx: Receiver<BackfillNotice>,
    snapshot: Arc<Mutex<BackfillSnapshot>>,
    handle: Option<JoinHandle<()>>,
}

impl HistoryBackfillWorkflow {
    pub(crate) fn start(
        db_path: PathBuf,
        mode: NetworkMode,
        repaint: impl Fn() + Send + Sync + 'static,
    ) -> Result<Self> {
        let (command_tx, command_rx) = mpsc::channel();
        let (notice_tx, notice_rx) = mpsc::channel();
        let snapshot = Arc::new(Mutex::new(BackfillSnapshot {
            status: BackfillStatus::Idle,
            feed_id: None,
            discovered: 0,
            processed: 0,
            inserted: 0,
            failed: 0,
            batch_size: BATCH_SIZE,
            has_more: false,
            last_error: None,
            source_description: "自动检测归档页，未发现时使用 RSS/Atom 分页".into(),
        }));
        let shared = Arc::clone(&snapshot);
        let repaint = Arc::new(repaint);
        let handle = thread::Builder::new()
            .name("shiyue-history-backfill".into())
            .spawn(move || worker(db_path, mode, command_rx, notice_tx, shared, repaint))?;
        Ok(Self {
            command_tx,
            notice_rx,
            snapshot,
            handle: Some(handle),
        })
    }

    pub(crate) fn start_feed(&self, feed_id: i64, feed_url: String) -> Result<()> {
        self.command_tx
            .send(Command::Start { feed_id, feed_url })
            .map_err(|_| anyhow!("历史回补后台任务已停止"))
    }
    pub(crate) fn next_batch(&self) -> Result<()> {
        self.send(Command::Next)
    }
    pub(crate) fn pause(&self) -> Result<()> {
        self.send(Command::Pause)
    }
    pub(crate) fn resume(&self) -> Result<()> {
        self.send(Command::Resume)
    }
    pub(crate) fn retry_failed(&self) -> Result<()> {
        self.send(Command::Retry)
    }
    fn send(&self, command: Command) -> Result<()> {
        self.command_tx
            .send(command)
            .map_err(|_| anyhow!("历史回补后台任务已停止"))
    }
    pub(crate) fn snapshot(&self) -> BackfillSnapshot {
        self.snapshot
            .lock()
            .expect("backfill snapshot poisoned")
            .clone()
    }
    pub(crate) fn try_notices(&self) -> impl Iterator<Item = BackfillNotice> + '_ {
        std::iter::from_fn(|| match self.notice_rx.try_recv() {
            Ok(value) => Some(value),
            Err(TryRecvError::Empty | TryRecvError::Disconnected) => None,
        })
    }
}

impl Drop for HistoryBackfillWorkflow {
    fn drop(&mut self) {
        let _ = self.command_tx.send(Command::Shutdown);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

struct State {
    feed_id: i64,
    entries: Vec<Entry>,
    seen_urls: HashSet<String>,
    failed_entries: Vec<Entry>,
    cursor: usize,
    pager: Pager,
    paused: bool,
    shutdown: bool,
}

fn worker(
    db_path: PathBuf,
    mode: NetworkMode,
    command_rx: Receiver<Command>,
    notice_tx: Sender<BackfillNotice>,
    snapshot: Arc<Mutex<BackfillSnapshot>>,
    repaint: Arc<dyn Fn() + Send + Sync>,
) {
    let Ok(client) = web_clip::client_with_mode(mode) else {
        return;
    };
    let mut state: Option<State> = None;
    while let Ok(command) = command_rx.recv() {
        match command {
            Command::Shutdown => break,
            Command::Start { feed_id, feed_url } => {
                state = None;
                let result = start_state(&client, mode, feed_id, feed_url, &snapshot);
                match result {
                    Ok(next) => {
                        state = Some(next);
                        process_batch(
                            &db_path,
                            mode,
                            &client,
                            &command_rx,
                            &notice_tx,
                            &repaint,
                            &snapshot,
                            &mut state,
                        );
                    }
                    Err(error) => fail_snapshot(&snapshot, error.to_string()),
                }
                notify(&notice_tx, &repaint);
            }
            Command::Next => process_batch(
                &db_path,
                mode,
                &client,
                &command_rx,
                &notice_tx,
                &repaint,
                &snapshot,
                &mut state,
            ),
            Command::Pause => {
                if let Some(current) = state.as_mut() {
                    current.paused = true;
                    set_status(&snapshot, BackfillStatus::Paused);
                    notify(&notice_tx, &repaint);
                }
            }
            Command::Resume => {
                if let Some(current) = state.as_mut() {
                    current.paused = false;
                    process_batch(
                        &db_path,
                        mode,
                        &client,
                        &command_rx,
                        &notice_tx,
                        &repaint,
                        &snapshot,
                        &mut state,
                    );
                }
            }
            Command::Retry => {
                retry_failed(&db_path, mode, &client, &snapshot, &mut state);
                notify(&notice_tx, &repaint);
            }
        }
        if state.as_ref().is_some_and(|current| current.shutdown) {
            break;
        }
    }
}

fn start_state(
    client: &reqwest::blocking::Client,
    mode: NetworkMode,
    feed_id: i64,
    feed_url: String,
    snapshot: &Arc<Mutex<BackfillSnapshot>>,
) -> Result<State> {
    {
        let mut current = snapshot.lock().expect("backfill snapshot poisoned");
        current.feed_id = Some(feed_id);
        current.status = BackfillStatus::Fetching;
        current.discovered = 0;
        current.processed = 0;
        current.inserted = 0;
        current.failed = 0;
        current.has_more = false;
        current.last_error = None;
        current.source_description = "正在自动检测归档页…".into();
    }
    let (pager, entries) = history_discovery::discover(&feed_url, &mut |url, kind| {
        fetch_source(client, mode, url, kind)
    })?;
    let mut state = State {
        feed_id,
        entries: Vec::new(),
        seen_urls: HashSet::new(),
        failed_entries: Vec::new(),
        cursor: 0,
        pager,
        paused: false,
        shutdown: false,
    };
    append_entries(&mut state, entries);
    let mut current = snapshot.lock().expect("backfill snapshot poisoned");
    current.status = BackfillStatus::Fetching;
    current.feed_id = Some(feed_id);
    current.discovered = state.entries.len();
    current.processed = 0;
    current.inserted = 0;
    current.failed = 0;
    current.has_more = !state.entries.is_empty() || state.pager.has_more();
    current.last_error = None;
    current.source_description = state.pager.description.clone();
    Ok(state)
}

fn fetch_source(
    client: &reqwest::blocking::Client,
    mode: NetworkMode,
    url: &str,
    kind: SourceKind,
) -> Result<web_clip::FetchedWebClip> {
    match kind {
        SourceKind::Feed => {
            let mut last_error = None;
            for candidate in
                std::iter::once(url.to_owned()).chain(crate::fetch::feed_fallback_urls(url))
            {
                match web_clip::fetch_feed_document_with_mode(client, &candidate, mode) {
                    Ok(document) => return Ok(document),
                    Err(error) => last_error = Some(error),
                }
            }
            Err(last_error.unwrap_or_else(|| anyhow!("订阅源地址为空")))
        }
        SourceKind::Archive => {
            // Archive pages are frequently advertised as HTTP but redirect
            // or block non-TLS clients. Try the secure equivalent first while
            // retaining the discovered URL as a fallback for legacy sites.
            let mut candidates = Vec::with_capacity(2);
            if let Ok(mut secure) = reqwest::Url::parse(url) {
                if secure.scheme() == "http" {
                    let _ = secure.set_scheme("https");
                    candidates.push(secure.to_string());
                }
            }
            candidates.push(url.to_owned());
            let mut last_error = None;
            for candidate in candidates {
                for attempt in 0..4 {
                    match web_clip::fetch_html_with_mode(client, &candidate, mode) {
                        Ok(document) => return Ok(document),
                        Err(error) => {
                            let retryable = error.to_string().contains("HTTP 429");
                            last_error = Some(error);
                            if !retryable || attempt == 3 {
                                break;
                            }
                            std::thread::sleep(std::time::Duration::from_secs(
                                2_u64.saturating_pow(attempt + 1),
                            ));
                        }
                    }
                }
            }
            Err(last_error.unwrap_or_else(|| anyhow!("归档地址为空")))
        }
    }
}

fn append_entries(state: &mut State, entries: Vec<Entry>) {
    for entry in entries {
        let identity = entry
            .article
            .url
            .as_deref()
            .map(canonical_article_identity)
            .unwrap_or_else(|| format!("id:{}", entry.article.entry_id));
        if state.seen_urls.insert(identity) {
            state.entries.push(entry);
        }
    }
}

/// Treat equivalent feed/archive representations as one article.
///
/// Archive pages commonly link to `/post`, `/post/` or `/post/index.html`,
/// while an Atom feed may use a fragment or a redirect URL. Those are the
/// same article for backfill purposes and must not consume another batch slot.
fn canonical_article_identity(raw: &str) -> String {
    let Ok(mut url) = reqwest::Url::parse(raw.trim()) else {
        return raw.trim().to_ascii_lowercase();
    };
    url.set_fragment(None);
    if let Some(host) = url.host_str().map(str::to_ascii_lowercase) {
        let _ = url.set_host(Some(&host));
    }
    let path = url.path().trim_end_matches('/');
    let path = path.strip_suffix("/index.html").unwrap_or(path);
    let path = if path.is_empty() { "/" } else { path }.to_owned();
    url.set_path(&path);
    url.to_string().trim_end_matches('/').to_ascii_lowercase()
}

fn append_page(
    client: &reqwest::blocking::Client,
    mode: NetworkMode,
    state: &mut State,
) -> Result<()> {
    let entries = state
        .pager
        .next_page(&mut |url, kind| fetch_source(client, mode, url, kind))?;
    append_entries(state, entries);
    Ok(())
}
fn process_batch(
    db_path: &Path,
    mode: NetworkMode,
    client: &reqwest::blocking::Client,
    command_rx: &Receiver<Command>,
    notice_tx: &Sender<BackfillNotice>,
    repaint: &Arc<dyn Fn() + Send + Sync>,
    snapshot: &Arc<Mutex<BackfillSnapshot>>,
    state: &mut Option<State>,
) {
    let Some(current) = state.as_mut() else {
        return;
    };
    if current.paused {
        set_status(snapshot, BackfillStatus::Paused);
        return;
    }
    set_status(snapshot, BackfillStatus::Fetching);
    snapshot
        .lock()
        .expect("backfill snapshot poisoned")
        .last_error = None;
    let target = current.cursor.saturating_add(BATCH_SIZE);
    let mut pages = 0;
    while current.entries.len() < target && current.pager.has_more() && pages < 10 {
        if drain_control(command_rx, current, snapshot) {
            notify(notice_tx, repaint);
            return;
        }
        if let Err(error) = append_page(client, mode, current) {
            fail_snapshot(snapshot, error.to_string());
            snapshot
                .lock()
                .expect("backfill snapshot poisoned")
                .has_more = true;
            notify(notice_tx, repaint);
            return;
        }
        pages += 1;
    }
    while current.cursor < current.entries.len().min(target) {
        if drain_control(command_rx, current, snapshot) {
            notify(notice_tx, repaint);
            return;
        }
        let entry = current.entries[current.cursor].clone();
        let result = fetch_article(client, mode, &entry);
        current.cursor += 1;
        match result {
            Ok(article) => match Db::open(db_path).and_then(|db| {
                db.record_historical_articles_unread(
                    &db.get_feed(current.feed_id)?,
                    chrono::Utc::now().timestamp(),
                    &[article],
                )
            }) {
                Ok(inserted) => {
                    snapshot
                        .lock()
                        .expect("backfill snapshot poisoned")
                        .inserted += inserted;
                }
                Err(_error) => current.failed_entries.push(entry),
            },
            Err(_) => current.failed_entries.push(entry),
        }
        let mut view = snapshot.lock().expect("backfill snapshot poisoned");
        view.processed += 1;
        view.failed = current.failed_entries.len();
        view.discovered = current.entries.len();
        drop(view);
        notify(notice_tx, repaint);
    }
    let has_more = current.cursor < current.entries.len() || current.pager.has_more();
    let mut view = snapshot.lock().expect("backfill snapshot poisoned");
    view.has_more = has_more;
    view.status = if has_more {
        BackfillStatus::WaitingNextBatch
    } else {
        BackfillStatus::Completed
    };
    drop(view);
    notify(notice_tx, repaint);
}

fn fetch_article(
    client: &reqwest::blocking::Client,
    mode: NetworkMode,
    entry: &Entry,
) -> Result<NewArticle> {
    if !entry.fetch_body {
        return Ok(entry.article.clone());
    }
    let url = entry
        .article
        .url
        .as_deref()
        .ok_or_else(|| anyhow!("文章地址缺失"))?;
    let mut last_error = None;
    let fetched = (0..4)
        .find_map(
            |attempt| match web_clip::fetch_html_with_mode(client, url, mode) {
                Ok(document) => Some(document),
                Err(error) => {
                    let retryable = error.to_string().contains("HTTP 429");
                    last_error = Some(error);
                    if retryable && attempt < 3 {
                        std::thread::sleep(std::time::Duration::from_secs(
                            2_u64.saturating_pow(attempt + 1),
                        ));
                    }
                    None
                }
            },
        )
        .ok_or_else(|| last_error.unwrap_or_else(|| anyhow!("正文抓取失败")))?;
    article_from_document(entry, &fetched)
}

fn article_from_document(entry: &Entry, fetched: &web_clip::FetchedWebClip) -> Result<NewArticle> {
    let readable = prepare_article_html(&fetched.html);
    if readable.content.trim().is_empty() {
        return Err(anyhow!("没有提取到正文"));
    }
    let (author, published) = extract_metadata(&fetched.html);
    Ok(NewArticle {
        entry_id: fetched.final_url.clone(),
        url: Some(fetched.final_url.clone()),
        title: readable.title.or_else(|| entry.article.title.clone()),
        author: author.or_else(|| entry.article.author.clone()),
        // Preserve archive metadata when a page omits it; a month alone is not a publication date.
        published: published
            .or(entry.article.published)
            .or_else(|| date_from_url(&fetched.final_url))
            .or_else(|| entry.article.url.as_deref().and_then(date_from_url)),
        content: Some(readable.content),
    })
}

fn date_from_url(raw: &str) -> Option<i64> {
    let url = reqwest::Url::parse(raw).ok()?;
    let parts: Vec<_> = url
        .path_segments()?
        .filter(|part| !part.is_empty())
        .collect();
    for part in &parts {
        if part.len() == 8 {
            if let Ok(date) = chrono::NaiveDate::parse_from_str(part, "%Y%m%d") {
                return date
                    .and_hms_opt(0, 0, 0)
                    .map(|time| time.and_utc().timestamp());
            }
        }
    }
    if let Some(date) = parts.windows(3).find_map(|window| {
        let year = window[0].parse::<i32>().ok()?;
        let month = window[1].parse::<u32>().ok()?;
        let day = window[2].parse::<u32>().ok()?;
        chrono::NaiveDate::from_ymd_opt(year, month, day)
    }) {
        return date
            .and_hms_opt(0, 0, 0)
            .map(|time| time.and_utc().timestamp());
    }
    None
}

fn extract_metadata(html: &str) -> (Option<String>, Option<i64>) {
    let document = scraper::Html::parse_document(html);
    let date_selectors = [
        "meta[property='article:published_time'], meta[name='date']",
        "[itemprop~='datePublished']",
        "abbr.published, .dt-published, time.published, .published[datetime], .published[title]",
        "time[datetime]:not(.updated):not([itemprop~='dateModified'])",
    ];
    let published = date_selectors.iter().find_map(|selector| {
        let selector = scraper::Selector::parse(selector).unwrap();
        document
            .select(&selector)
            .filter(is_article_metadata)
            .find_map(|node| parse_published_value(&metadata_value(node)))
    });
    let author_selectors = [
        "meta[name='author']",
        "[itemprop~='author'] [itemprop~='name'], .author .fn",
        "[rel~='author'], .p-author, [itemprop~='author']",
    ];
    let author = author_selectors.iter().find_map(|selector| {
        let selector = scraper::Selector::parse(selector).unwrap();
        document
            .select(&selector)
            .filter(is_article_metadata)
            .find_map(|node| clean_author(&metadata_value(node)))
    });
    let json_selector = scraper::Selector::parse("script[type='application/ld+json']").unwrap();
    let mut json_author = None;
    let mut json_date = None;
    for node in document.select(&json_selector) {
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(&node.inner_html()) {
            if let Some(article) = json_article(&value) {
                json_author =
                    json_author.or_else(|| article.get("author").and_then(json_author_name));
                json_date = json_date.or_else(|| {
                    article
                        .get("datePublished")?
                        .as_str()
                        .and_then(parse_published_value)
                });
            }
        }
    }
    (author.or(json_author), published.or(json_date))
}

fn is_article_metadata(node: &scraper::ElementRef<'_>) -> bool {
    !node
        .ancestors()
        .filter_map(scraper::ElementRef::wrap)
        .any(|ancestor| {
            ancestor
                .value()
                .classes()
                .chain(ancestor.value().id())
                .any(|token| {
                    let token = token.to_ascii_lowercase();
                    token.contains("comment") || token.contains("reply") || token == "related-posts"
                })
        })
}

fn metadata_value(node: scraper::ElementRef<'_>) -> String {
    ["content", "datetime", "title"]
        .iter()
        .find_map(|attribute| node.value().attr(attribute))
        .map(str::to_owned)
        .unwrap_or_else(|| node.text().collect::<String>())
}

fn clean_author(value: &str) -> Option<String> {
    let value = value.split_whitespace().collect::<Vec<_>>().join(" ");
    (!value.is_empty() && value.chars().count() <= 120 && !value.starts_with("http"))
        .then_some(value)
}

fn json_article(value: &serde_json::Value) -> Option<&serde_json::Value> {
    if let Some(values) = value.as_array() {
        return values.iter().find_map(json_article);
    }
    let article_type = |kind: &str| {
        let kind = kind.to_ascii_lowercase();
        kind.ends_with("article") || kind == "blogposting"
    };
    if value.get("@type").is_some_and(|kind| {
        kind.as_str().is_some_and(article_type)
            || kind.as_array().is_some_and(|kinds| {
                kinds
                    .iter()
                    .filter_map(|kind| kind.as_str())
                    .any(article_type)
            })
    }) {
        return Some(value);
    }
    value.get("@graph").and_then(json_article)
}

fn json_author_name(value: &serde_json::Value) -> Option<String> {
    if let Some(values) = value.as_array() {
        return values.iter().find_map(json_author_name);
    }
    value
        .as_str()
        .or_else(|| value.get("name")?.as_str())
        .and_then(clean_author)
}

fn parse_published_value(value: &str) -> Option<i64> {
    let value = value.trim();
    chrono::DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|date| date.timestamp())
        .or_else(|| {
            chrono::DateTime::parse_from_str(value, "%Y-%m-%dT%H:%M%#z")
                .ok()
                .map(|date| date.timestamp())
        })
        .or_else(|| {
            chrono::NaiveDate::parse_from_str(value, "%Y-%m-%d")
                .ok()
                .and_then(|date| date.and_hms_opt(0, 0, 0))
                .map(|date| date.and_utc().timestamp())
        })
}

fn retry_failed(
    db_path: &Path,
    mode: NetworkMode,
    client: &reqwest::blocking::Client,
    snapshot: &Arc<Mutex<BackfillSnapshot>>,
    state: &mut Option<State>,
) {
    let Some(current) = state.as_mut() else {
        return;
    };
    let failed = std::mem::take(&mut current.failed_entries);
    for entry in failed {
        match fetch_article(client, mode, &entry).and_then(|article| {
            Db::open(db_path).and_then(|db| {
                db.record_historical_articles_unread(
                    &db.get_feed(current.feed_id)?,
                    Utc::now().timestamp(),
                    &[article],
                )
            })
        }) {
            Ok(inserted) => {
                snapshot
                    .lock()
                    .expect("backfill snapshot poisoned")
                    .inserted += inserted;
            }
            Err(_) => current.failed_entries.push(entry),
        }
    }
    let mut view = snapshot.lock().expect("backfill snapshot poisoned");
    view.failed = current.failed_entries.len();
    view.has_more = current.cursor < current.entries.len() || current.pager.has_more();
    view.status = if view.failed == 0 {
        if view.has_more {
            BackfillStatus::WaitingNextBatch
        } else {
            BackfillStatus::Completed
        }
    } else {
        BackfillStatus::Failed
    };
}

fn drain_control(
    rx: &Receiver<Command>,
    state: &mut State,
    snapshot: &Arc<Mutex<BackfillSnapshot>>,
) -> bool {
    loop {
        match rx.try_recv() {
            Ok(Command::Pause) => {
                state.paused = true;
                set_status(snapshot, BackfillStatus::Paused);
                return true;
            }
            Ok(Command::Shutdown) => {
                state.paused = true;
                state.shutdown = true;
                return true;
            }
            Ok(Command::Resume) => state.paused = false,
            Ok(Command::Next | Command::Start { .. } | Command::Retry) => {}
            Err(TryRecvError::Empty | TryRecvError::Disconnected) => return state.paused,
        }
    }
}

fn set_status(snapshot: &Arc<Mutex<BackfillSnapshot>>, status: BackfillStatus) {
    snapshot.lock().expect("backfill snapshot poisoned").status = status;
}
fn fail_snapshot(snapshot: &Arc<Mutex<BackfillSnapshot>>, error: String) {
    let mut view = snapshot.lock().expect("backfill snapshot poisoned");
    view.status = BackfillStatus::Failed;
    view.last_error = Some(error);
}
fn notify(notice_tx: &Sender<BackfillNotice>, repaint: &Arc<dyn Fn() + Send + Sync>) {
    let _ = notice_tx.send(BackfillNotice::Changed);
    repaint();
}

#[cfg(test)]
mod tests {
    use super::{date_from_url, extract_metadata};

    #[test]
    fn date_from_url_supports_compact_article_dates() {
        let expected = chrono::NaiveDate::from_ymd_opt(2026, 8, 26)
            .unwrap()
            .and_hms_opt(0, 0, 0)
            .unwrap()
            .and_utc()
            .timestamp();
        assert_eq!(
            date_from_url("https://blog.solazy.me/20260826/"),
            Some(expected)
        );
    }

    #[test]
    fn date_from_url_supports_segmented_article_dates() {
        let expected = chrono::NaiveDate::from_ymd_opt(2026, 8, 26)
            .unwrap()
            .and_hms_opt(0, 0, 0)
            .unwrap()
            .and_utc()
            .timestamp();
        assert_eq!(
            date_from_url("https://example.com/2026/08/26/article"),
            Some(expected)
        );
    }

    #[test]
    fn date_from_url_does_not_fabricate_a_day_from_year_and_month() {
        assert_eq!(date_from_url("https://example.com/2026/08/article"), None);
    }

    #[test]
    fn metadata_parser_reads_common_author_and_published_markup() {
        let (author, published) = extract_metadata(
            r#"<p class="vcard author">作者：<a class="fn">阮一峰</a></p>
               <abbr class="published" title="2026-08-07T08:08:27+08:00">2026年8月7日</abbr>"#,
        );
        assert_eq!(author.as_deref(), Some("阮一峰"));
        assert_eq!(published, Some(1786061307));
    }

    #[test]
    fn metadata_parser_reads_minute_precision_datetime() {
        let (_, published) = extract_metadata(r#"<time datetime="2026-08-26T15:00Z">"#);
        assert_eq!(published, Some(1787756400));
    }
}
