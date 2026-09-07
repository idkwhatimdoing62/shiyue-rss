//! Session-bound historical import for 阮一峰的网络日志.

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
use crate::model::NewArticle;
use crate::ruanyifeng_archive::{self, ArchiveEntry, BATCH_SIZE};
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
    entries: Vec<ArchiveEntry>,
    seen_urls: HashSet<String>,
    failed_entries: Vec<ArchiveEntry>,
    cursor: usize,
    month_url: Option<String>,
    paused: bool,
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
    }
}

fn start_state(
    client: &reqwest::blocking::Client,
    mode: NetworkMode,
    feed_id: i64,
    feed_url: String,
    snapshot: &Arc<Mutex<BackfillSnapshot>>,
) -> Result<State> {
    if !feed_url.contains("ruanyifeng.com") {
        return Err(anyhow!("历史回补目前仅支持阮一峰博客"));
    }
    let root =
        web_clip::fetch_html_with_mode(client, ruanyifeng_archive::DEFAULT_ARCHIVE_URL, mode)?;
    let page = ruanyifeng_archive::parse_archive_page(&root.html, &root.final_url)?;
    let month_url = page
        .month_url
        .or_else(|| Some(ruanyifeng_archive::DEFAULT_ARCHIVE_URL.into()));
    let mut state = State {
        feed_id,
        entries: Vec::new(),
        seen_urls: HashSet::new(),
        failed_entries: Vec::new(),
        cursor: 0,
        month_url,
        paused: false,
    };
    append_page(client, mode, &mut state)?;
    let mut current = snapshot.lock().expect("backfill snapshot poisoned");
    current.status = BackfillStatus::Fetching;
    current.feed_id = Some(feed_id);
    current.discovered = state.entries.len();
    current.processed = 0;
    current.inserted = 0;
    current.failed = 0;
    current.has_more = !state.entries.is_empty() || state.month_url.is_some();
    current.last_error = None;
    Ok(state)
}

fn append_page(
    client: &reqwest::blocking::Client,
    mode: NetworkMode,
    state: &mut State,
) -> Result<()> {
    let Some(url) = state.month_url.take() else {
        return Ok(());
    };
    let fetched = web_clip::fetch_html_with_mode(client, &url, mode)?;
    let page = ruanyifeng_archive::parse_archive_page(&fetched.html, &fetched.final_url)?;
    for entry in page.entries {
        if state.seen_urls.insert(entry.url.clone()) {
            state.entries.push(entry);
        }
    }
    state.month_url = page.previous_month;
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
    let target = current.cursor.saturating_add(BATCH_SIZE);
    while current.entries.len() < target && current.month_url.is_some() {
        if let Err(error) = append_page(client, mode, current) {
            fail_snapshot(snapshot, error.to_string());
            return;
        }
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
                db.record_historical_articles(
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
    let has_more = current.cursor < current.entries.len() || current.month_url.is_some();
    let mut view = snapshot.lock().expect("backfill snapshot poisoned");
    view.has_more = has_more;
    view.status = if has_more {
        BackfillStatus::WaitingNextBatch
    } else {
        BackfillStatus::Completed
    };
}

fn fetch_article(
    client: &reqwest::blocking::Client,
    mode: NetworkMode,
    entry: &ArchiveEntry,
) -> Result<NewArticle> {
    let fetched = web_clip::fetch_html_with_mode(client, &entry.url, mode)?;
    let readable = prepare_article_html(&fetched.html);
    if readable.content.trim().is_empty() {
        return Err(anyhow!("没有提取到正文"));
    }
    Ok(NewArticle {
        entry_id: entry.url.clone(),
        url: Some(entry.url.clone()),
        title: readable.title.or_else(|| Some(entry.title.clone())),
        author: None,
        published: extract_published(&fetched.html),
        content: Some(readable.content),
    })
}

fn extract_published(html: &str) -> Option<i64> {
    let document = scraper::Html::parse_document(html);
    let meta =
        scraper::Selector::parse("meta[property='article:published_time'], meta[name='date']")
            .ok()?;
    let time = scraper::Selector::parse("time[datetime]").ok()?;
    document
        .select(&meta)
        .filter_map(|node| node.value().attr("content"))
        .chain(
            document
                .select(&time)
                .filter_map(|node| node.value().attr("datetime")),
        )
        .find_map(|value| {
            chrono::DateTime::parse_from_rfc3339(value)
                .map(|date| date.timestamp())
                .or_else(|_| {
                    chrono::NaiveDate::parse_from_str(value, "%Y-%m-%d")
                        .map(|date| date.and_hms_opt(0, 0, 0).unwrap().and_utc().timestamp())
                })
                .ok()
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
                db.record_historical_articles(
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
    view.status = if view.failed == 0 {
        BackfillStatus::WaitingNextBatch
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
