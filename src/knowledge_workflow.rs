//! Persistent knowledge-processing module.
//!
//! `KnowledgeEngine` is the only external seam used by GUI and CLI adapters.
//! Task/attempt rows, executor ownership and pipeline orchestration stay private.

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::article_document_presentation::prepare_article_html;
use crate::config::ResourceEnrichmentConfig;
use crate::db::Db;
use crate::library_projection_revision::{self, ProjectionImpact};
use crate::local_data_maintenance::MaintenanceParticipant;
use crate::resource_enrichment::{
    CredentialSource, EnrichmentOutput, EnrichmentProvider, ProviderRequest,
};
use crate::resource_library_lifecycle::{ResourcePrivacy, SnapshotInput};

pub(crate) mod article_target;
mod failure;
pub(crate) mod resource_target;
mod types;

use failure::{classify_error, sanitize_detail};
pub(crate) use types::{
    ConnectionState, ErrorKind, KnowledgeNotice, RequestDisposition, RequestReceipt, TaskKey,
    TaskKind, TaskSnapshot, TaskStage, TaskStatus,
};

const MAX_CONCURRENCY: usize = 2;
const MAX_AUTOMATIC_RETRIES: i64 = 2;
const LEASE_HEARTBEAT_SECS: i64 = 5;
const LEASE_EXPIRES_SECS: i64 = 15;
// Reserve part of the shared maintenance deadline for the fenced interrupt,
// lease release, database close, and cross-thread acknowledgement. Provider
// calls are not cancellable, so the grace period cannot consume this budget.
const QUIESCE_ACK_BUDGET: Duration = Duration::from_secs(1);

enum EngineCommand {
    Request {
        key: TaskKey,
        reply: std_mpsc::Sender<Result<RequestReceipt, String>>,
    },
    TestConnection {
        reply: std_mpsc::Sender<Result<(), String>>,
    },
    Observe {
        key: TaskKey,
    },
    Quiesce {
        deadline: Instant,
        reply: std_mpsc::Sender<Result<(), String>>,
    },
    Resume {
        reply: std_mpsc::Sender<Result<(), String>>,
    },
    Shutdown,
}

enum WorkDone {
    Task(i64),
    Connection {
        generation: i64,
        result: Result<String, String>,
    },
}

#[derive(Clone)]
enum ProviderMode {
    System,
    #[cfg(test)]
    Fixed(Arc<dyn EnrichmentProvider>),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ExecutorPolicy {
    Acquire,
    ObserveOnly,
}

/// Deep module facade shared by GUI and CLI adapters.
pub(crate) struct KnowledgeEngine {
    db_path: PathBuf,
    command_tx: std_mpsc::Sender<EngineCommand>,
    notice_rx: std_mpsc::Receiver<KnowledgeNotice>,
    projection_observer: KnowledgeProjectionObserver,
    join: Option<std::thread::JoinHandle<()>>,
}

/// Workflow-owned, memory-only view of task snapshots for desktop projection.
/// `None` from `snapshot` means the key has not been materialized yet; an
/// inner `None` means it was materialized and has no durable task.
#[derive(Default)]
struct KnowledgeProjectionState {
    residents: HashSet<TaskKey>,
    snapshots: HashMap<TaskKey, Option<TaskSnapshot>>,
}

#[derive(Clone)]
pub(crate) struct KnowledgeProjectionObserver {
    command_tx: std_mpsc::Sender<EngineCommand>,
    state: Arc<RwLock<KnowledgeProjectionState>>,
    changed_rx: Arc<Mutex<std_mpsc::Receiver<TaskKey>>>,
}

impl KnowledgeProjectionObserver {
    pub(crate) fn observe(&self, key: TaskKey) {
        let inserted = self
            .state
            .write()
            .expect("knowledge projection state poisoned")
            .residents
            .insert(key);
        if inserted {
            let _ = self.command_tx.send(EngineCommand::Observe { key });
        }
    }

    pub(crate) fn forget(&self, key: TaskKey) {
        let mut state = self
            .state
            .write()
            .expect("knowledge projection state poisoned");
        state.residents.remove(&key);
        state.snapshots.remove(&key);
    }

    pub(crate) fn snapshot(&self, key: TaskKey) -> Option<Option<TaskSnapshot>> {
        self.state
            .read()
            .expect("knowledge projection state poisoned")
            .snapshots
            .get(&key)
            .cloned()
    }

    pub(crate) fn try_changed(&self) -> impl Iterator<Item = TaskKey> + '_ {
        std::iter::from_fn(|| {
            self.changed_rx
                .lock()
                .expect("knowledge projection notice receiver poisoned")
                .try_recv()
                .ok()
        })
    }

    #[cfg(test)]
    pub(crate) fn disconnected_for_test() -> Self {
        let (command_tx, _command_rx) = std_mpsc::channel();
        let (_changed_tx, changed_rx) = std_mpsc::channel();
        Self {
            command_tx,
            state: Arc::new(RwLock::new(KnowledgeProjectionState::default())),
            changed_rx: Arc::new(Mutex::new(changed_rx)),
        }
    }

    #[cfg(test)]
    pub(crate) fn is_resident(&self, key: TaskKey) -> bool {
        self.state
            .read()
            .expect("knowledge projection state poisoned")
            .residents
            .contains(&key)
    }
}

struct KnowledgeMaintenanceParticipant {
    command_tx: std_mpsc::Sender<EngineCommand>,
}

impl MaintenanceParticipant for KnowledgeMaintenanceParticipant {
    fn name(&self) -> &'static str {
        "knowledge_processing_workflow"
    }

    fn quiesce(&self, deadline: Instant, _epoch: &str) -> Result<()> {
        let (reply_tx, reply_rx) = std_mpsc::channel();
        self.command_tx
            .send(EngineCommand::Quiesce {
                deadline,
                reply: reply_tx,
            })
            .context("KNOWLEDGE_ENGINE_STOPPED: cannot quiesce knowledge processing")?;
        let remaining = deadline.saturating_duration_since(Instant::now());
        reply_rx
            .recv_timeout(remaining)
            .context("KNOWLEDGE_QUIESCE_DEADLINE_EXCEEDED: safe point was not acknowledged")?
            .map_err(anyhow::Error::msg)
    }

    fn resume(&self, _epoch: &str) -> Result<()> {
        let (reply_tx, reply_rx) = std_mpsc::channel();
        self.command_tx
            .send(EngineCommand::Resume { reply: reply_tx })
            .context("KNOWLEDGE_ENGINE_STOPPED: cannot resume knowledge processing")?;
        reply_rx
            .recv_timeout(Duration::from_secs(1))
            .context("KNOWLEDGE_RESUME_TIMEOUT: knowledge processing did not resume")?
            .map_err(anyhow::Error::msg)
    }
}

impl KnowledgeEngine {
    pub(crate) fn start(db_path: PathBuf, config: ResourceEnrichmentConfig) -> Result<Self> {
        Self::start_with_mode(
            db_path,
            config,
            ProviderMode::System,
            ExecutorPolicy::Acquire,
        )
    }

    pub(crate) fn start_client(db_path: PathBuf, config: ResourceEnrichmentConfig) -> Result<Self> {
        Self::start_with_mode(
            db_path,
            config,
            ProviderMode::System,
            ExecutorPolicy::ObserveOnly,
        )
    }

    #[cfg(test)]
    fn start_with_provider(
        db_path: PathBuf,
        config: ResourceEnrichmentConfig,
        provider: Arc<dyn EnrichmentProvider>,
    ) -> Result<Self> {
        Self::start_with_mode(
            db_path,
            config,
            ProviderMode::Fixed(provider),
            ExecutorPolicy::Acquire,
        )
    }

    fn start_with_mode(
        db_path: PathBuf,
        config: ResourceEnrichmentConfig,
        provider_mode: ProviderMode,
        policy: ExecutorPolicy,
    ) -> Result<Self> {
        Db::open(&db_path).context("知识处理模块无法打开数据库")?;
        let (command_tx, command_rx) = std_mpsc::channel();
        let (notice_tx, notice_rx) = std_mpsc::channel();
        let projection_state = Arc::new(RwLock::new(KnowledgeProjectionState::default()));
        let (projection_notice_tx, projection_notice_rx) = std_mpsc::sync_channel(32);
        let projection_observer = KnowledgeProjectionObserver {
            command_tx: command_tx.clone(),
            state: Arc::clone(&projection_state),
            changed_rx: Arc::new(Mutex::new(projection_notice_rx)),
        };
        let projection_publication = ProjectionPublication {
            state: projection_state,
            notice_tx: projection_notice_tx,
        };
        let thread_path = db_path.clone();
        let join = std::thread::Builder::new()
            .name("shiyue-knowledge-engine".into())
            .spawn(move || {
                let mut explicitly_quiesced = false;
                loop {
                    let maintenance_active =
                        match crate::local_data_maintenance::MaintenanceFence::observe(&thread_path)
                        {
                            Ok(availability) => availability.is_active(),
                            Err(error) => {
                                let _ = notice_tx.send(KnowledgeNotice::ModuleFault {
                                    user_message: "无法读取资料维护状态".into(),
                                    technical_detail: sanitize_detail(&format!("{error:#}")),
                                });
                                std::thread::sleep(Duration::from_millis(100));
                                continue;
                            }
                        };
                    if explicitly_quiesced || maintenance_active {
                        projection_publication.clear_materialized();
                        match command_rx.recv_timeout(Duration::from_millis(100)) {
                            Ok(EngineCommand::Shutdown)
                            | Err(std_mpsc::RecvTimeoutError::Disconnected) => break,
                            Ok(EngineCommand::Request { reply, .. }) => {
                                let _ = reply.send(Err(
                                    "MAINTENANCE_IN_PROGRESS: 资料维护期间不能提交 AI 任务".into(),
                                ));
                            }
                            Ok(EngineCommand::TestConnection { reply }) => {
                                let _ = reply.send(Err(
                                    "MAINTENANCE_IN_PROGRESS: 资料维护期间不能测试连接".into(),
                                ));
                            }
                            Ok(EngineCommand::Observe { .. }) => {}
                            Ok(EngineCommand::Quiesce { reply, .. }) => {
                                explicitly_quiesced = true;
                                let _ = reply.send(Ok(()));
                            }
                            Ok(EngineCommand::Resume { reply }) => {
                                if maintenance_active {
                                    let _ = reply.send(Err(
                                        "MAINTENANCE_IN_PROGRESS: 资料维护尚未结束".into(),
                                    ));
                                } else {
                                    explicitly_quiesced = false;
                                    let _ = reply.send(Ok(()));
                                }
                            }
                            Err(std_mpsc::RecvTimeoutError::Timeout) => {}
                        }
                        continue;
                    }
                    match engine_loop(
                        thread_path.clone(),
                        config.clone(),
                        provider_mode.clone(),
                        policy,
                        &command_rx,
                        notice_tx.clone(),
                        &projection_publication,
                    ) {
                        Ok(EngineExit::Maintenance { explicit }) => {
                            projection_publication.clear_materialized();
                            explicitly_quiesced = explicit;
                        }
                        Ok(EngineExit::Shutdown) => break,
                        Err(error)
                            if crate::local_data_maintenance::MaintenanceFence::rejected(
                                &error,
                            ) =>
                        {
                            // The sidecar may become active between this
                            // worker's observation and a fenced write. The
                            // rejected write is the safe point: `db` is
                            // dropped by `engine_loop`, and the outer host now
                            // waits for the explicit participant handshake.
                            projection_publication.clear_materialized();
                            explicitly_quiesced = false;
                        }
                        Err(error) => {
                            let _ = notice_tx.send(KnowledgeNotice::ModuleFault {
                                user_message: "后台知识处理模块已停止".into(),
                                technical_detail: sanitize_detail(&format!("{error:#}")),
                            });
                            break;
                        }
                    }
                }
            })?;
        Ok(Self {
            db_path,
            command_tx,
            notice_rx,
            projection_observer,
            join: Some(join),
        })
    }

    pub(crate) fn request(&self, key: TaskKey) -> Result<RequestReceipt> {
        let (reply_tx, reply_rx) = std_mpsc::channel();
        self.command_tx
            .send(EngineCommand::Request {
                key,
                reply: reply_tx,
            })
            .context("知识处理模块已停止")?;
        reply_rx
            .recv_timeout(Duration::from_secs(2))
            .context("知识处理请求持久化超时")?
            .map_err(anyhow::Error::msg)
    }

    pub(crate) fn snapshot(&self, key: TaskKey) -> Result<Option<TaskSnapshot>> {
        let db = Db::open(&self.db_path)?;
        WorkflowStore::new(&db).latest_snapshot(key)
    }

    pub(crate) fn wait_terminal(&self, key: TaskKey, timeout: Duration) -> Result<TaskSnapshot> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(snapshot) = self.snapshot(key)?
                && snapshot.status.is_terminal()
            {
                return Ok(snapshot);
            }
            if Instant::now() >= deadline {
                bail!("WORKFLOW_WAIT_TIMEOUT: task remains queued or running");
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    pub(crate) fn test_connection(&self) -> Result<()> {
        let (reply_tx, reply_rx) = std_mpsc::channel();
        self.command_tx
            .send(EngineCommand::TestConnection { reply: reply_tx })
            .context("知识处理模块已停止")?;
        reply_rx
            .recv_timeout(Duration::from_secs(2))
            .context("连接测试请求超时")?
            .map_err(anyhow::Error::msg)
    }

    pub(crate) fn try_notices(&self) -> impl Iterator<Item = KnowledgeNotice> + '_ {
        std::iter::from_fn(|| self.notice_rx.try_recv().ok())
    }

    pub(crate) fn projection_observer(&self) -> KnowledgeProjectionObserver {
        self.projection_observer.clone()
    }

    pub(crate) fn maintenance_participant(&self) -> Arc<dyn MaintenanceParticipant> {
        Arc::new(KnowledgeMaintenanceParticipant {
            command_tx: self.command_tx.clone(),
        })
    }
}

impl crate::resource_library_lifecycle::ProcessingHandoff for KnowledgeEngine {
    fn request_resource_processing(&self, resource_id: i64) -> Result<(), String> {
        self.request(TaskKey::new(TaskKind::ResourceCompletion, resource_id))
            .map(|_| ())
            .map_err(|error| failure::sanitize_detail(&format!("{error:#}")))
    }
}

impl Drop for KnowledgeEngine {
    fn drop(&mut self) {
        let _ = self.command_tx.send(EngineCommand::Shutdown);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

#[derive(Debug, Clone)]
struct ClaimedTask {
    id: i64,
    key: TaskKey,
    generation: i64,
}

struct WorkflowStore<'a> {
    db: &'a Db,
}

impl<'a> WorkflowStore<'a> {
    fn new(db: &'a Db) -> Self {
        Self { db }
    }

    fn request(&self, key: TaskKey, now: i64) -> Result<RequestReceipt> {
        let tx = self.db.fenced_transaction()?;
        Self::validate_target_on(&tx, key)?;
        let latest: Option<(i64, String)> = tx
            .query_row(
                "SELECT id,status FROM knowledge_tasks WHERE kind=?1 AND target_id=?2
                 ORDER BY created_at DESC,id DESC LIMIT 1",
                params![key.kind.as_str(), key.target_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let disposition = match latest {
            Some((_task_id, ref status)) if status == "queued" || status == "running" => {
                RequestDisposition::Existing
            }
            Some((task_id, ref status)) if status == "failed" || status == "interrupted" => {
                let next: i64 = tx.query_row(
                    "SELECT COALESCE(MAX(attempt_number),0)+1 FROM knowledge_task_attempts WHERE task_id=?1",
                    [task_id],
                    |row| row.get(0),
                )?;
                tx.execute(
                    "UPDATE knowledge_tasks SET status='queued',current_stage=NULL,next_run_at=?2,updated_at=?2 WHERE id=?1",
                    params![task_id, now],
                )?;
                tx.execute(
                    "INSERT INTO knowledge_task_attempts(task_id,attempt_number,status,created_at)
                     VALUES(?1,?2,'queued',?3)",
                    params![task_id, next, now],
                )?;
                bump_change(&tx, task_id)?;
                RequestDisposition::Retried
            }
            _ => {
                tx.execute(
                    "INSERT INTO knowledge_tasks(kind,target_id,status,next_run_at,created_at,updated_at)
                     VALUES(?1,?2,'queued',?3,?3,?3)",
                    params![key.kind.as_str(), key.target_id, now],
                )?;
                let task_id = tx.last_insert_rowid();
                tx.execute(
                    "INSERT INTO knowledge_task_attempts(task_id,attempt_number,status,created_at)
                     VALUES(?1,1,'queued',?2)",
                    params![task_id, now],
                )?;
                bump_change(&tx, task_id)?;
                RequestDisposition::Created
            }
        };
        tx.commit()?;
        Ok(RequestReceipt { key, disposition })
    }

    fn validate_target_on(conn: &rusqlite::Connection, key: TaskKey) -> Result<()> {
        let table = match key.kind {
            TaskKind::ResourceCompletion => "resources",
            TaskKind::ArticleSummary => "articles",
        };
        let exists: bool = conn.query_row(
            &format!("SELECT EXISTS(SELECT 1 FROM {table} WHERE id=?1)"),
            [key.target_id],
            |row| row.get(0),
        )?;
        if !exists {
            bail!(
                "WORKFLOW_TARGET_NOT_FOUND: {:?} {}",
                key.kind,
                key.target_id
            );
        }
        Ok(())
    }

    fn latest_snapshot(&self, key: TaskKey) -> Result<Option<TaskSnapshot>> {
        self.db
            .conn
            .query_row(
                "SELECT t.id,t.kind,t.target_id,t.status,t.current_stage,t.change_seq,
                        a.attempt_number,a.automatic_retry,a.error_kind,a.user_message,a.technical_detail
                 FROM knowledge_tasks t
                 JOIN knowledge_task_attempts a ON a.id=(
                   SELECT id FROM knowledge_task_attempts WHERE task_id=t.id
                   ORDER BY attempt_number DESC,id DESC LIMIT 1)
                 WHERE t.kind=?1 AND t.target_id=?2
                 ORDER BY t.created_at DESC,t.id DESC LIMIT 1",
                params![key.kind.as_str(), key.target_id],
                map_snapshot,
            )
            .optional()
            .map_err(Into::into)
    }

    fn current_clock(&self) -> Result<i64> {
        Ok(self.db.conn.query_row(
            "SELECT sequence FROM knowledge_change_clock WHERE singleton_id=1",
            [],
            |row| row.get(0),
        )?)
    }

    fn changes_after(&self, cursor: i64) -> Result<Vec<(i64, TaskKey)>> {
        let mut stmt = self.db.conn.prepare(
            "SELECT change_seq,kind,target_id FROM knowledge_tasks
             WHERE change_seq>?1 ORDER BY change_seq,id",
        )?;
        let rows = stmt.query_map([cursor], |row| {
            let kind: String = row.get(1)?;
            Ok((
                row.get(0)?,
                TaskKey::new(
                    TaskKind::parse(&kind).map_err(sql_conversion_error)?,
                    row.get(2)?,
                ),
            ))
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    fn acquire_lease(&self, owner: &str, now: i64) -> Result<Option<i64>> {
        let tx = match self.db.fenced_transaction() {
            Ok(tx) => tx,
            Err(error) if crate::local_data_maintenance::MaintenanceFence::rejected(&error) => {
                return Ok(None);
            }
            Err(error) => return Err(error),
        };
        let (current_owner, generation, heartbeat): (Option<String>, i64, i64) = tx.query_row(
            "SELECT owner_id,generation,heartbeat_at FROM knowledge_executor_lease WHERE singleton_id=1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        if current_owner.as_deref() == Some(owner) {
            tx.execute(
                "UPDATE knowledge_executor_lease SET heartbeat_at=?2 WHERE singleton_id=1 AND owner_id=?1 AND generation=?3",
                params![owner, now, generation],
            )?;
            tx.commit()?;
            return Ok(Some(generation));
        }
        if current_owner.is_none() || heartbeat <= now - LEASE_EXPIRES_SECS {
            let next = generation + 1;
            let changed = tx.execute(
                "UPDATE knowledge_executor_lease
                 SET owner_id=?1,generation=?2,heartbeat_at=?3
                 WHERE singleton_id=1 AND generation=?4
                   AND (owner_id IS NULL OR heartbeat_at<=?5)",
                params![owner, next, now, generation, now - LEASE_EXPIRES_SECS],
            )?;
            tx.commit()?;
            return Ok((changed == 1).then_some(next));
        }
        tx.commit()?;
        Ok(None)
    }

    fn heartbeat(&self, owner: &str, generation: i64, now: i64) -> Result<bool> {
        // Heartbeat does not claim work. Allowing it during the drain race
        // preserves the generation so the next observation can release the
        // lease at the participant safe point.
        let tx = self.db.maintenance_drain_transaction()?;
        let owned = tx.execute(
            "UPDATE knowledge_executor_lease SET heartbeat_at=?3
             WHERE singleton_id=1 AND owner_id=?1 AND generation=?2",
            params![owner, generation, now],
        )? == 1;
        tx.commit()?;
        Ok(owned)
    }

    fn owns_lease(&self, owner: &str, generation: i64) -> Result<bool> {
        Ok(self.db.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM knowledge_executor_lease
             WHERE singleton_id=1 AND owner_id=?1 AND generation=?2)",
            params![owner, generation],
            |row| row.get(0),
        )?)
    }

    fn release_lease(&self, owner: &str, generation: i64) -> Result<()> {
        let tx = self.db.maintenance_drain_transaction()?;
        tx.execute(
            "UPDATE knowledge_executor_lease SET owner_id=NULL,heartbeat_at=0
             WHERE singleton_id=1 AND owner_id=?1 AND generation=?2",
            params![owner, generation],
        )?;
        tx.commit()
    }

    fn interrupt_stale_running(&self, generation: i64, now: i64) -> Result<usize> {
        let tx = self.db.maintenance_drain_transaction()?;
        let mut stmt = tx.prepare(
            "SELECT DISTINCT t.id FROM knowledge_tasks t
             JOIN knowledge_task_attempts a ON a.task_id=t.id
             WHERE t.status='running' AND a.status='running'
               AND (a.claim_generation IS NULL OR a.claim_generation<>?1)",
        )?;
        let task_ids = stmt
            .query_map([generation], |row| row.get::<_, i64>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(stmt);
        for task_id in &task_ids {
            tx.execute(
                "UPDATE knowledge_task_attempts SET status='interrupted',finished_at=?2,
                 error_kind='interrupted',user_message='执行器失联，任务已中断',
                 technical_detail='EXECUTOR_INTERRUPTED: lease generation was superseded'
                 WHERE task_id=?1 AND status='running'",
                params![task_id, now],
            )?;
            tx.execute(
                "UPDATE resource_enrichment_runs SET status='failed',finished_at=?2,
                 error_code='EXECUTOR_INTERRUPTED',error_message='executor lease was superseded'
                 WHERE attempt_id IN (SELECT id FROM knowledge_task_attempts WHERE task_id=?1)
                   AND status IN ('pending','running')",
                params![task_id, now],
            )?;
            tx.execute(
                "UPDATE knowledge_tasks SET status='interrupted',current_stage=NULL,updated_at=?2 WHERE id=?1",
                params![task_id, now],
            )?;
            bump_change(&tx, *task_id)?;
        }
        tx.commit()?;
        Ok(task_ids.len())
    }

    fn interrupt_generation(&self, owner: &str, generation: i64, now: i64) -> Result<()> {
        let tx = self.db.maintenance_drain_transaction()?;
        if !lease_owned_on(&tx, owner, generation)? {
            tx.commit()?;
            return Ok(());
        }
        let mut stmt = tx.prepare(
            "SELECT DISTINCT task_id FROM knowledge_task_attempts
             WHERE status='running' AND claim_generation=?1",
        )?;
        let ids = stmt
            .query_map([generation], |row| row.get::<_, i64>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(stmt);
        for task_id in ids {
            tx.execute(
                "UPDATE knowledge_task_attempts SET status='interrupted',finished_at=?2,
                 error_kind='interrupted',user_message='任务被程序退出中断',
                 technical_detail='EXECUTOR_INTERRUPTED: owner shut down'
                 WHERE task_id=?1 AND status='running' AND claim_generation=?3",
                params![task_id, now, generation],
            )?;
            tx.execute(
                "UPDATE knowledge_tasks SET status='interrupted',current_stage=NULL,updated_at=?2 WHERE id=?1",
                params![task_id, now],
            )?;
            tx.execute(
                "UPDATE resource_enrichment_runs SET status='failed',finished_at=?2,
                 error_code='EXECUTOR_INTERRUPTED',error_message='executor owner shut down'
                 WHERE attempt_id IN (SELECT id FROM knowledge_task_attempts WHERE task_id=?1)
                   AND status IN ('pending','running')",
                params![task_id, now],
            )?;
            bump_change(&tx, task_id)?;
        }
        tx.commit()?;
        Ok(())
    }

    fn claim_next(&self, owner: &str, generation: i64, now: i64) -> Result<Option<ClaimedTask>> {
        let tx = match self.db.fenced_transaction() {
            Ok(tx) => tx,
            Err(error) if crate::local_data_maintenance::MaintenanceFence::rejected(&error) => {
                return Ok(None);
            }
            Err(error) => return Err(error),
        };
        if !lease_owned_on(&tx, owner, generation)? {
            tx.commit()?;
            return Ok(None);
        }
        let selected: Option<(i64, String, i64)> = tx
            .query_row(
                "SELECT id,kind,target_id FROM knowledge_tasks
                 WHERE status='queued' AND next_run_at<=?1
                 ORDER BY next_run_at,created_at,id LIMIT 1",
                [now],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        let Some((task_id, kind, target_id)) = selected else {
            tx.commit()?;
            return Ok(None);
        };
        let task_kind = TaskKind::parse(&kind)?;
        let stage = match task_kind {
            TaskKind::ResourceCompletion => TaskStage::Fetching,
            TaskKind::ArticleSummary => TaskStage::Summarizing,
        };
        let changed = tx.execute(
            "UPDATE knowledge_tasks SET status='running',current_stage=?2,updated_at=?3
             WHERE id=?1 AND status='queued'",
            params![task_id, stage.as_str(), now],
        )?;
        if changed == 0 {
            tx.commit()?;
            return Ok(None);
        }
        tx.execute(
            "UPDATE knowledge_task_attempts
             SET status='running',current_stage=?2,started_at=?3,claim_generation=?4
             WHERE id=(SELECT id FROM knowledge_task_attempts
                       WHERE task_id=?1 AND status='queued'
                       ORDER BY attempt_number DESC,id DESC LIMIT 1)",
            params![task_id, stage.as_str(), now, generation],
        )?;
        bump_change(&tx, task_id)?;
        tx.commit()?;
        Ok(Some(ClaimedTask {
            id: task_id,
            key: TaskKey::new(task_kind, target_id),
            generation,
        }))
    }

    fn advance(&self, owner: &str, task: &ClaimedTask, stage: TaskStage, now: i64) -> Result<bool> {
        let tx = self.db.fenced_transaction()?;
        if !fence_valid(&tx, owner, task.id, task.generation)? {
            tx.commit()?;
            return Ok(false);
        }
        tx.execute(
            "UPDATE knowledge_tasks SET current_stage=?2,updated_at=?3 WHERE id=?1",
            params![task.id, stage.as_str(), now],
        )?;
        tx.execute(
            "UPDATE knowledge_task_attempts SET current_stage=?2
             WHERE task_id=?1 AND status='running' AND claim_generation=?3",
            params![task.id, stage.as_str(), task.generation],
        )?;
        bump_change(&tx, task.id)?;
        tx.commit()?;
        Ok(true)
    }

    fn record_snapshot_and_advance(
        &self,
        owner: &str,
        task: &ClaimedTask,
        input: &SnapshotInput,
        now: i64,
    ) -> Result<bool> {
        let tx = self.db.fenced_transaction()?;
        if !fence_valid(&tx, owner, task.id, task.generation)? {
            tx.commit()?;
            return Ok(false);
        }
        let content = input.cleaned_content.as_deref().unwrap_or_default();
        let hash = format!("{:x}", Sha256::digest(content.as_bytes()));
        let existing: Option<i64> = tx
            .query_row(
                "SELECT id FROM resource_snapshots WHERE resource_id=?1 AND content_hash=?2",
                params![task.key.target_id, hash],
                |row| row.get(0),
            )
            .optional()?;
        let snapshot_id = if let Some(id) = existing {
            id
        } else {
            tx.execute(
                "INSERT INTO resource_snapshots(resource_id,content_hash,fetched_url,http_status,title,cleaned_content,fetched_at)
                 VALUES(?1,?2,?3,?4,?5,?6,?7)",
                params![task.key.target_id,hash,input.fetched_url,input.http_status,input.title,input.cleaned_content,now],
            )?;
            tx.last_insert_rowid()
        };
        resource_target::record_snapshot_success(&tx, task.key.target_id, snapshot_id, input, now)?;
        tx.execute(
            "UPDATE knowledge_tasks SET current_stage='organizing',updated_at=?2 WHERE id=?1",
            params![task.id, now],
        )?;
        tx.execute(
            "UPDATE knowledge_task_attempts SET current_stage='organizing'
             WHERE task_id=?1 AND status='running' AND claim_generation=?2",
            params![task.id, task.generation],
        )?;
        bump_change(&tx, task.id)?;
        tx.commit()?;
        Ok(true)
    }

    fn start_enrichment(
        &self,
        owner: &str,
        task: &ClaimedTask,
        config: &ResourceEnrichmentConfig,
        snapshot_id: Option<i64>,
        now: i64,
    ) -> Result<Option<i64>> {
        let tx = self.db.fenced_transaction()?;
        if !fence_valid(&tx, owner, task.id, task.generation)? {
            tx.commit()?;
            return Ok(None);
        }
        let attempt_id: i64 = tx.query_row(
            "SELECT id FROM knowledge_task_attempts
             WHERE task_id=?1 AND status='running' AND claim_generation=?2",
            params![task.id, task.generation],
            |row| row.get(0),
        )?;
        tx.execute(
            "INSERT INTO resource_enrichment_runs(
               resource_id,snapshot_id,provider,model,prompt_version,schema_version,
               started_at,status,attempt_id)
             VALUES(?1,?2,?3,?4,?5,?6,?7,'running',?8)",
            params![
                task.key.target_id,
                snapshot_id,
                config.provider,
                config.model,
                config.prompt_version,
                config.schema_version,
                now,
                attempt_id
            ],
        )?;
        let run_id = tx.last_insert_rowid();
        tx.commit()?;
        Ok(Some(run_id))
    }

    fn complete_resource(
        &self,
        owner: &str,
        task: &ClaimedTask,
        run_id: i64,
        output: &EnrichmentOutput,
        now: i64,
    ) -> Result<bool> {
        let tx = self.db.fenced_transaction()?;
        if !fence_valid(&tx, owner, task.id, task.generation)? {
            tx.commit()?;
            return Ok(false);
        }
        resource_target::apply_enrichment(&tx, task.key.target_id, output, now)?;
        tx.execute(
            "UPDATE resource_enrichment_runs SET status='succeeded',finished_at=?2
             WHERE id=?1",
            params![run_id, now],
        )?;
        finish_success_on(&tx, task, now)?;
        tx.commit()?;
        Ok(true)
    }

    fn complete_resource_without_enrichment(
        &self,
        owner: &str,
        task: &ClaimedTask,
        now: i64,
    ) -> Result<bool> {
        let tx = self.db.fenced_transaction()?;
        if !fence_valid(&tx, owner, task.id, task.generation)? {
            tx.commit()?;
            return Ok(false);
        }
        finish_success_on(&tx, task, now)?;
        tx.commit()?;
        Ok(true)
    }

    fn complete_article(
        &self,
        owner: &str,
        task: &ClaimedTask,
        summary: &str,
        translation: &str,
        model: &str,
        now: i64,
    ) -> Result<bool> {
        let tx = self.db.fenced_transaction()?;
        if !fence_valid(&tx, owner, task.id, task.generation)? {
            tx.commit()?;
            return Ok(false);
        }
        let existing = tx
            .query_row(
                "SELECT summary_zh,translation_zh,model FROM article_ai WHERE article_id=?1",
                [task.key.target_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                },
            )
            .optional()?;
        let article_changed = existing.as_ref().is_none_or(
            |(current_summary, current_translation, current_model)| {
                current_summary != summary
                    || current_translation != translation
                    || current_model != model
            },
        );
        if article_changed {
            tx.execute(
                "INSERT INTO article_ai(article_id,summary_zh,translation_zh,model,updated_at)
                 VALUES(?1,?2,?3,?4,?5)
                 ON CONFLICT(article_id) DO UPDATE SET summary_zh=excluded.summary_zh,
                 translation_zh=excluded.translation_zh,model=excluded.model,updated_at=excluded.updated_at",
                params![task.key.target_id, summary, translation, model, now],
            )?;
            library_projection_revision::record(&tx, ProjectionImpact::article())?;
        }
        finish_success_on(&tx, task, now)?;
        tx.commit()?;
        Ok(true)
    }

    fn fail(
        &self,
        owner: &str,
        task: &ClaimedTask,
        run_id: Option<i64>,
        error: &anyhow::Error,
        now: i64,
    ) -> Result<bool> {
        let (kind, user_message) = classify_error(error);
        let detail = sanitize_detail(&format!("{error:#}"));
        let tx = self.db.fenced_transaction()?;
        if !fence_valid(&tx, owner, task.id, task.generation)? {
            tx.commit()?;
            return Ok(false);
        }
        if task.key.kind == TaskKind::ResourceCompletion {
            let stage: Option<String> = tx.query_row(
                "SELECT current_stage FROM knowledge_tasks WHERE id=?1",
                [task.id],
                |row| row.get(0),
            )?;
            if stage.as_deref() == Some("fetching") {
                resource_target::record_fetch_failure(
                    &tx,
                    task.key.target_id,
                    kind.as_str(),
                    &detail,
                    now,
                )?;
            }
        }
        tx.execute(
            "UPDATE knowledge_task_attempts SET status='failed',finished_at=?2,
             error_kind=?3,user_message=?4,technical_detail=?5
             WHERE task_id=?1 AND status='running' AND claim_generation=?6",
            params![
                task.id,
                now,
                kind.as_str(),
                user_message,
                detail,
                task.generation
            ],
        )?;
        if let Some(run_id) = run_id {
            tx.execute(
                "UPDATE resource_enrichment_runs SET status='failed',finished_at=?2,
                 error_code=?3,error_message=?4 WHERE id=?1",
                params![run_id, now, kind.code(), detail],
            )?;
        } else {
            tx.execute(
                "UPDATE resource_enrichment_runs SET status='failed',finished_at=?2,
                 error_code=?3,error_message=?4
                 WHERE attempt_id=(SELECT id FROM knowledge_task_attempts
                   WHERE task_id=?1 AND claim_generation=?5 ORDER BY attempt_number DESC LIMIT 1)
                   AND status IN ('pending','running')",
                params![task.id, now, kind.code(), detail, task.generation],
            )?;
        }
        let automatic_count: i64 = tx.query_row(
            "SELECT COUNT(*) FROM knowledge_task_attempts WHERE task_id=?1 AND automatic_retry=1",
            [task.id],
            |row| row.get(0),
        )?;
        if kind == ErrorKind::Transient && automatic_count < MAX_AUTOMATIC_RETRIES {
            let next: i64 = tx.query_row(
                "SELECT COALESCE(MAX(attempt_number),0)+1 FROM knowledge_task_attempts WHERE task_id=?1",
                [task.id],
                |row| row.get(0),
            )?;
            let delay = 1_i64 << automatic_count;
            tx.execute(
                "UPDATE knowledge_tasks SET status='queued',current_stage=NULL,next_run_at=?2,updated_at=?3 WHERE id=?1",
                params![task.id, now + delay, now],
            )?;
            tx.execute(
                "INSERT INTO knowledge_task_attempts(task_id,attempt_number,status,automatic_retry,created_at)
                 VALUES(?1,?2,'queued',1,?3)",
                params![task.id, next, now],
            )?;
        } else {
            tx.execute(
                "UPDATE knowledge_tasks SET status='failed',updated_at=?2 WHERE id=?1",
                params![task.id, now],
            )?;
        }
        bump_change(&tx, task.id)?;
        tx.commit()?;
        Ok(true)
    }

    fn prune_terminal_history(&self, now: i64) -> Result<()> {
        const RETENTION_SECONDS: i64 = 90 * 24 * 60 * 60;
        let tx = self.db.fenced_transaction()?;
        tx.execute(
            "DELETE FROM knowledge_tasks
             WHERE status IN ('succeeded','failed','interrupted') AND updated_at<?1",
            [now - RETENTION_SECONDS],
        )?;
        tx.commit()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EngineExit {
    Shutdown,
    Maintenance { explicit: bool },
}

struct ProjectionPublication {
    state: Arc<RwLock<KnowledgeProjectionState>>,
    notice_tx: std_mpsc::SyncSender<TaskKey>,
}

impl ProjectionPublication {
    fn residents(&self) -> Vec<TaskKey> {
        self.state
            .read()
            .expect("knowledge projection state poisoned")
            .residents
            .iter()
            .copied()
            .collect()
    }

    fn is_resident(&self, key: TaskKey) -> bool {
        self.state
            .read()
            .expect("knowledge projection state poisoned")
            .residents
            .contains(&key)
    }

    fn clear_materialized(&self) {
        self.state
            .write()
            .expect("knowledge projection state poisoned")
            .snapshots
            .clear();
    }
}

fn engine_loop(
    db_path: PathBuf,
    config: ResourceEnrichmentConfig,
    provider_mode: ProviderMode,
    policy: ExecutorPolicy,
    command_rx: &std_mpsc::Receiver<EngineCommand>,
    notice_tx: std_mpsc::Sender<KnowledgeNotice>,
    projection: &ProjectionPublication,
) -> Result<EngineExit> {
    let db = match Db::open(&db_path) {
        Ok(db) => db,
        Err(error) if error.to_string().contains("MAINTENANCE_IN_PROGRESS") => {
            return Ok(EngineExit::Maintenance { explicit: false });
        }
        Err(error) => return Err(error),
    };
    let store = WorkflowStore::new(&db);
    store.prune_terminal_history(now())?;
    let observed = projection.residents();
    for key in observed {
        publish_projection_snapshot(&store, key, projection)?;
    }
    let owner_id = owner_id();
    let mut generation = None;
    let mut last_lease_attempt = Instant::now() - Duration::from_secs(2);
    let mut last_heartbeat = Instant::now();
    let mut cursor = store.current_clock()?;
    let mut active = HashMap::<i64, TaskKey>::new();
    let (done_tx, done_rx) = std_mpsc::channel();
    let mut connection_running = false;
    let mut shutting_down = false;
    let mut maintenance_requested = false;
    let mut explicit_maintenance = false;
    let mut quiesce_reply: Option<std_mpsc::Sender<Result<(), String>>> = None;
    let mut quiesce_force_at = None;
    let mut shutdown_started = None;
    let mut watcher_faulted = false;
    let mut last_watch = Instant::now() - Duration::from_secs(1);

    loop {
        if !shutting_down
            && crate::local_data_maintenance::MaintenanceFence::observe(&db_path)?.is_active()
        {
            shutting_down = true;
            maintenance_requested = true;
        }
        while let Ok(done) = done_rx.try_recv() {
            match done {
                WorkDone::Task(task_id) => {
                    active.remove(&task_id);
                }
                WorkDone::Connection {
                    generation: result_generation,
                    result,
                } => {
                    connection_running = false;
                    let state = if generation == Some(result_generation)
                        && store
                            .owns_lease(&owner_id, result_generation)
                            .unwrap_or(false)
                    {
                        match result {
                            Ok(message) => ConnectionState::Succeeded(message),
                            Err(detail) => ConnectionState::Failed { detail },
                        }
                    } else {
                        ConnectionState::Failed {
                            detail: "执行权已转移，请重新测试连接".into(),
                        }
                    };
                    let _ = notice_tx.send(KnowledgeNotice::ConnectionChanged(state));
                }
            }
        }

        let watch_interval = if generation.is_some() || !active.is_empty() || connection_running {
            Duration::from_millis(200)
        } else {
            Duration::from_secs(1)
        };
        if last_watch.elapsed() >= watch_interval {
            last_watch = Instant::now();
            match store.changes_after(cursor) {
                Ok(changes) => {
                    if watcher_faulted {
                        watcher_faulted = false;
                        let _ = notice_tx.send(KnowledgeNotice::ModuleFault {
                            user_message: "后台任务状态观察已恢复".into(),
                            technical_detail: "knowledge change watcher recovered".into(),
                        });
                    }
                    let mut coalesced = HashSet::new();
                    for (sequence, key) in changes {
                        cursor = cursor.max(sequence);
                        coalesced.insert(key);
                    }
                    for key in coalesced {
                        if projection.is_resident(key) {
                            publish_projection_snapshot(&store, key, projection)?;
                        }
                        let _ = notice_tx.send(KnowledgeNotice::Changed(key));
                    }
                }
                Err(error) if !watcher_faulted => {
                    watcher_faulted = true;
                    let _ = notice_tx.send(KnowledgeNotice::ModuleFault {
                        user_message: "暂时无法读取后台任务状态，正在重试".into(),
                        technical_detail: sanitize_detail(&format!("{error:#}")),
                    });
                }
                Err(_) => {}
            }
        }

        if !shutting_down && policy == ExecutorPolicy::Acquire {
            if generation.is_none() && last_lease_attempt.elapsed() >= Duration::from_secs(1) {
                last_lease_attempt = Instant::now();
                if let Some(acquired) = store.acquire_lease(&owner_id, now())? {
                    store.interrupt_stale_running(acquired, now())?;
                    generation = Some(acquired);
                    last_heartbeat = Instant::now();
                }
            }
            if let Some(current) = generation
                && last_heartbeat.elapsed() >= Duration::from_secs(LEASE_HEARTBEAT_SECS as u64)
            {
                if store.heartbeat(&owner_id, current, now())? {
                    last_heartbeat = Instant::now();
                } else {
                    generation = None;
                }
            }
        }

        if shutting_down {
            let elapsed = shutdown_started.get_or_insert_with(Instant::now).elapsed();
            if active.is_empty() && !connection_running {
                if let Some(current) = generation {
                    store.release_lease(&owner_id, current)?;
                }
                drop(db);
                if maintenance_requested {
                    projection.clear_materialized();
                }
                if let Some(reply) = quiesce_reply.take() {
                    let _ = reply.send(Ok(()));
                }
                return Ok(if maintenance_requested {
                    EngineExit::Maintenance {
                        explicit: explicit_maintenance,
                    }
                } else {
                    EngineExit::Shutdown
                });
            }
            let force_safe_point = quiesce_force_at
                .is_some_and(|deadline| Instant::now() >= deadline)
                || (quiesce_force_at.is_none() && elapsed >= Duration::from_secs(2));
            if force_safe_point {
                if let Some(current) = generation {
                    store.interrupt_generation(&owner_id, current, now())?;
                    store.release_lease(&owner_id, current)?;
                }
                drop(db);
                if maintenance_requested {
                    projection.clear_materialized();
                }
                if let Some(reply) = quiesce_reply.take() {
                    let _ = reply.send(Ok(()));
                }
                return Ok(if maintenance_requested {
                    EngineExit::Maintenance {
                        explicit: explicit_maintenance,
                    }
                } else {
                    EngineExit::Shutdown
                });
            }
        } else if let Some(current) = generation {
            while active.len() + usize::from(connection_running) < MAX_CONCURRENCY {
                let Some(task) = store.claim_next(&owner_id, current, now())? else {
                    break;
                };
                active.insert(task.id, task.key);
                spawn_task(
                    db_path.clone(),
                    owner_id.clone(),
                    task,
                    config.clone(),
                    provider_mode.clone(),
                    done_tx.clone(),
                );
            }
        }

        match command_rx.recv_timeout(Duration::from_millis(50)) {
            Ok(EngineCommand::Request { key, reply }) => {
                if maintenance_requested {
                    let _ = reply.send(Err(
                        "MAINTENANCE_IN_PROGRESS: 资料维护期间不能提交 AI 任务".into(),
                    ));
                } else {
                    let result = store.request(key, now());
                    let _ =
                        reply.send(result.map_err(|error| sanitize_detail(&format!("{error:#}"))));
                }
            }
            Ok(EngineCommand::TestConnection { reply }) => {
                let result = if maintenance_requested {
                    Err("MAINTENANCE_IN_PROGRESS: 资料维护期间不能测试连接".into())
                } else {
                    match generation {
                        Some(current) if !connection_running => {
                            connection_running = true;
                            spawn_connection_test(
                                config.clone(),
                                provider_mode.clone(),
                                current,
                                done_tx.clone(),
                            );
                            let _ = notice_tx
                                .send(KnowledgeNotice::ConnectionChanged(ConnectionState::Running));
                            Ok(())
                        }
                        Some(_) => Ok(()),
                        None => Err("连接测试正由另一个拾阅进程管理，请在主界面重试".into()),
                    }
                };
                let _ = reply.send(result);
            }
            Ok(EngineCommand::Observe { key }) => {
                publish_projection_snapshot(&store, key, projection)?;
            }
            Ok(EngineCommand::Quiesce { deadline, reply }) => {
                if quiesce_reply.is_some() {
                    let _ = reply.send(Err(
                        "KNOWLEDGE_QUIESCE_ALREADY_PENDING: safe point request already exists"
                            .into(),
                    ));
                } else {
                    shutting_down = true;
                    maintenance_requested = true;
                    explicit_maintenance = true;
                    shutdown_started.get_or_insert_with(Instant::now);
                    quiesce_force_at =
                        Some(deadline.checked_sub(QUIESCE_ACK_BUDGET).unwrap_or(deadline));
                    quiesce_reply = Some(reply);
                }
            }
            Ok(EngineCommand::Resume { reply }) => {
                let result = if maintenance_requested {
                    Err("KNOWLEDGE_QUIESCE_IN_PROGRESS: safe point is not ready".into())
                } else {
                    Ok(())
                };
                let _ = reply.send(result);
            }
            Ok(EngineCommand::Shutdown) | Err(std_mpsc::RecvTimeoutError::Disconnected) => {
                shutting_down = true;
            }
            Err(std_mpsc::RecvTimeoutError::Timeout) => {}
        }
    }
}

fn publish_projection_snapshot(
    store: &WorkflowStore<'_>,
    key: TaskKey,
    projection: &ProjectionPublication,
) -> Result<()> {
    let snapshot = store.latest_snapshot(key)?;
    let mut state = projection
        .state
        .write()
        .expect("knowledge projection state poisoned");
    if !state.residents.contains(&key) {
        return Ok(());
    }
    state.snapshots.insert(key, snapshot);
    drop(state);
    let _ = projection.notice_tx.try_send(key);
    Ok(())
}

fn spawn_task(
    db_path: PathBuf,
    owner_id: String,
    task: ClaimedTask,
    config: ResourceEnrichmentConfig,
    provider_mode: ProviderMode,
    done_tx: std_mpsc::Sender<WorkDone>,
) {
    std::thread::spawn(move || {
        let result = execute_task(&db_path, &owner_id, &task, &config, &provider_mode);
        if let Err(error) = result
            && let Ok(db) = Db::open(&db_path)
        {
            let store = WorkflowStore::new(&db);
            let _ = store.fail(&owner_id, &task, None, &error, now());
        }
        let _ = done_tx.send(WorkDone::Task(task.id));
    });
}

fn execute_task(
    db_path: &Path,
    owner_id: &str,
    task: &ClaimedTask,
    config: &ResourceEnrichmentConfig,
    provider_mode: &ProviderMode,
) -> Result<()> {
    match task.key.kind {
        TaskKind::ResourceCompletion => {
            execute_resource(db_path, owner_id, task, config, provider_mode)
        }
        TaskKind::ArticleSummary => execute_article(db_path, owner_id, task, config, provider_mode),
    }
}

fn execute_resource(
    db_path: &Path,
    owner_id: &str,
    task: &ClaimedTask,
    config: &ResourceEnrichmentConfig,
    provider_mode: &ProviderMode,
) -> Result<()> {
    let mut resource = {
        let db = Db::open(db_path)?;
        resource_target::load(&db, task.key.target_id).context("目标 Resource 不存在")?
    };
    if resource.latest_snapshot_id.is_none() && resource.linked_article_id.is_none() {
        // Never hold a database writer lease while waiting on the network.
        let fetched = crate::web_clip::client()
            .and_then(|client| crate::web_clip::fetch_html(&client, &resource.url))
            .context("资源网页抓取失败")?;
        let snapshot = prepare_article_html(&fetched.html);
        if snapshot.content.trim().is_empty() {
            bail!("资源网页没有可处理正文");
        }
        let db = Db::open(db_path)?;
        let store = WorkflowStore::new(&db);
        if !store.record_snapshot_and_advance(
            owner_id,
            task,
            &SnapshotInput {
                fetched_url: Some(fetched.final_url),
                http_status: Some(200),
                title: snapshot.title,
                cleaned_content: Some(snapshot.content),
                fetch_error: None,
            },
            now(),
        )? {
            return Ok(());
        }
        resource = resource_target::load(&db, resource.id)?;
    } else {
        let db = Db::open(db_path)?;
        if !WorkflowStore::new(&db).advance(owner_id, task, TaskStage::Organizing, now())? {
            return Ok(());
        }
    }
    if resource.privacy == ResourcePrivacy::Private {
        let db = Db::open(db_path)?;
        WorkflowStore::new(&db).complete_resource_without_enrichment(owner_id, task, now())?;
        return Ok(());
    }
    if !config.enabled {
        bail!("AI 资源整理未启用");
    }
    let (input, run_id) = {
        let db = Db::open(db_path)?;
        let input = resource_target::enrichment_input(&db, resource.id)?
            .context("Resource 缺少可整理内容")?;
        let run_id = WorkflowStore::new(&db)
            .start_enrichment(owner_id, task, config, resource.latest_snapshot_id, now())?
            .context("EXECUTOR_FENCE_LOST")?;
        (input, run_id)
    };
    let output = match with_provider(config, provider_mode, |provider| {
        crate::resource_enrichment::enrich_with(provider, &input, config.max_input_chars)
    }) {
        Ok(output) => output,
        Err(error) => {
            let db = Db::open(db_path)?;
            WorkflowStore::new(&db).fail(owner_id, task, Some(run_id), &error, now())?;
            return Ok(());
        }
    };
    let db = Db::open(db_path)?;
    WorkflowStore::new(&db).complete_resource(owner_id, task, run_id, &output, now())?;
    Ok(())
}

fn execute_article(
    db_path: &Path,
    owner_id: &str,
    task: &ClaimedTask,
    config: &ResourceEnrichmentConfig,
    provider_mode: &ProviderMode,
) -> Result<()> {
    let article = {
        let db = Db::open(db_path)?;
        db.get_article(task.key.target_id)
            .context("目标 Article 不存在")?
    };
    let output = with_provider(config, provider_mode, |provider| {
        crate::resource_enrichment::summarize_and_translate(
            provider,
            article.title.as_deref().unwrap_or(""),
            article.content.as_deref().unwrap_or(""),
            config.max_input_chars,
        )
    })?;
    let db = Db::open(db_path)?;
    WorkflowStore::new(&db).complete_article(
        owner_id,
        task,
        &output.summary_zh,
        &output.translation_zh,
        &config.model,
        now(),
    )?;
    Ok(())
}

fn spawn_connection_test(
    config: ResourceEnrichmentConfig,
    provider_mode: ProviderMode,
    generation: i64,
    done_tx: std_mpsc::Sender<WorkDone>,
) {
    std::thread::spawn(move || {
        let result = with_provider(&config, &provider_mode, |provider| {
            let raw = provider.enrich(&ProviderRequest {
                system_prompt: "Return one JSON object with key ok and boolean true.".into(),
                data_json: "{\"purpose\":\"connection_test\"}".into(),
            })?;
            if raw.trim().is_empty() {
                bail!("AI Provider 返回空响应");
            }
            Ok(format!("DeepSeek 连接成功 · {}", config.model))
        })
        .map_err(|error| sanitize_detail(&format!("{error:#}")));
        let _ = done_tx.send(WorkDone::Connection { generation, result });
    });
}

fn with_provider<T>(
    config: &ResourceEnrichmentConfig,
    mode: &ProviderMode,
    run: impl FnOnce(&dyn EnrichmentProvider) -> Result<T>,
) -> Result<T> {
    match mode {
        ProviderMode::System => {
            let key = crate::resource_enrichment::SystemCredentialSource
                .api_key()?
                .context("请先在“资料库管理”中保存 DeepSeek API Key")?;
            let provider =
                crate::resource_enrichment::OpenAiCompatibleProvider::new(config.clone(), key)?;
            run(&provider)
        }
        #[cfg(test)]
        ProviderMode::Fixed(provider) => run(provider.as_ref()),
    }
}

fn finish_success_on(tx: &Transaction<'_>, task: &ClaimedTask, now: i64) -> Result<()> {
    tx.execute(
        "UPDATE knowledge_task_attempts SET status='succeeded',finished_at=?2,
         error_kind=NULL,user_message=NULL,technical_detail=NULL
         WHERE task_id=?1 AND status='running' AND claim_generation=?3",
        params![task.id, now, task.generation],
    )?;
    tx.execute(
        "UPDATE knowledge_tasks SET status='succeeded',updated_at=?2 WHERE id=?1",
        params![task.id, now],
    )?;
    bump_change(tx, task.id)?;
    Ok(())
}

fn bump_change(tx: &Transaction<'_>, task_id: i64) -> Result<i64> {
    tx.execute(
        "UPDATE knowledge_change_clock SET sequence=sequence+1 WHERE singleton_id=1",
        [],
    )?;
    let sequence: i64 = tx.query_row(
        "SELECT sequence FROM knowledge_change_clock WHERE singleton_id=1",
        [],
        |row| row.get(0),
    )?;
    tx.execute(
        "UPDATE knowledge_tasks SET change_seq=?2 WHERE id=?1",
        params![task_id, sequence],
    )?;
    Ok(sequence)
}

fn lease_owned_on(conn: &Connection, owner: &str, generation: i64) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM knowledge_executor_lease
         WHERE singleton_id=1 AND owner_id=?1 AND generation=?2)",
        params![owner, generation],
        |row| row.get(0),
    )?)
}

fn fence_valid(conn: &Connection, owner: &str, task_id: i64, generation: i64) -> Result<bool> {
    if !lease_owned_on(conn, owner, generation)? {
        return Ok(false);
    }
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM knowledge_task_attempts
         WHERE task_id=?1 AND status='running' AND claim_generation=?2)",
        params![task_id, generation],
        |row| row.get(0),
    )?)
}

fn map_snapshot(row: &rusqlite::Row<'_>) -> rusqlite::Result<TaskSnapshot> {
    let kind: String = row.get(1)?;
    let status: String = row.get(3)?;
    let stage: Option<String> = row.get(4)?;
    let error: Option<String> = row.get(8)?;
    let parsed_kind = TaskKind::parse(&kind).map_err(sql_conversion_error)?;
    Ok(TaskSnapshot {
        task_id: row.get(0)?,
        key: TaskKey::new(parsed_kind, row.get(2)?),
        status: TaskStatus::parse(&status).map_err(sql_conversion_error)?,
        current_stage: TaskStage::parse(stage).map_err(sql_conversion_error)?,
        change_seq: row.get(5)?,
        attempt_number: row.get(6)?,
        automatic_retry: row.get(7)?,
        error_kind: ErrorKind::parse(error).map_err(sql_conversion_error)?,
        user_message: row.get(9)?,
        technical_detail: row.get(10)?,
    })
}

fn sql_conversion_error(error: anyhow::Error) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(
        0,
        rusqlite::types::Type::Text,
        Box::new(std::io::Error::other(error.to_string())),
    )
}

fn owner_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("{}-{nanos}", std::process::id())
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backup::BackupStore;
    use crate::config::Config;
    use crate::local_data_maintenance::{MaintenanceEngine, MaintenanceRequest, MaintenanceStatus};
    use crate::model::NewArticle;
    use crate::resource_library_lifecycle::{
        Clock, CreateResource, NoProcessingHandoff, ProjectionScope, ResourceCollection,
        ResourceKind, ResourceLibraryLifecycle, ResourceLifecycleChange, ResourcePrivacy,
        ResourceSource, SnapshotInput,
    };

    // These tests intentionally exercise real worker, provider, SQLite, and
    // maintenance threads. Running several wall-clock protocols at once makes
    // scheduler starvation look like a workflow timeout, so keep this module's
    // integration-style engine scenarios isolated from one another.
    static ENGINE_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn engine_test_guard() -> std::sync::MutexGuard<'static, ()> {
        ENGINE_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    struct TestClock(i64);
    impl Clock for TestClock {
        fn now(&self) -> i64 {
            self.0
        }
    }

    struct ValidResourceProvider;
    impl EnrichmentProvider for ValidResourceProvider {
        fn enrich(&self, _: &ProviderRequest) -> Result<String> {
            Ok(serde_json::json!({
                "purpose_zh": "设计工具",
                "use_when_zh": "需要设计素材时",
                "capabilities": ["查找素材"],
                "limitations": [],
                "categories": ["tool"],
                "tags_zh": ["设计"],
                "tags_en": ["design"],
                "pricing": "unknown",
                "requires_login": null,
                "languages": ["en"],
                "evidence": []
            })
            .to_string())
        }
    }

    struct ControlledResourceProvider {
        entered: std_mpsc::Sender<()>,
        release: std::sync::Mutex<std_mpsc::Receiver<()>>,
    }

    impl EnrichmentProvider for ControlledResourceProvider {
        fn enrich(&self, request: &ProviderRequest) -> Result<String> {
            let _ = self.entered.send(());
            self.release
                .lock()
                .expect("controlled provider release lock poisoned")
                .recv_timeout(Duration::from_secs(5))
                .context("controlled provider was not released")?;
            ValidResourceProvider.enrich(request)
        }
    }

    fn file_db(name: &str) -> (Db, PathBuf) {
        let path = std::env::temp_dir().join(format!(
            "shiyue-workflow-{name}-{}-{}.db",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_file(&path);
        (Db::open(&path).unwrap(), path)
    }

    fn resource(db: &Db, now: i64) -> i64 {
        let outcome = ResourceLibraryLifecycle::new(db, &NoProcessingHandoff, &TestClock(now))
            .apply(
                ResourceLifecycleChange::Create(CreateResource {
                    url: "https://example.com/design".into(),
                    parent_resource_id: None,
                    linked_article_id: None,
                    kind: ResourceKind::Page,
                    title: None,
                    private_note: None,
                    privacy: ResourcePrivacy::Public,
                    source: ResourceSource::Gui,
                    manual_rating: None,
                }),
                ProjectionScope::collection(ResourceCollection::Active),
            )
            .unwrap();
        let id = outcome.affected_resource_ids[0];
        let input = SnapshotInput {
            fetched_url: Some("https://example.com/design".into()),
            http_status: Some(200),
            title: Some("Design Tool".into()),
            cleaned_content: Some("A useful design tool".into()),
            fetch_error: None,
        };
        let tx = db.conn.unchecked_transaction().unwrap();
        tx.execute(
            "INSERT INTO resource_snapshots(resource_id,content_hash,fetched_url,http_status,title,cleaned_content,fetched_at)
             VALUES(?1,'test-hash',?2,200,?3,?4,?5)",
            params![id, input.fetched_url, input.title, input.cleaned_content, now],
        )
        .unwrap();
        let snapshot_id = tx.last_insert_rowid();
        resource_target::record_snapshot_success(&tx, id, snapshot_id, &input, now).unwrap();
        tx.commit().unwrap();
        id
    }

    fn wait_for_status(engine: &KnowledgeEngine, key: TaskKey, expected: TaskStatus) {
        let deadline = Instant::now() + Duration::from_secs(4);
        loop {
            if engine
                .snapshot(key)
                .unwrap()
                .is_some_and(|snapshot| snapshot.status == expected)
            {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "knowledge task did not reach {expected:?}"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn identical_article_knowledge_output_is_a_revision_noop() {
        let (db, path) = file_db("article-revision");
        let feed_id = db.add_feed("https://example.test/article-feed", 0).unwrap();
        let feed = db.get_feed(feed_id).unwrap();
        db.record_success(
            &feed,
            1,
            &Config::default(),
            None,
            &[NewArticle {
                entry_id: "article".into(),
                url: Some("https://example.test/article".into()),
                title: Some("Article".into()),
                author: None,
                published: None,
                content: Some("Body".into()),
            }],
        )
        .unwrap();
        let article_id = db
            .conn
            .query_row(
                "SELECT id FROM articles WHERE feed_id=?1",
                [feed_id],
                |row| row.get::<_, i64>(0),
            )
            .unwrap();
        let store = WorkflowStore::new(&db);
        let owner = "article-revision-owner";
        let generation = store.acquire_lease(owner, 10).unwrap().unwrap();
        let key = TaskKey::new(TaskKind::ArticleSummary, article_id);
        store.request(key, 11).unwrap();
        let first = store.claim_next(owner, generation, 11).unwrap().unwrap();
        let before = library_projection_revision::read(&db.conn).unwrap();
        assert!(
            store
                .complete_article(owner, &first, "摘要", "翻译", "model", 12)
                .unwrap()
        );
        let after_first = library_projection_revision::read(&db.conn).unwrap();
        assert_eq!(after_first.article, before.article + 1);
        let projected = crate::article_library_lifecycle::ArticleLibraryLifecycle::new(&db)
            .project(crate::article_library_lifecycle::ProjectionScope::Article(
                article_id,
            ))
            .unwrap();
        let ai = &projected.article_ai[&article_id];
        assert_eq!(ai.summary_zh, "摘要");
        assert_eq!(ai.translation_zh, "翻译");
        assert_eq!(projected.stamp.revision, after_first.article);

        store.request(key, 13).unwrap();
        let second = store.claim_next(owner, generation, 13).unwrap().unwrap();
        assert!(
            store
                .complete_article(owner, &second, "摘要", "翻译", "model", 14)
                .unwrap()
        );
        assert_eq!(
            library_projection_revision::read(&db.conn).unwrap(),
            after_first
        );
        drop(db);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn request_is_idempotent_and_failed_request_creates_attempt() {
        let (db, path) = file_db("request");
        let id = resource(&db, 10);
        let store = WorkflowStore::new(&db);
        let key = TaskKey::new(TaskKind::ResourceCompletion, id);
        assert_eq!(
            store.request(key, 11).unwrap().disposition,
            RequestDisposition::Created
        );
        assert_eq!(
            store.request(key, 12).unwrap().disposition,
            RequestDisposition::Existing
        );
        db.conn
            .execute("UPDATE knowledge_tasks SET status='failed'", [])
            .unwrap();
        db.conn
            .execute("UPDATE knowledge_task_attempts SET status='failed'", [])
            .unwrap();
        assert_eq!(
            store.request(key, 13).unwrap().disposition,
            RequestDisposition::Retried
        );
        let count: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM knowledge_task_attempts", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(count, 2);
        drop(db);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn two_engines_have_only_one_executor_lease() {
        let _guard = engine_test_guard();
        let (db, path) = file_db("lease");
        drop(db);
        let first = KnowledgeEngine::start_with_provider(
            path.clone(),
            Default::default(),
            Arc::new(ValidResourceProvider),
        )
        .unwrap();
        let second = KnowledgeEngine::start_with_provider(
            path.clone(),
            Default::default(),
            Arc::new(ValidResourceProvider),
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        let db = loop {
            let db = Db::open(&path).unwrap();
            let owner: Option<String> = db
                .conn
                .query_row(
                    "SELECT owner_id FROM knowledge_executor_lease WHERE singleton_id=1",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            if owner.is_some() {
                break db;
            }
            drop(db);
            assert!(Instant::now() < deadline, "executor lease was not acquired");
            std::thread::sleep(Duration::from_millis(20));
        };
        drop(second);
        drop(first);
        drop(db);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn gui_style_request_notice_snapshot_completes() {
        let _guard = engine_test_guard();
        let (db, path) = file_db("gui-style");
        let id = resource(&db, now());
        drop(db);
        let engine = KnowledgeEngine::start_with_provider(
            path.clone(),
            Default::default(),
            Arc::new(ValidResourceProvider),
        )
        .unwrap();
        let key = TaskKey::new(TaskKind::ResourceCompletion, id);
        engine.request(key).unwrap();
        let deadline = Instant::now() + Duration::from_secs(4);
        loop {
            let changed = engine
                .try_notices()
                .any(|notice| notice == KnowledgeNotice::Changed(key));
            if changed
                && engine
                    .snapshot(key)
                    .unwrap()
                    .is_some_and(|s| s.status == TaskStatus::Succeeded)
            {
                break;
            }
            assert!(Instant::now() < deadline, "workflow did not finish");
            std::thread::sleep(Duration::from_millis(20));
        }
        drop(engine);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn projection_residency_evicts_immediately_and_reobserve_materializes() {
        let _guard = engine_test_guard();
        let (db, path) = file_db("projection-residency");
        let id = resource(&db, now());
        drop(db);
        let engine = KnowledgeEngine::start_with_provider(
            path.clone(),
            Default::default(),
            Arc::new(ValidResourceProvider),
        )
        .unwrap();
        let observer = engine.projection_observer();
        let key = TaskKey::new(TaskKind::ResourceCompletion, id);
        observer.observe(key);
        let initial_deadline = Instant::now() + Duration::from_secs(2);
        while observer.snapshot(key).is_none() {
            assert!(
                Instant::now() < initial_deadline,
                "initial materialization timed out"
            );
            std::thread::sleep(Duration::from_millis(10));
        }

        engine.request(key).unwrap();
        let terminal_deadline = Instant::now() + Duration::from_secs(4);
        loop {
            if observer
                .snapshot(key)
                .flatten()
                .is_some_and(|snapshot| snapshot.status == TaskStatus::Succeeded)
            {
                break;
            }
            assert!(
                Instant::now() < terminal_deadline,
                "terminal materialization timed out"
            );
            std::thread::sleep(Duration::from_millis(20));
        }

        observer.forget(key);
        assert!(!observer.is_resident(key));
        assert!(observer.snapshot(key).is_none());
        std::thread::sleep(Duration::from_millis(100));
        assert!(
            observer.snapshot(key).is_none(),
            "queued publication resurrected an evicted snapshot"
        );

        observer.observe(key);
        let rematerialize_deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if observer
                .snapshot(key)
                .flatten()
                .is_some_and(|snapshot| snapshot.status == TaskStatus::Succeeded)
            {
                break;
            }
            assert!(
                Instant::now() < rematerialize_deadline,
                "re-observation did not materialize durable state"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        drop(engine);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn cli_style_no_wait_survives_restart_and_waits_for_terminal_state() {
        let _guard = engine_test_guard();
        let (db, path) = file_db("cli-style");
        let id = resource(&db, now());
        drop(db);
        let key = TaskKey::new(TaskKind::ResourceCompletion, id);

        let client = KnowledgeEngine::start_with_mode(
            path.clone(),
            Default::default(),
            ProviderMode::Fixed(Arc::new(ValidResourceProvider)),
            ExecutorPolicy::ObserveOnly,
        )
        .unwrap();
        client.request(key).unwrap();
        assert_eq!(
            client.snapshot(key).unwrap().unwrap().status,
            TaskStatus::Queued
        );
        drop(client);

        let executor = KnowledgeEngine::start_with_provider(
            path.clone(),
            Default::default(),
            Arc::new(ValidResourceProvider),
        )
        .unwrap();
        let terminal = executor.wait_terminal(key, Duration::from_secs(4)).unwrap();
        assert_eq!(terminal.status, TaskStatus::Succeeded);
        drop(executor);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn maintenance_participant_quiesces_idle_engine_rejects_work_and_resumes() {
        let _guard = engine_test_guard();
        let (db, path) = file_db("maintenance-idle");
        let id = resource(&db, now());
        drop(db);
        let engine = KnowledgeEngine::start_with_provider(
            path.clone(),
            Default::default(),
            Arc::new(ValidResourceProvider),
        )
        .unwrap();
        let key = TaskKey::new(TaskKind::ResourceCompletion, id);
        let observer = engine.projection_observer();
        observer.observe(key);
        let materialize_deadline = Instant::now() + Duration::from_secs(2);
        while observer.snapshot(key).is_none() {
            assert!(Instant::now() < materialize_deadline);
            std::thread::sleep(Duration::from_millis(10));
        }
        let participant = engine.maintenance_participant();
        participant
            .quiesce(Instant::now() + Duration::from_secs(2), "epoch-a")
            .unwrap();
        assert!(observer.is_resident(key));
        assert!(observer.snapshot(key).is_none());

        assert!(
            engine
                .request(key)
                .unwrap_err()
                .to_string()
                .contains("MAINTENANCE_IN_PROGRESS")
        );
        assert!(
            engine
                .test_connection()
                .unwrap_err()
                .to_string()
                .contains("MAINTENANCE_IN_PROGRESS")
        );

        participant.resume("epoch-b").unwrap();
        let rematerialize_deadline = Instant::now() + Duration::from_secs(2);
        while observer.snapshot(key).is_none() {
            assert!(Instant::now() < rematerialize_deadline);
            std::thread::sleep(Duration::from_millis(10));
        }
        engine.request(key).unwrap();
        wait_for_status(&engine, key, TaskStatus::Succeeded);
        drop(engine);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn maintenance_quiesce_allows_provider_to_finish_inside_deadline() {
        let _guard = engine_test_guard();
        let (db, path) = file_db("maintenance-grace");
        let id = resource(&db, now());
        drop(db);
        let (entered_tx, entered_rx) = std_mpsc::channel();
        let (release_tx, release_rx) = std_mpsc::channel();
        let engine = KnowledgeEngine::start_with_provider(
            path.clone(),
            Default::default(),
            Arc::new(ControlledResourceProvider {
                entered: entered_tx,
                release: std::sync::Mutex::new(release_rx),
            }),
        )
        .unwrap();
        let key = TaskKey::new(TaskKind::ResourceCompletion, id);
        engine.request(key).unwrap();
        entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let releaser = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            release_tx.send(()).unwrap();
        });

        let participant = engine.maintenance_participant();
        participant
            .quiesce(Instant::now() + Duration::from_secs(2), "epoch-a")
            .unwrap();
        releaser.join().unwrap();
        assert_eq!(
            engine.snapshot(key).unwrap().unwrap().status,
            TaskStatus::Succeeded
        );
        participant.resume("epoch-b").unwrap();
        drop(engine);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn maintenance_deadline_interrupts_attempt_and_fences_late_provider_result() {
        let _guard = engine_test_guard();
        let (db, path) = file_db("maintenance-timeout");
        let id = resource(&db, now());
        drop(db);
        let (entered_tx, entered_rx) = std_mpsc::channel();
        let (release_tx, release_rx) = std_mpsc::channel();
        let engine = KnowledgeEngine::start_with_provider(
            path.clone(),
            Default::default(),
            Arc::new(ControlledResourceProvider {
                entered: entered_tx,
                release: std::sync::Mutex::new(release_rx),
            }),
        )
        .unwrap();
        let key = TaskKey::new(TaskKind::ResourceCompletion, id);
        engine.request(key).unwrap();
        entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();

        let participant = engine.maintenance_participant();
        participant
            .quiesce(
                Instant::now() + Duration::from_secs(3),
                "epoch-before-maintenance",
            )
            .unwrap();
        assert_eq!(
            engine.snapshot(key).unwrap().unwrap().status,
            TaskStatus::Interrupted
        );
        participant.resume("epoch-after-maintenance").unwrap();
        release_tx.send(()).unwrap();
        std::thread::sleep(Duration::from_millis(150));

        let db = Db::open(&path).unwrap();
        assert_eq!(resource_target::load(&db, id).unwrap().purpose_zh, None);
        drop(db);
        drop(engine);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn queued_task_survives_quiesce_and_executes_after_an_executor_resumes() {
        let _guard = engine_test_guard();
        let (db, path) = file_db("maintenance-queued");
        let id = resource(&db, now());
        drop(db);
        let key = TaskKey::new(TaskKind::ResourceCompletion, id);
        let client = KnowledgeEngine::start_with_mode(
            path.clone(),
            Default::default(),
            ProviderMode::Fixed(Arc::new(ValidResourceProvider)),
            ExecutorPolicy::ObserveOnly,
        )
        .unwrap();
        client.request(key).unwrap();
        let participant = client.maintenance_participant();
        participant
            .quiesce(Instant::now() + Duration::from_secs(1), "epoch-a")
            .unwrap();
        assert_eq!(
            client.snapshot(key).unwrap().unwrap().status,
            TaskStatus::Queued
        );
        participant.resume("epoch-b").unwrap();
        drop(client);

        let executor = KnowledgeEngine::start_with_provider(
            path.clone(),
            Default::default(),
            Arc::new(ValidResourceProvider),
        )
        .unwrap();
        wait_for_status(&executor, key, TaskStatus::Succeeded);
        drop(executor);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn maintenance_participant_fails_immediately_after_engine_stops() {
        let _guard = engine_test_guard();
        let (db, path) = file_db("maintenance-disconnected");
        drop(db);
        let engine = KnowledgeEngine::start_with_provider(
            path.clone(),
            Default::default(),
            Arc::new(ValidResourceProvider),
        )
        .unwrap();
        let participant = engine.maintenance_participant();
        drop(engine);

        let error = participant
            .quiesce(Instant::now() + Duration::from_secs(1), "epoch-a")
            .unwrap_err()
            .to_string();
        assert!(error.contains("KNOWLEDGE_ENGINE_STOPPED"), "{error}");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn maintenance_engine_drives_knowledge_participant_through_the_real_sidecar() {
        let _guard = engine_test_guard();
        let (db, path) = file_db("maintenance-sidecar");
        let id = resource(&db, now());
        drop(db);
        let engine = KnowledgeEngine::start_with_provider(
            path.clone(),
            Default::default(),
            Arc::new(ValidResourceProvider),
        )
        .unwrap();
        let backup_dir = path.with_extension("maintenance-backups");
        let maintenance = MaintenanceEngine::start(
            path.clone(),
            BackupStore::open(&backup_dir).unwrap(),
            vec![engine.maintenance_participant()],
        )
        .unwrap();
        maintenance.request(MaintenanceRequest::Compact).unwrap();

        let deadline = Instant::now() + Duration::from_secs(4);
        let terminal = loop {
            if let Some(snapshot) = maintenance.snapshot().unwrap()
                && snapshot.status != MaintenanceStatus::Running
            {
                break snapshot;
            }
            assert!(Instant::now() < deadline, "maintenance did not finish");
            std::thread::sleep(Duration::from_millis(20));
        };
        let maintenance_notices = engine.try_notices().collect::<Vec<_>>();
        assert_eq!(
            terminal.status,
            MaintenanceStatus::Succeeded,
            "maintenance ended unexpectedly: {terminal:?}; knowledge notices={maintenance_notices:?}"
        );

        let key = TaskKey::new(TaskKind::ResourceCompletion, id);
        let notices = engine.try_notices().collect::<Vec<_>>();
        engine.request(key).unwrap_or_else(|error| {
            panic!("engine did not resume: {error:#}; notices={notices:?}")
        });
        wait_for_status(&engine, key, TaskStatus::Succeeded);
        drop(maintenance);
        drop(engine);
        let _ = std::fs::remove_dir_all(backup_dir);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn stale_generation_cannot_finish_after_takeover() {
        let (db, path) = file_db("fence");
        let id = resource(&db, 10);
        let store = WorkflowStore::new(&db);
        let key = TaskKey::new(TaskKind::ResourceCompletion, id);
        store.request(key, 11).unwrap();
        let first = store.acquire_lease("first", 20).unwrap().unwrap();
        let claimed = store.claim_next("first", first, 20).unwrap().unwrap();
        store
            .advance("first", &claimed, TaskStage::Organizing, 20)
            .unwrap();
        let run_id = store
            .start_enrichment("first", &claimed, &Default::default(), None, 20)
            .unwrap()
            .unwrap();
        let second = store.acquire_lease("second", 40).unwrap().unwrap();
        store.interrupt_stale_running(second, 40).unwrap();
        let late_output = EnrichmentOutput {
            purpose_zh: "不应写入".into(),
            use_when_zh: "不应写入".into(),
            capabilities: vec![],
            limitations: vec![],
            categories: vec!["tool".into()],
            tags_zh: vec![],
            tags_en: vec![],
            pricing: "unknown".into(),
            requires_login: None,
            languages: vec![],
            evidence: vec![],
        };
        assert!(
            !store
                .advance("first", &claimed, TaskStage::Organizing, 41)
                .unwrap()
        );
        assert!(
            !store
                .complete_resource("first", &claimed, run_id, &late_output, 41)
                .unwrap()
        );
        assert_eq!(
            store.latest_snapshot(key).unwrap().unwrap().status,
            TaskStatus::Interrupted
        );
        assert_eq!(resource_target::load(&db, id).unwrap().purpose_zh, None);
        drop(db);
        let _ = std::fs::remove_file(path);
    }
}
