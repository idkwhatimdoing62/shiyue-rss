//! Background, on-demand full-text retrieval for RSS articles.

use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use anyhow::Result;

use crate::article_document_presentation::prepare_article_html;
use crate::config::NetworkMode;
use crate::db::Db;
use crate::web_clip;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FullTextStatus {
    Idle,
    Fetching,
    Succeeded,
    Failed,
}

#[derive(Debug, Clone)]
pub(crate) struct FullTextSnapshot {
    pub(crate) status: FullTextStatus,
    pub(crate) article_id: Option<i64>,
    pub(crate) error: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) enum FullTextNotice {
    Changed,
}

enum Command {
    Fetch { article_id: i64, url: String },
    Shutdown,
}

pub(crate) struct ArticleFullTextWorkflow {
    command_tx: Sender<Command>,
    notice_rx: Receiver<FullTextNotice>,
    snapshot: Arc<Mutex<FullTextSnapshot>>,
    handle: Option<JoinHandle<()>>,
}

impl ArticleFullTextWorkflow {
    pub(crate) fn start(
        db_path: PathBuf,
        mode: NetworkMode,
        repaint: impl Fn() + Send + Sync + 'static,
    ) -> Result<Self> {
        let (command_tx, command_rx) = mpsc::channel();
        let (notice_tx, notice_rx) = mpsc::channel();
        let snapshot = Arc::new(Mutex::new(FullTextSnapshot {
            status: FullTextStatus::Idle,
            article_id: None,
            error: None,
        }));
        let shared = Arc::clone(&snapshot);
        let repaint = Arc::new(repaint);
        let handle = thread::Builder::new()
            .name("shiyue-article-fulltext".into())
            .spawn(move || worker(db_path, mode, command_rx, notice_tx, shared, repaint))?;
        Ok(Self {
            command_tx,
            notice_rx,
            snapshot,
            handle: Some(handle),
        })
    }

    pub(crate) fn request(&self, article_id: i64, url: String) -> Result<()> {
        self.command_tx
            .send(Command::Fetch { article_id, url })
            .map_err(|_| anyhow::anyhow!("全文抓取后台任务已停止"))
    }

    pub(crate) fn snapshot(&self) -> FullTextSnapshot {
        self.snapshot
            .lock()
            .expect("full text snapshot poisoned")
            .clone()
    }

    pub(crate) fn try_notices(&self) -> impl Iterator<Item = FullTextNotice> + '_ {
        std::iter::from_fn(|| match self.notice_rx.try_recv() {
            Ok(value) => Some(value),
            Err(TryRecvError::Empty | TryRecvError::Disconnected) => None,
        })
    }
}

impl Drop for ArticleFullTextWorkflow {
    fn drop(&mut self) {
        let _ = self.command_tx.send(Command::Shutdown);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn worker(
    db_path: PathBuf,
    mode: NetworkMode,
    command_rx: Receiver<Command>,
    notice_tx: Sender<FullTextNotice>,
    snapshot: Arc<Mutex<FullTextSnapshot>>,
    repaint: Arc<dyn Fn() + Send + Sync>,
) {
    let Ok(client) = web_clip::client_with_mode(mode) else {
        return;
    };
    while let Ok(command) = command_rx.recv() {
        match command {
            Command::Shutdown => break,
            Command::Fetch { article_id, url } => {
                fetch_one(&db_path, mode, &client, article_id, &url, &snapshot);
                let _ = notice_tx.send(FullTextNotice::Changed);
                repaint();
            }
        }
    }
}

fn fetch_one(
    db_path: &std::path::Path,
    mode: NetworkMode,
    client: &reqwest::blocking::Client,
    article_id: i64,
    url: &str,
    snapshot: &Arc<Mutex<FullTextSnapshot>>,
) {
    {
        let mut state = snapshot.lock().expect("full text snapshot poisoned");
        state.status = FullTextStatus::Fetching;
        state.article_id = Some(article_id);
        state.error = None;
    }
    let result = (|| -> Result<()> {
        let fetched = web_clip::fetch_html_with_mode(client, url, mode)?;
        let readable = prepare_article_html(&fetched.html);
        if readable.content.trim().is_empty() {
            anyhow::bail!("原网页没有提取到可读正文");
        }
        let db = Db::open(db_path)?;
        db.update_article_content(article_id, &readable.content)?;
        Ok(())
    })();
    let mut state = snapshot.lock().expect("full text snapshot poisoned");
    match result {
        Ok(()) => state.status = FullTextStatus::Succeeded,
        Err(error) => {
            state.status = FullTextStatus::Failed;
            state.error = Some(error.to_string());
        }
    }
}
