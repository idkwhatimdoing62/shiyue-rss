//! Session-bound RSS refresh orchestration.
//!
//! GUI and CLI express refresh intent through this module. HTTP, scheduling,
//! bounded concurrency, database commits, maintenance coordination and run
//! observation remain behind the facade described by ADR-0004.

use anyhow::{Context, Result, anyhow};
use chrono::Utc;
use std::collections::{HashMap, HashSet, VecDeque};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio::task::JoinSet;

use crate::config::Config;
use crate::config::NetworkMode;
use crate::db::Db;
use crate::local_data_maintenance::{GenerationFence, MaintenanceFence, MaintenanceParticipant};
use crate::model::{Feed, NewArticle};

const MAX_CONCURRENT_FEEDS: usize = 8;
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);
const EXTERNAL_MAINTENANCE_POLL: Duration = Duration::from_millis(250);
const MODULE_RETRY_DELAY: Duration = Duration::from_secs(1);
const MAX_TECHNICAL_DETAIL_CHARS: usize = 1_024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct RunId(pub(crate) u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RefreshWorkflowStatus {
    Idle,
    Running,
    PausedForMaintenance,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RefreshRunStatus {
    Fetching,
    Committing,
    Succeeded,
    Degraded,
    Failed,
    Interrupted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FeedRefreshStatus {
    Pending,
    Fetching,
    Committing,
    Succeeded,
    Failed,
    Removed,
    Interrupted,
}

impl FeedRefreshStatus {
    fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Succeeded | Self::Failed | Self::Removed | Self::Interrupted
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RefreshFailureKind {
    Timeout,
    Http,
    Network,
    Parse,
    Storage,
    Internal,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RefreshFailure {
    pub(crate) kind: RefreshFailureKind,
    pub(crate) technical_detail: String,
}

#[derive(Debug, Clone)]
pub(crate) struct FeedRefreshSnapshot {
    pub(crate) feed_id: i64,
    pub(crate) title: Option<String>,
    pub(crate) url: String,
    pub(crate) status: FeedRefreshStatus,
    pub(crate) new_articles: usize,
    pub(crate) failure: Option<RefreshFailure>,
}

#[derive(Debug, Clone)]
pub(crate) struct RefreshRunSnapshot {
    pub(crate) run_id: RunId,
    pub(crate) status: RefreshRunStatus,
    pub(crate) target_count: usize,
    pub(crate) completed_count: usize,
    pub(crate) new_article_count: usize,
    pub(crate) failed_feed_count: usize,
    pub(crate) feeds: Vec<FeedRefreshSnapshot>,
    pub(crate) module_failure: Option<RefreshFailure>,
}

impl RefreshRunSnapshot {
    pub(crate) fn feeds_with_new_articles(&self) -> usize {
        self.feeds
            .iter()
            .filter(|feed| feed.new_articles > 0)
            .count()
    }
}

#[derive(Debug, Clone)]
pub(crate) struct RefreshWorkflowSnapshot {
    pub(crate) status: RefreshWorkflowStatus,
    pub(crate) current: Option<RefreshRunSnapshot>,
    pub(crate) last_completed: Option<RefreshRunSnapshot>,
}

#[derive(Debug, Clone)]
pub(crate) enum RefreshNotice {
    Changed(RunId),
    ModuleFault {
        user_message: String,
        technical_detail: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkerMode {
    Scheduled,
    OneShot,
}

#[derive(Default)]
struct PendingTargets {
    all: bool,
    ids: HashSet<i64>,
    exclude: HashSet<i64>,
}

impl PendingTargets {
    fn is_empty(&self) -> bool {
        !self.all && self.ids.is_empty()
    }

    fn merge_all(&mut self, active: &HashSet<i64>) {
        self.all = true;
        self.exclude.extend(active);
    }

    fn merge_ids(&mut self, ids: impl IntoIterator<Item = i64>, active: &HashSet<i64>) {
        self.ids
            .extend(ids.into_iter().filter(|id| !active.contains(id)));
    }

    fn take(&mut self) -> Self {
        std::mem::take(self)
    }
}

enum RefreshIntent {
    All,
    Feeds(HashSet<i64>),
}

enum WorkerCommand {
    Refresh(RefreshIntent),
    Pause {
        reply: std_mpsc::Sender<Result<(), String>>,
    },
    Resume {
        reply: std_mpsc::Sender<Result<(), String>>,
    },
    Shutdown {
        reply: std_mpsc::Sender<()>,
    },
}

type FetchFuture = Pin<
    Box<dyn Future<Output = std::result::Result<FetchPayload, RefreshFailure>> + Send + 'static>,
>;

#[derive(Debug)]
struct FetchPayload {
    title: Option<String>,
    articles: Vec<NewArticle>,
}

trait FeedFetcher: Send + Sync {
    fn fetch(&self, feed: Feed) -> FetchFuture;
}

trait RefreshClock: Send + Sync {
    fn now(&self) -> i64;
}

struct SystemClock;

impl RefreshClock for SystemClock {
    fn now(&self) -> i64 {
        Utc::now().timestamp()
    }
}

struct HttpFeedFetcher {
    client: reqwest::Client,
    mode: NetworkMode,
}

impl HttpFeedFetcher {
    fn new(mode: NetworkMode) -> Result<Self> {
        Ok(Self {
            client: crate::fetch::client_with_mode(mode)?,
            mode,
        })
    }
}

impl FeedFetcher for HttpFeedFetcher {
    fn fetch(&self, feed: Feed) -> FetchFuture {
        let client = self.client.clone();
        let mode = self.mode;
        Box::pin(async move {
            crate::fetch::fetch_with_feed_fallback(&client, &feed.url, mode)
                .await
                .map(|(title, articles)| FetchPayload { title, articles })
                .map_err(classify_fetch_error)
        })
    }
}

struct WorkflowState {
    snapshot: RefreshWorkflowSnapshot,
    next_run_id: u64,
}

impl Default for WorkflowState {
    fn default() -> Self {
        Self {
            snapshot: RefreshWorkflowSnapshot {
                status: RefreshWorkflowStatus::Idle,
                current: None,
                last_completed: None,
            },
            next_run_id: 1,
        }
    }
}

type WakeCallback = Arc<dyn Fn() + Send + Sync>;

struct WorkerContext {
    db_path: PathBuf,
    cfg: Config,
    mode: WorkerMode,
    fetcher: Arc<dyn FeedFetcher>,
    clock: Arc<dyn RefreshClock>,
    state: Arc<Mutex<WorkflowState>>,
    notice_tx: std_mpsc::Sender<RefreshNotice>,
    wake: WakeCallback,
}

struct WorkflowMaintenanceParticipant {
    command_tx: mpsc::UnboundedSender<WorkerCommand>,
}

impl MaintenanceParticipant for WorkflowMaintenanceParticipant {
    fn name(&self) -> &'static str {
        "rss_refresh_workflow"
    }

    fn quiesce(&self, deadline: Instant, _epoch: &str) -> Result<()> {
        let (reply_tx, reply_rx) = std_mpsc::channel();
        self.command_tx
            .send(WorkerCommand::Pause { reply: reply_tx })
            .context("RSS refresh workflow has stopped")?;
        let remaining = deadline.saturating_duration_since(Instant::now());
        reply_rx
            .recv_timeout(remaining)
            .context("RSS refresh workflow did not reach a maintenance safe point")?
            .map_err(anyhow::Error::msg)
    }

    fn resume(&self, _epoch: &str) -> Result<()> {
        let (reply_tx, reply_rx) = std_mpsc::channel();
        self.command_tx
            .send(WorkerCommand::Resume { reply: reply_tx })
            .context("RSS refresh workflow has stopped")?;
        reply_rx
            .recv_timeout(Duration::from_secs(1))
            .context("RSS refresh workflow did not resume")?
            .map_err(anyhow::Error::msg)
    }
}

pub(crate) struct RssRefreshWorkflow {
    command_tx: mpsc::UnboundedSender<WorkerCommand>,
    notice_rx: std_mpsc::Receiver<RefreshNotice>,
    state: Arc<Mutex<WorkflowState>>,
    join: Option<JoinHandle<()>>,
}

impl RssRefreshWorkflow {
    pub(crate) fn start_scheduled(
        db_path: PathBuf,
        cfg: Config,
        wake: impl Fn() + Send + Sync + 'static,
    ) -> Result<Self> {
        let network_mode = cfg.network_mode;
        Self::start_with(
            db_path,
            cfg,
            WorkerMode::Scheduled,
            Arc::new(HttpFeedFetcher::new(network_mode)?),
            Arc::new(SystemClock),
            Arc::new(wake),
        )
    }

    fn start_with(
        db_path: PathBuf,
        cfg: Config,
        mode: WorkerMode,
        fetcher: Arc<dyn FeedFetcher>,
        clock: Arc<dyn RefreshClock>,
        wake: WakeCallback,
    ) -> Result<Self> {
        // Fail at the boundary rather than creating a worker that can never
        // access its store. The handle is dropped immediately: no long-lived
        // database writer lease crosses an HTTP wait.
        drop(Db::open(&db_path).context("RSS refresh workflow cannot open the database")?);
        let (command_tx, command_rx) = mpsc::unbounded_channel();
        let (notice_tx, notice_rx) = std_mpsc::channel();
        let state = Arc::new(Mutex::new(WorkflowState::default()));
        let thread_state = Arc::clone(&state);
        let thread_wake = Arc::clone(&wake);
        let join = std::thread::Builder::new()
            .name("shiyue-rss-refresh".into())
            .spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build();
                match runtime {
                    Ok(runtime) => runtime.block_on(worker_loop(
                        WorkerContext {
                            db_path,
                            cfg,
                            mode,
                            fetcher,
                            clock,
                            state: thread_state,
                            notice_tx: notice_tx.clone(),
                            wake: thread_wake,
                        },
                        command_rx,
                    )),
                    Err(error) => {
                        let detail = sanitize_detail(&format!("create runtime: {error:#}"));
                        let _ = notice_tx.send(RefreshNotice::ModuleFault {
                            user_message: "RSS 刷新模块无法启动".into(),
                            technical_detail: detail,
                        });
                        wake();
                    }
                }
            })?;
        Ok(Self {
            command_tx,
            notice_rx,
            state,
            join: Some(join),
        })
    }

    pub(crate) fn request_all(&self) -> Result<()> {
        self.command_tx
            .send(WorkerCommand::Refresh(RefreshIntent::All))
            .context("RSS refresh workflow has stopped")
    }

    pub(crate) fn request_feed(&self, feed_id: i64) -> Result<()> {
        self.command_tx
            .send(WorkerCommand::Refresh(RefreshIntent::Feeds(HashSet::from(
                [feed_id],
            ))))
            .context("RSS refresh workflow has stopped")
    }

    pub(crate) fn snapshot(&self) -> RefreshWorkflowSnapshot {
        self.state
            .lock()
            .expect("RSS refresh state poisoned")
            .snapshot
            .clone()
    }

    pub(crate) fn try_notices(&self) -> std_mpsc::TryIter<'_, RefreshNotice> {
        self.notice_rx.try_iter()
    }

    pub(crate) fn maintenance_participant(&self) -> Arc<dyn MaintenanceParticipant> {
        Arc::new(WorkflowMaintenanceParticipant {
            command_tx: self.command_tx.clone(),
        })
    }

    pub(crate) fn run_once_all(db_path: &Path, cfg: &Config) -> Result<RefreshRunSnapshot> {
        Self::run_once(db_path, cfg, RefreshIntent::All)
    }

    pub(crate) fn run_once_feed(
        db_path: &Path,
        cfg: &Config,
        feed_id: i64,
    ) -> Result<RefreshRunSnapshot> {
        Self::run_once(db_path, cfg, RefreshIntent::Feeds(HashSet::from([feed_id])))
    }

    fn run_once(db_path: &Path, cfg: &Config, intent: RefreshIntent) -> Result<RefreshRunSnapshot> {
        let network_mode = cfg.network_mode;
        let workflow = Self::start_with(
            db_path.to_path_buf(),
            cfg.clone(),
            WorkerMode::OneShot,
            Arc::new(HttpFeedFetcher::new(network_mode)?),
            Arc::new(SystemClock),
            Arc::new(|| {}),
        )?;
        workflow
            .command_tx
            .send(WorkerCommand::Refresh(intent))
            .context("RSS refresh workflow has stopped")?;
        loop {
            match workflow.notice_rx.recv() {
                Ok(RefreshNotice::Changed(run_id)) => {
                    let snapshot = workflow.snapshot();
                    if let Some(completed) = snapshot.last_completed
                        && completed.run_id == run_id
                    {
                        return Ok(completed);
                    }
                }
                Ok(RefreshNotice::ModuleFault {
                    technical_detail, ..
                }) => return Err(anyhow!(technical_detail)),
                Err(_) => return Err(anyhow!("RSS refresh workflow stopped before completion")),
            }
        }
    }
}

impl Drop for RssRefreshWorkflow {
    fn drop(&mut self) {
        let (reply_tx, reply_rx) = std_mpsc::channel();
        if self
            .command_tx
            .send(WorkerCommand::Shutdown { reply: reply_tx })
            .is_ok()
            && reply_rx.recv_timeout(SHUTDOWN_TIMEOUT).is_ok()
            && let Some(join) = self.join.take()
        {
            let _ = join.join();
        }
        // A wedged network/runtime thread is detached after the bounded
        // shutdown window; application exit must not hang forever.
        self.join.take();
    }
}

enum RunControl {
    Continue,
    Paused,
    Shutdown,
}

async fn worker_loop(
    context: WorkerContext,
    mut command_rx: mpsc::UnboundedReceiver<WorkerCommand>,
) {
    let WorkerContext {
        db_path,
        cfg,
        mode,
        fetcher,
        clock,
        state,
        notice_tx,
        wake,
    } = context;
    let mut pending = PendingTargets::default();
    let mut paused = false;
    loop {
        if paused {
            set_workflow_status(&state, RefreshWorkflowStatus::PausedForMaintenance);
            tokio::select! {
                command = command_rx.recv() => match command {
                    Some(WorkerCommand::Refresh(intent)) => merge_idle_intent(&mut pending, intent),
                    Some(WorkerCommand::Pause { reply }) => { let _ = reply.send(Ok(())); }
                    Some(WorkerCommand::Resume { reply }) => {
                        paused = false;
                        set_workflow_status(&state, RefreshWorkflowStatus::Idle);
                        let _ = reply.send(Ok(()));
                    }
                    Some(WorkerCommand::Shutdown { reply }) => { let _ = reply.send(()); return; }
                    None => return,
                },
                _ = tokio::time::sleep(EXTERNAL_MAINTENANCE_POLL) => {
                    if !MaintenanceFence::observe(&db_path)
                        .map(|availability| availability.is_active())
                        .unwrap_or(true)
                    {
                        paused = false;
                        set_workflow_status(&state, RefreshWorkflowStatus::Idle);
                    }
                }
            }
            continue;
        }

        let had_pending_intent = !pending.is_empty();
        let targets = if had_pending_intent {
            resolve_pending(&db_path, pending.take())
        } else if mode == WorkerMode::Scheduled {
            Db::open(&db_path).and_then(|db| db.due_feeds(clock.now()))
        } else {
            Ok(Vec::new())
        };

        match targets {
            Ok(feeds) if !feeds.is_empty() || had_pending_intent => {
                match execute_run(
                    &db_path,
                    &cfg,
                    Arc::clone(&fetcher),
                    Arc::clone(&clock),
                    feeds,
                    &mut command_rx,
                    &mut pending,
                    &state,
                    &notice_tx,
                    &wake,
                )
                .await
                {
                    RunControl::Continue => {}
                    RunControl::Paused => paused = true,
                    RunControl::Shutdown => return,
                }
                continue;
            }
            Ok(_) => {}
            Err(error) => {
                publish_fault(
                    &notice_tx,
                    &wake,
                    "无法读取待刷新的订阅",
                    &format!("{error:#}"),
                );
            }
        }

        let wait = if mode == WorkerMode::Scheduled {
            next_wait(&db_path, &cfg, clock.now()).unwrap_or(MODULE_RETRY_DELAY)
        } else {
            Duration::from_secs(24 * 60 * 60)
        };
        set_workflow_status(&state, RefreshWorkflowStatus::Idle);
        tokio::select! {
            _ = tokio::time::sleep(wait) => {}
            command = command_rx.recv() => match command {
                Some(WorkerCommand::Refresh(intent)) => merge_idle_intent(&mut pending, intent),
                Some(WorkerCommand::Pause { reply }) => {
                    paused = true;
                    let _ = reply.send(Ok(()));
                }
                Some(WorkerCommand::Resume { reply }) => { let _ = reply.send(Ok(())); }
                Some(WorkerCommand::Shutdown { reply }) => { let _ = reply.send(()); return; }
                None => return,
            }
        }
    }
}

fn merge_idle_intent(pending: &mut PendingTargets, intent: RefreshIntent) {
    match intent {
        RefreshIntent::All => pending.merge_all(&HashSet::new()),
        RefreshIntent::Feeds(ids) => pending.merge_ids(ids, &HashSet::new()),
    }
}

fn resolve_pending(db_path: &Path, pending: PendingTargets) -> Result<Vec<Feed>> {
    let db = Db::open(db_path)?;
    let mut by_id = HashMap::new();
    if pending.all {
        for feed in db.enabled_feeds()? {
            if !pending.exclude.contains(&feed.id) {
                by_id.insert(feed.id, feed);
            }
        }
    }
    if !pending.ids.is_empty() {
        for feed in db.enabled_feeds()? {
            if pending.ids.contains(&feed.id) {
                by_id.insert(feed.id, feed);
            }
        }
    }
    let mut feeds = by_id.into_values().collect::<Vec<_>>();
    feeds.sort_by_key(|feed| feed.id);
    Ok(feeds)
}

fn next_wait(db_path: &Path, cfg: &Config, now: i64) -> Result<Duration> {
    let next = Db::open(db_path)?
        .earliest_next_fetch()?
        .unwrap_or(now + cfg.default_interval_secs.max(1));
    Ok(Duration::from_secs((next - now).max(1) as u64))
}

#[allow(clippy::too_many_arguments)]
async fn execute_run(
    db_path: &Path,
    cfg: &Config,
    fetcher: Arc<dyn FeedFetcher>,
    clock: Arc<dyn RefreshClock>,
    feeds: Vec<Feed>,
    command_rx: &mut mpsc::UnboundedReceiver<WorkerCommand>,
    pending: &mut PendingTargets,
    state: &Arc<Mutex<WorkflowState>>,
    notice_tx: &std_mpsc::Sender<RefreshNotice>,
    wake: &WakeCallback,
) -> RunControl {
    let original_feeds = feeds.clone();
    let mut run = begin_run(state, feeds);
    publish_run(state, notice_tx, wake, &run, false);
    if run.target_count == 0 {
        run.status = RefreshRunStatus::Succeeded;
        publish_run(state, notice_tx, wake, &run, true);
        return RunControl::Continue;
    }

    let generation = match MaintenanceFence::witness(db_path) {
        Ok(generation) => generation,
        Err(error) if is_maintenance_error(&error) => {
            interrupt_run(&mut run);
            pending.merge_ids(run.feeds.iter().map(|feed| feed.feed_id), &HashSet::new());
            publish_run(state, notice_tx, wake, &run, true);
            return RunControl::Paused;
        }
        Err(error) => {
            fail_run_module(&mut run, RefreshFailureKind::Storage, &format!("{error:#}"));
            publish_run(state, notice_tx, wake, &run, true);
            publish_fault(
                notice_tx,
                wake,
                "RSS 刷新模块无法访问资料库",
                &format!("{error:#}"),
            );
            return RunControl::Continue;
        }
    };

    let active_ids = run
        .feeds
        .iter()
        .map(|feed| feed.feed_id)
        .collect::<HashSet<_>>();
    let mut queue = VecDeque::from(original_feeds);
    let mut tasks = JoinSet::new();
    spawn_available(&mut tasks, &mut queue, &fetcher, &mut run);
    publish_run(state, notice_tx, wake, &run, false);

    while !tasks.is_empty() || !queue.is_empty() {
        tokio::select! {
            command = command_rx.recv() => match command {
                Some(WorkerCommand::Refresh(RefreshIntent::All)) => pending.merge_all(&active_ids),
                Some(WorkerCommand::Refresh(RefreshIntent::Feeds(ids))) => pending.merge_ids(ids, &active_ids),
                Some(WorkerCommand::Resume { reply }) => { let _ = reply.send(Ok(())); }
                Some(WorkerCommand::Pause { reply }) => {
                    abort_and_drain(&mut tasks).await;
                    merge_unfinished(pending, &run);
                    interrupt_run(&mut run);
                    publish_run(state, notice_tx, wake, &run, true);
                    let _ = reply.send(Ok(()));
                    return RunControl::Paused;
                }
                Some(WorkerCommand::Shutdown { reply }) => {
                    abort_and_drain(&mut tasks).await;
                    interrupt_run(&mut run);
                    publish_run(state, notice_tx, wake, &run, true);
                    let _ = reply.send(());
                    return RunControl::Shutdown;
                }
                None => {
                    abort_and_drain(&mut tasks).await;
                    interrupt_run(&mut run);
                    publish_run(state, notice_tx, wake, &run, true);
                    return RunControl::Shutdown;
                }
            },
            joined = tasks.join_next(), if !tasks.is_empty() => {
                let Some(joined) = joined else { continue };
                let (feed, result) = match joined {
                    Ok(value) => value,
                    Err(error) => {
                        abort_and_drain(&mut tasks).await;
                        fail_run_module(&mut run, RefreshFailureKind::Internal, &format!("fetch task failed: {error}"));
                        publish_run(state, notice_tx, wake, &run, true);
                        publish_fault(notice_tx, wake, "RSS 刷新执行器异常停止", &format!("fetch task failed: {error}"));
                        return RunControl::Continue;
                    }
                };
                set_feed_status(&mut run, feed.id, FeedRefreshStatus::Committing);
                run.status = RefreshRunStatus::Committing;
                publish_run(state, notice_tx, wake, &run, false);
                match commit_result(db_path, cfg, clock.now(), &generation, &feed, result) {
                    Ok(CommitResult::Applied { new_articles, failure }) => {
                        let snapshot = run.feeds.iter_mut().find(|value| value.feed_id == feed.id)
                            .expect("active feed exists in run snapshot");
                        snapshot.new_articles = new_articles;
                        snapshot.failure = failure;
                        snapshot.status = if snapshot.failure.is_some() {
                            run.failed_feed_count += 1;
                            FeedRefreshStatus::Failed
                        } else {
                            run.new_article_count += new_articles;
                            FeedRefreshStatus::Succeeded
                        };
                        run.completed_count += 1;
                    }
                    Ok(CommitResult::Removed) => {
                        let snapshot = run.feeds.iter_mut().find(|value| value.feed_id == feed.id)
                            .expect("active feed exists in run snapshot");
                        snapshot.status = FeedRefreshStatus::Removed;
                        snapshot.new_articles = 0;
                        snapshot.failure = None;
                        run.completed_count += 1;
                    }
                    Err(error) if is_maintenance_error(&error) => {
                        abort_and_drain(&mut tasks).await;
                        merge_unfinished(pending, &run);
                        interrupt_run(&mut run);
                        publish_run(state, notice_tx, wake, &run, true);
                        return RunControl::Paused;
                    }
                    Err(error) => {
                        abort_and_drain(&mut tasks).await;
                        if let Some(snapshot) = run.feeds.iter_mut().find(|value| value.feed_id == feed.id) {
                            snapshot.status = FeedRefreshStatus::Failed;
                            snapshot.failure = Some(RefreshFailure {
                                kind: RefreshFailureKind::Storage,
                                technical_detail: sanitize_detail(&format!("{error:#}")),
                            });
                        }
                        run.failed_feed_count += 1;
                        fail_run_module(&mut run, RefreshFailureKind::Storage, &format!("{error:#}"));
                        publish_run(state, notice_tx, wake, &run, true);
                        publish_fault(notice_tx, wake, "RSS 刷新结果无法写入资料库", &format!("{error:#}"));
                        return RunControl::Continue;
                    }
                }
                spawn_available(&mut tasks, &mut queue, &fetcher, &mut run);
                run.status = RefreshRunStatus::Fetching;
                publish_run(state, notice_tx, wake, &run, false);
            }
        }
    }

    run.status = if run.failed_feed_count == 0 {
        RefreshRunStatus::Succeeded
    } else {
        // Fetch/parse failures are valid per-Feed outcomes once their durable
        // backoff state commits. `Failed` is reserved for module/storage
        // faults that prevent the workflow from processing outcomes safely.
        RefreshRunStatus::Degraded
    };
    publish_run(state, notice_tx, wake, &run, true);
    RunControl::Continue
}

fn begin_run(state: &Arc<Mutex<WorkflowState>>, feeds: Vec<Feed>) -> RefreshRunSnapshot {
    let mut guard = state.lock().expect("RSS refresh state poisoned");
    let run_id = RunId(guard.next_run_id);
    guard.next_run_id += 1;
    let snapshots = feeds
        .into_iter()
        .map(|feed| FeedRefreshSnapshot {
            feed_id: feed.id,
            title: feed.title,
            url: feed.url,
            status: FeedRefreshStatus::Pending,
            new_articles: 0,
            failure: None,
        })
        .collect::<Vec<_>>();
    RefreshRunSnapshot {
        run_id,
        status: RefreshRunStatus::Fetching,
        target_count: snapshots.len(),
        completed_count: 0,
        new_article_count: 0,
        failed_feed_count: 0,
        feeds: snapshots,
        module_failure: None,
    }
}

fn publish_run(
    state: &Arc<Mutex<WorkflowState>>,
    notice_tx: &std_mpsc::Sender<RefreshNotice>,
    wake: &WakeCallback,
    run: &RefreshRunSnapshot,
    terminal: bool,
) {
    {
        let mut guard = state.lock().expect("RSS refresh state poisoned");
        if terminal {
            guard.snapshot.status = RefreshWorkflowStatus::Idle;
            guard.snapshot.current = None;
            guard.snapshot.last_completed = Some(run.clone());
        } else {
            guard.snapshot.status = RefreshWorkflowStatus::Running;
            guard.snapshot.current = Some(run.clone());
        }
    }
    let _ = notice_tx.send(RefreshNotice::Changed(run.run_id));
    wake();
}

fn set_workflow_status(state: &Arc<Mutex<WorkflowState>>, status: RefreshWorkflowStatus) {
    state
        .lock()
        .expect("RSS refresh state poisoned")
        .snapshot
        .status = status;
}

fn spawn_available(
    tasks: &mut JoinSet<(Feed, std::result::Result<FetchPayload, RefreshFailure>)>,
    queue: &mut VecDeque<Feed>,
    fetcher: &Arc<dyn FeedFetcher>,
    run: &mut RefreshRunSnapshot,
) {
    while tasks.len() < MAX_CONCURRENT_FEEDS {
        let Some(feed) = queue.pop_front() else { break };
        set_feed_status(run, feed.id, FeedRefreshStatus::Fetching);
        let task_fetcher = Arc::clone(fetcher);
        tasks.spawn(async move {
            let result = task_fetcher.fetch(feed.clone()).await;
            (feed, result)
        });
    }
}

async fn abort_and_drain(
    tasks: &mut JoinSet<(Feed, std::result::Result<FetchPayload, RefreshFailure>)>,
) {
    tasks.abort_all();
    while tasks.join_next().await.is_some() {}
}

enum CommitResult {
    Applied {
        new_articles: usize,
        failure: Option<RefreshFailure>,
    },
    Removed,
}

fn commit_result(
    db_path: &Path,
    cfg: &Config,
    now: i64,
    generation: &GenerationFence,
    feed: &Feed,
    result: std::result::Result<FetchPayload, RefreshFailure>,
) -> Result<CommitResult> {
    let mut db = Db::open(db_path)?;
    let tx = db.fenced_transaction_for(generation)?;
    let feed_exists = |tx: &crate::local_data_maintenance::FencedTransaction<'_>| {
        tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM feeds WHERE id = ?1)",
            [feed.id],
            |row| row.get::<_, bool>(0),
        )
    };
    if !feed_exists(&tx)? {
        tx.commit()?;
        return Ok(CommitResult::Removed);
    }
    let committed = match result {
        Ok(payload) => {
            match Db::record_success_on(&tx, feed, now, cfg, payload.title, &payload.articles) {
                Ok(new_articles) => CommitResult::Applied {
                    new_articles,
                    failure: None,
                },
                Err(_error) if !feed_exists(&tx)? => CommitResult::Removed,
                Err(error) => return Err(error),
            }
        }
        Err(failure) => match Db::record_failure_on(&tx, feed, now, cfg, &failure.technical_detail)
        {
            Ok(()) => CommitResult::Applied {
                new_articles: 0,
                failure: Some(failure),
            },
            Err(_error) if !feed_exists(&tx)? => CommitResult::Removed,
            Err(error) => return Err(error),
        },
    };
    tx.commit()?;
    Ok(committed)
}

fn set_feed_status(run: &mut RefreshRunSnapshot, feed_id: i64, status: FeedRefreshStatus) {
    if let Some(feed) = run.feeds.iter_mut().find(|feed| feed.feed_id == feed_id) {
        feed.status = status;
    }
}

fn merge_unfinished(pending: &mut PendingTargets, run: &RefreshRunSnapshot) {
    pending.merge_ids(
        run.feeds
            .iter()
            .filter(|feed| !feed.status.is_terminal())
            .map(|feed| feed.feed_id),
        &HashSet::new(),
    );
}

fn interrupt_run(run: &mut RefreshRunSnapshot) {
    for feed in &mut run.feeds {
        if !feed.status.is_terminal() {
            feed.status = FeedRefreshStatus::Interrupted;
        }
    }
    run.status = RefreshRunStatus::Interrupted;
}

fn fail_run_module(run: &mut RefreshRunSnapshot, kind: RefreshFailureKind, detail: &str) {
    for feed in &mut run.feeds {
        if !feed.status.is_terminal() {
            feed.status = FeedRefreshStatus::Interrupted;
        }
    }
    run.status = RefreshRunStatus::Failed;
    run.module_failure = Some(RefreshFailure {
        kind,
        technical_detail: sanitize_detail(detail),
    });
}

fn publish_fault(
    notice_tx: &std_mpsc::Sender<RefreshNotice>,
    wake: &WakeCallback,
    user_message: &str,
    detail: &str,
) {
    let _ = notice_tx.send(RefreshNotice::ModuleFault {
        user_message: user_message.into(),
        technical_detail: sanitize_detail(detail),
    });
    wake();
}

fn classify_fetch_error(error: anyhow::Error) -> RefreshFailure {
    let kind = if let Some(request) = error.downcast_ref::<reqwest::Error>() {
        if request.is_timeout() {
            RefreshFailureKind::Timeout
        } else if request.status().is_some() {
            RefreshFailureKind::Http
        } else {
            RefreshFailureKind::Network
        }
    } else {
        RefreshFailureKind::Parse
    };
    RefreshFailure {
        kind,
        technical_detail: sanitize_detail(&format!("{error:#}")),
    }
}

fn is_maintenance_error(error: &anyhow::Error) -> bool {
    MaintenanceFence::rejected(error)
}

fn sanitize_detail(detail: &str) -> String {
    let compact = detail
        .replace(['\r', '\n'], " ")
        .replace("Bearer ", "Bearer [redacted]")
        .replace("sk-", "[redacted-key-prefix]");
    compact.chars().take(MAX_TECHNICAL_DETAIL_CHARS).collect()
}

/// Convert persisted request details into a concise user-facing explanation
/// while retaining the bounded technical detail for diagnosis.
pub(crate) fn format_refresh_error_for_display(detail: &str) -> String {
    let lower = detail.to_ascii_lowercase();
    let summary = if lower.contains("timeout") || lower.contains("timed out") {
        "请求超时，请检查网络连接或稍后重试"
    } else if lower.contains("error sending request")
        || lower.contains("dns")
        || lower.contains("failed to lookup")
        || lower.contains("connect")
    {
        "网络连接失败，请检查网络或代理设置"
    } else if lower.contains("404") {
        "订阅地址不存在（HTTP 404），请编辑订阅地址"
    } else if lower.contains("401") || lower.contains("403") {
        "服务器拒绝访问，请检查订阅地址或访问权限"
    } else if detail.contains("解析订阅源失败") {
        "订阅内容无法解析，请检查源格式"
    } else {
        return detail.to_owned();
    };
    format!("{summary}\n技术详情：{detail}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};

    static TEST_SEQUENCE: AtomicUsize = AtomicUsize::new(1);

    struct FixedClock(AtomicI64);

    impl FixedClock {
        fn new(now: i64) -> Self {
            Self(AtomicI64::new(now))
        }
    }

    impl RefreshClock for FixedClock {
        fn now(&self) -> i64 {
            self.0.load(Ordering::SeqCst)
        }
    }

    struct ActiveFetch {
        active: Arc<AtomicUsize>,
    }

    impl Drop for ActiveFetch {
        fn drop(&mut self) {
            self.active.fetch_sub(1, Ordering::SeqCst);
        }
    }

    struct FakeFetcher {
        fail_ids: HashSet<i64>,
        delay: Duration,
        active: Arc<AtomicUsize>,
        max_active: Arc<AtomicUsize>,
    }

    impl FakeFetcher {
        fn new(fail_ids: impl IntoIterator<Item = i64>, delay: Duration) -> Self {
            Self {
                fail_ids: fail_ids.into_iter().collect(),
                delay,
                active: Arc::new(AtomicUsize::new(0)),
                max_active: Arc::new(AtomicUsize::new(0)),
            }
        }
    }

    impl FeedFetcher for FakeFetcher {
        fn fetch(&self, feed: Feed) -> FetchFuture {
            let fail = self.fail_ids.contains(&feed.id);
            let delay = self.delay;
            let active = Arc::clone(&self.active);
            let max_active = Arc::clone(&self.max_active);
            Box::pin(async move {
                let current = active.fetch_add(1, Ordering::SeqCst) + 1;
                max_active.fetch_max(current, Ordering::SeqCst);
                let _guard = ActiveFetch { active };
                tokio::time::sleep(delay).await;
                if fail {
                    return Err(RefreshFailure {
                        kind: RefreshFailureKind::Network,
                        technical_detail: format!("fake failure for {}", feed.url),
                    });
                }
                Ok(FetchPayload {
                    title: Some(format!("Feed {}", feed.id)),
                    articles: vec![NewArticle {
                        entry_id: format!("entry-{}", feed.id),
                        url: Some(format!("https://example.test/{}/article", feed.id)),
                        title: Some(format!("Article {}", feed.id)),
                        author: None,
                        published: Some(1_700_000_000),
                        content: Some("body".into()),
                    }],
                })
            })
        }
    }

    struct TestStore {
        root: PathBuf,
        path: PathBuf,
        feed_ids: Vec<i64>,
    }

    impl TestStore {
        fn new(feed_count: usize, now: i64) -> Self {
            let root = std::env::temp_dir().join(format!(
                "shiyue-rss-workflow-{}-{}",
                std::process::id(),
                TEST_SEQUENCE.fetch_add(1, Ordering::SeqCst)
            ));
            std::fs::create_dir_all(&root).unwrap();
            let path = root.join("rrss.db");
            let db = Db::open(&path).unwrap();
            let feed_ids = (0..feed_count)
                .map(|index| {
                    db.add_feed(&format!("https://feed-{index}.example.test/rss"), now)
                        .unwrap()
                })
                .collect();
            drop(db);
            Self {
                root,
                path,
                feed_ids,
            }
        }
    }

    impl Drop for TestStore {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn start_test_workflow(
        store: &TestStore,
        mode: WorkerMode,
        fetcher: Arc<dyn FeedFetcher>,
        now: i64,
    ) -> RssRefreshWorkflow {
        RssRefreshWorkflow::start_with(
            store.path.clone(),
            Config::default(),
            mode,
            fetcher,
            Arc::new(FixedClock::new(now)),
            Arc::new(|| {}),
        )
        .unwrap()
    }

    fn wait_for_current(workflow: &RssRefreshWorkflow) -> RefreshRunSnapshot {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if let Some(run) = workflow.snapshot().current {
                return run;
            }
            assert!(Instant::now() < deadline, "run did not start");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn wait_for_terminal_after(
        workflow: &RssRefreshWorkflow,
        after: Option<RunId>,
    ) -> RefreshRunSnapshot {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(!remaining.is_zero(), "run did not finish");
            match workflow.notice_rx.recv_timeout(remaining).unwrap() {
                RefreshNotice::Changed(run_id) => {
                    if after.is_some_and(|previous| run_id <= previous) {
                        continue;
                    }
                    if let Some(run) = workflow.snapshot().last_completed
                        && run.run_id == run_id
                    {
                        return run;
                    }
                }
                RefreshNotice::ModuleFault {
                    technical_detail, ..
                } => panic!("unexpected module fault: {technical_detail}"),
            }
        }
    }

    #[test]
    fn scheduled_mode_refreshes_due_feeds() {
        let now = 1_700_000_000;
        let store = TestStore::new(2, now);
        let workflow = start_test_workflow(
            &store,
            WorkerMode::Scheduled,
            Arc::new(FakeFetcher::new([], Duration::from_millis(1))),
            now,
        );
        let run = wait_for_terminal_after(&workflow, None);
        assert_eq!(run.status, RefreshRunStatus::Succeeded);
        assert_eq!(run.target_count, 2);
        assert_eq!(run.new_article_count, 2);
    }

    #[test]
    fn manual_all_refresh_is_bounded_to_eight_concurrent_feeds() {
        let now = 1_700_000_000;
        let store = TestStore::new(12, now);
        let fetcher = Arc::new(FakeFetcher::new([], Duration::from_millis(30)));
        let workflow = start_test_workflow(&store, WorkerMode::OneShot, fetcher.clone(), now);
        workflow.request_all().unwrap();
        let run = wait_for_terminal_after(&workflow, None);
        assert_eq!(run.status, RefreshRunStatus::Succeeded);
        assert_eq!(fetcher.max_active.load(Ordering::SeqCst), 8);
    }

    #[test]
    fn partial_failure_is_degraded_and_commits_other_feeds() {
        let now = 1_700_000_000;
        let store = TestStore::new(3, now);
        let workflow = start_test_workflow(
            &store,
            WorkerMode::OneShot,
            Arc::new(FakeFetcher::new(
                [store.feed_ids[1]],
                Duration::from_millis(1),
            )),
            now,
        );
        workflow.request_all().unwrap();
        let run = wait_for_terminal_after(&workflow, None);
        assert_eq!(run.status, RefreshRunStatus::Degraded);
        assert_eq!(run.completed_count, 3);
        assert_eq!(run.failed_feed_count, 1);
        assert_eq!(run.new_article_count, 2);
        assert!(
            Db::open(&store.path)
                .unwrap()
                .get_feed(store.feed_ids[1])
                .unwrap()
                .last_error
                .is_some()
        );
    }

    #[test]
    fn all_fetch_failures_are_degraded_when_failure_state_commits() {
        let now = 1_700_000_000;
        let store = TestStore::new(2, now);
        let workflow = start_test_workflow(
            &store,
            WorkerMode::OneShot,
            Arc::new(FakeFetcher::new(
                store.feed_ids.clone(),
                Duration::from_millis(1),
            )),
            now,
        );
        workflow.request_all().unwrap();
        let run = wait_for_terminal_after(&workflow, None);
        assert_eq!(run.status, RefreshRunStatus::Degraded);
        assert_eq!(run.completed_count, 2);
        assert_eq!(run.failed_feed_count, 2);
    }

    #[test]
    fn intent_during_a_run_creates_one_pending_run_for_missing_targets() {
        let now = 1_700_000_000;
        let store = TestStore::new(2, now);
        let workflow = start_test_workflow(
            &store,
            WorkerMode::OneShot,
            Arc::new(FakeFetcher::new([], Duration::from_millis(80))),
            now,
        );
        workflow.request_feed(store.feed_ids[0]).unwrap();
        let first = wait_for_current(&workflow);
        workflow.request_all().unwrap();
        let first = wait_for_terminal_after(&workflow, Some(RunId(first.run_id.0 - 1)));
        let second = wait_for_terminal_after(&workflow, Some(first.run_id));
        assert_eq!(first.target_count, 1);
        assert_eq!(second.target_count, 1);
        assert_eq!(second.feeds[0].feed_id, store.feed_ids[1]);
    }

    #[test]
    fn maintenance_interrupts_and_resumes_unfinished_targets() {
        let now = 1_700_000_000;
        let store = TestStore::new(1, now);
        let workflow = start_test_workflow(
            &store,
            WorkerMode::OneShot,
            Arc::new(FakeFetcher::new([], Duration::from_millis(150))),
            now,
        );
        workflow.request_all().unwrap();
        let active = wait_for_current(&workflow);
        let participant = workflow.maintenance_participant();
        participant
            .quiesce(Instant::now() + Duration::from_secs(1), "test")
            .unwrap();
        let interrupted = workflow.snapshot().last_completed.unwrap();
        assert_eq!(interrupted.run_id, active.run_id);
        assert_eq!(interrupted.status, RefreshRunStatus::Interrupted);
        participant.resume("test").unwrap();
        let resumed = wait_for_terminal_after(&workflow, Some(interrupted.run_id));
        assert_eq!(resumed.status, RefreshRunStatus::Succeeded);
        assert_eq!(resumed.target_count, 1);
    }

    #[test]
    fn disabled_pending_feed_is_removed_before_the_next_run() {
        let now = 1_700_000_000;
        let store = TestStore::new(2, now);
        let workflow = start_test_workflow(
            &store,
            WorkerMode::OneShot,
            Arc::new(FakeFetcher::new([], Duration::from_millis(80))),
            now,
        );
        workflow.request_feed(store.feed_ids[0]).unwrap();
        let first = wait_for_current(&workflow);
        workflow.request_feed(store.feed_ids[1]).unwrap();
        Db::open(&store.path)
            .unwrap()
            .set_disabled(store.feed_ids[1], true, now)
            .unwrap();
        let first = wait_for_terminal_after(&workflow, Some(RunId(first.run_id.0 - 1)));
        let second = wait_for_terminal_after(&workflow, Some(first.run_id));
        assert_eq!(second.status, RefreshRunStatus::Succeeded);
        assert_eq!(second.target_count, 0);
    }

    #[test]
    fn deleting_a_subscription_during_an_active_run_discards_the_late_result() {
        let now = 1_700_000_000;
        let store = TestStore::new(1, now);
        let feed_id = store.feed_ids[0];
        let workflow = start_test_workflow(
            &store,
            WorkerMode::OneShot,
            Arc::new(FakeFetcher::new([], Duration::from_millis(120))),
            now,
        );
        workflow.request_feed(feed_id).unwrap();
        let active = wait_for_current(&workflow);
        Db::open(&store.path)
            .unwrap()
            .remove_feed(&feed_id.to_string())
            .unwrap();

        let completed = wait_for_terminal_after(&workflow, Some(RunId(active.run_id.0 - 1)));
        assert_eq!(completed.status, RefreshRunStatus::Succeeded);
        assert_eq!(completed.completed_count, 1);
        assert_eq!(completed.failed_feed_count, 0);
        assert_eq!(completed.feeds[0].status, FeedRefreshStatus::Removed);
        assert!(
            Db::open(&store.path)
                .unwrap()
                .find_feed(feed_id)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn sanitizes_and_caps_technical_details() {
        let detail = format!("Bearer secret sk-abc\n{}", "x".repeat(2_000));
        let sanitized = sanitize_detail(&detail);
        assert!(!sanitized.contains("Bearer secret"));
        assert!(!sanitized.contains("sk-abc"));
        assert_eq!(sanitized.chars().count(), MAX_TECHNICAL_DETAIL_CHARS);
    }

    #[test]
    fn refresh_error_display_explains_common_network_failures() {
        let message = format_refresh_error_for_display(
            "error sending request for url (https://example.test/feed): operation timed out",
        );
        assert!(message.starts_with("请求超时，请检查网络连接或稍后重试"));
        assert!(message.contains("技术详情："));
    }

    #[test]
    fn refresh_error_display_keeps_unknown_details_unchanged() {
        assert_eq!(
            format_refresh_error_for_display("自定义订阅错误"),
            "自定义订阅错误"
        );
    }
}
