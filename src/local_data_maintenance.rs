//! Cross-process coordination for exclusive local-library maintenance.
//!
//! The coordination state deliberately lives beside the primary database. A
//! restore replaces the primary database, so the database being restored
//! cannot also be the source of truth for the maintenance epoch.

use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use chrono::Utc;
use fs2::FileExt;
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use std::ops::{Deref, DerefMut};

use crate::backup::{BackupEntry, BackupStore};
use crate::db::Db;

const PROTOCOL_VERSION: i64 = 1;
const WRITER_DRAIN_TIMEOUT: Duration = Duration::from_secs(2);
const RESUME_ATTEMPTS: usize = 3;

static EPOCH_COUNTER: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MaintenanceOperation {
    Restore,
    Compact,
    SchemaMigration,
}

impl MaintenanceOperation {
    fn as_str(self) -> &'static str {
        match self {
            Self::Restore => "restore",
            Self::Compact => "compact",
            Self::SchemaMigration => "schema_migration",
        }
    }

    fn parse(value: &str) -> Result<Self> {
        match value {
            "restore" => Ok(Self::Restore),
            "compact" => Ok(Self::Compact),
            "schema_migration" => Ok(Self::SchemaMigration),
            _ => bail!("unknown maintenance operation: {value}"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MaintenanceStage {
    WaitingForWriters,
    CreatingSafetyBackup,
    Executing,
    Validating,
    Reopening,
    ResumingParticipants,
    Finished,
}

impl MaintenanceStage {
    fn as_str(self) -> &'static str {
        match self {
            Self::WaitingForWriters => "waiting_for_writers",
            Self::CreatingSafetyBackup => "creating_safety_backup",
            Self::Executing => "executing",
            Self::Validating => "validating",
            Self::Reopening => "reopening",
            Self::ResumingParticipants => "resuming_participants",
            Self::Finished => "finished",
        }
    }

    fn parse(value: &str) -> Result<Self> {
        match value {
            "waiting_for_writers" => Ok(Self::WaitingForWriters),
            "creating_safety_backup" => Ok(Self::CreatingSafetyBackup),
            "executing" => Ok(Self::Executing),
            "validating" => Ok(Self::Validating),
            "reopening" => Ok(Self::Reopening),
            "resuming_participants" => Ok(Self::ResumingParticipants),
            "finished" => Ok(Self::Finished),
            _ => bail!("unknown maintenance stage: {value}"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MaintenanceStatus {
    Running,
    Succeeded,
    Degraded,
    Failed,
}

impl MaintenanceStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Succeeded => "succeeded",
            Self::Degraded => "degraded",
            Self::Failed => "failed",
        }
    }

    fn parse(value: &str) -> Result<Self> {
        match value {
            "running" => Ok(Self::Running),
            "succeeded" => Ok(Self::Succeeded),
            "degraded" => Ok(Self::Degraded),
            "failed" => Ok(Self::Failed),
            _ => bail!("unknown maintenance status: {value}"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MaintenanceFailureKind {
    WriterTimeout,
    Coordination,
    SafetyBackup,
    Storage,
    Integrity,
    Reopen,
    ParticipantResume,
}

impl MaintenanceFailureKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::WriterTimeout => "writer_timeout",
            Self::Coordination => "coordination",
            Self::SafetyBackup => "safety_backup",
            Self::Storage => "storage",
            Self::Integrity => "integrity",
            Self::Reopen => "reopen",
            Self::ParticipantResume => "participant_resume",
        }
    }

    fn parse(value: &str) -> Result<Self> {
        match value {
            "writer_timeout" => Ok(Self::WriterTimeout),
            "coordination" => Ok(Self::Coordination),
            "safety_backup" => Ok(Self::SafetyBackup),
            "storage" => Ok(Self::Storage),
            "integrity" => Ok(Self::Integrity),
            "reopen" => Ok(Self::Reopen),
            "participant_resume" => Ok(Self::ParticipantResume),
            _ => bail!("unknown maintenance failure kind: {value}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MaintenanceSnapshot {
    pub run_id: String,
    pub epoch: String,
    pub operation: MaintenanceOperation,
    pub stage: MaintenanceStage,
    pub status: MaintenanceStatus,
    pub active: bool,
    pub safety_backup: Option<PathBuf>,
    pub failure_kind: Option<MaintenanceFailureKind>,
    pub user_message: Option<String>,
    pub technical_detail: Option<String>,
    pub failed_participants: Vec<String>,
    pub started_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone)]
pub(crate) enum MaintenanceRequest {
    Restore(BackupEntry),
    Compact,
}

impl MaintenanceRequest {
    fn operation(&self) -> MaintenanceOperation {
        match self {
            Self::Restore(_) => MaintenanceOperation::Restore,
            Self::Compact => MaintenanceOperation::Compact,
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) enum MaintenanceNotice {
    Changed(MaintenanceSnapshot),
    ModuleFault {
        user_message: String,
        technical_detail: String,
    },
}

/// A long-lived writer that must become quiescent before exclusive storage
/// maintenance. Two production adapters exist: RSS scheduling and knowledge
/// processing.
pub(crate) trait MaintenanceParticipant: Send + Sync {
    fn name(&self) -> &'static str;
    fn quiesce(&self, deadline: Instant, epoch: &str) -> Result<()>;
    fn resume(&self, epoch: &str) -> Result<()>;
}

#[derive(Debug, Clone)]
struct CoordinationPaths {
    state: PathBuf,
    writer_lock: PathBuf,
    owner_lock: PathBuf,
}

impl CoordinationPaths {
    fn for_database(database: &Path) -> Self {
        let base = database.as_os_str().to_string_lossy();
        Self {
            state: PathBuf::from(format!("{base}.maintenance.sqlite3")),
            writer_lock: PathBuf::from(format!("{base}.maintenance.lock")),
            owner_lock: PathBuf::from(format!("{base}.maintenance.owner.lock")),
        }
    }
}

/// The only external seam for observing maintenance and fencing normal
/// database work. Callers never interpret the sidecar protocol or handle raw
/// writer permits themselves.
pub(crate) struct MaintenanceFence;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MaintenanceAvailability {
    Available,
    Active,
}

impl MaintenanceAvailability {
    pub(crate) fn is_active(self) -> bool {
        self == Self::Active
    }
}

impl MaintenanceFence {
    pub(crate) fn rejected(error: &anyhow::Error) -> bool {
        let detail = format!("{error:#}");
        detail.contains("MAINTENANCE_IN_PROGRESS") || detail.contains("STALE_LIBRARY_EPOCH")
    }

    /// Open the fence held for the complete lifetime of a normal `Db`.
    pub(crate) fn open_connection(database: &Path) -> Result<ConnectionFence> {
        let generation = GenerationFence::open(database)?;
        let lifetime_permit = generation.acquire(WriteIntent::Mutation)?;
        Ok(ConnectionFence {
            generation,
            _lifetime_permit: lifetime_permit,
        })
    }

    /// Capture the current library generation without holding maintenance
    /// open while an external/network operation runs.
    pub(crate) fn witness(database: &Path) -> Result<GenerationFence> {
        GenerationFence::open(database)
    }

    /// Observe maintenance without exposing sidecar layout or fail-open
    /// choices to callers.
    pub(crate) fn observe(database: &Path) -> Result<MaintenanceAvailability> {
        let state = read_state(&CoordinationPaths::for_database(database))?;
        Ok(if state.active {
            MaintenanceAvailability::Active
        } else {
            MaintenanceAvailability::Available
        })
    }
}

/// Fence retained by a normal database connection. Its lifetime permit makes
/// the connection itself visible to exclusive maintenance; each write still
/// receives a short permit so maintenance intent is checked at commit time.
#[derive(Debug)]
pub(crate) struct ConnectionFence {
    generation: GenerationFence,
    _lifetime_permit: WriterPermit,
}

impl ConnectionFence {
    pub(crate) fn generation(&self) -> &str {
        self.generation.opened_epoch()
    }

    pub(crate) fn begin_write<'connection>(
        &self,
        connection: &'connection Connection,
    ) -> Result<FencedTransaction<'connection>> {
        self.generation.begin_write(connection)
    }

    pub(crate) fn begin_immediate_write<'connection>(
        &self,
        connection: &'connection mut Connection,
    ) -> Result<FencedTransaction<'connection>> {
        self.generation.begin_immediate_write(connection)
    }

    /// Acquire the write permit before a lifecycle publishes its irreversible
    /// `Committing` state. A permit obtained while maintenance is unavailable
    /// may drain after maintenance intent is published, but no new one may be
    /// acquired once that intent is visible.
    pub(crate) fn begin_linearized_immediate_write<'connection>(
        &self,
        connection: &'connection mut Connection,
    ) -> Result<FencedTransaction<'connection>> {
        self.generation
            .begin_immediate_write_for(connection, WriteIntent::LinearizedMutation)
    }

    /// Persist only the participant state required to reach a maintenance
    /// safe point. The shared lock still prevents epoch rotation while this
    /// commits, but an already-published maintenance intent is allowed.
    pub(crate) fn begin_drain_write<'connection>(
        &self,
        connection: &'connection Connection,
    ) -> Result<FencedTransaction<'connection>> {
        self.generation
            .begin_write_for(connection, WriteIntent::MaintenanceDrain)
    }

    pub(crate) fn validate(&self) -> Result<()> {
        self._lifetime_permit.validate()
    }
}

/// Generation witness retained across work whose result must not cross a
/// restore boundary (for example an RSS network fetch).
#[derive(Debug, Clone)]
pub(crate) struct GenerationFence {
    gate: WriterGate,
}

impl GenerationFence {
    fn open(database: &Path) -> Result<Self> {
        Ok(Self {
            gate: WriterGate::open(database)?,
        })
    }

    fn opened_epoch(&self) -> &str {
        self.gate.opened_epoch()
    }

    fn acquire(&self, intent: WriteIntent) -> Result<WriterPermit> {
        self.gate.permit(intent)
    }

    pub(crate) fn begin_write<'connection>(
        &self,
        connection: &'connection Connection,
    ) -> Result<FencedTransaction<'connection>> {
        self.begin_write_for(connection, WriteIntent::Mutation)
    }

    fn begin_write_for<'connection>(
        &self,
        connection: &'connection Connection,
        intent: WriteIntent,
    ) -> Result<FencedTransaction<'connection>> {
        let permit = self.acquire(intent)?;
        let transaction = connection.unchecked_transaction()?;
        Ok(FencedTransaction {
            transaction: Some(transaction),
            permit: Some(permit),
        })
    }

    fn begin_immediate_write<'connection>(
        &self,
        connection: &'connection mut Connection,
    ) -> Result<FencedTransaction<'connection>> {
        self.begin_immediate_write_for(connection, WriteIntent::Mutation)
    }

    fn begin_immediate_write_for<'connection>(
        &self,
        connection: &'connection mut Connection,
        intent: WriteIntent,
    ) -> Result<FencedTransaction<'connection>> {
        let permit = self.acquire(intent)?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        Ok(FencedTransaction {
            transaction: Some(transaction),
            permit: Some(permit),
        })
    }
}

/// Transaction whose only commit path validates the maintenance generation
/// immediately before SQLite commit. Dropping it rolls back as usual.
#[derive(Debug)]
pub(crate) struct FencedTransaction<'connection> {
    transaction: Option<Transaction<'connection>>,
    permit: Option<WriterPermit>,
}

impl<'connection> Deref for FencedTransaction<'connection> {
    type Target = Transaction<'connection>;

    fn deref(&self) -> &Self::Target {
        self.transaction
            .as_ref()
            .expect("fenced transaction already committed")
    }
}

impl DerefMut for FencedTransaction<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.transaction
            .as_mut()
            .expect("fenced transaction already committed")
    }
}

impl<'connection> FencedTransaction<'connection> {
    pub(crate) fn uncoordinated(
        transaction: Transaction<'connection>,
    ) -> FencedTransaction<'connection> {
        FencedTransaction {
            transaction: Some(transaction),
            permit: None,
        }
    }

    pub(crate) fn commit(mut self) -> Result<()> {
        if let Some(permit) = self.permit.as_ref() {
            permit.validate()?;
        }
        let transaction = self
            .transaction
            .take()
            .expect("fenced transaction already committed");
        transaction.commit()?;
        self.permit.take();
        Ok(())
    }
}

/// Internal implementation for a per-database writer generation. It is kept
/// private so every caller crosses `MaintenanceFence`.
#[derive(Debug, Clone)]
struct WriterGate {
    paths: CoordinationPaths,
    opened_epoch: String,
}

impl WriterGate {
    fn open(database: &Path) -> Result<Self> {
        let paths = CoordinationPaths::for_database(database);
        let state = read_state(&paths)?;
        if state.active {
            bail!("MAINTENANCE_IN_PROGRESS: local library is temporarily read-only");
        }
        Ok(Self {
            paths,
            opened_epoch: state.epoch,
        })
    }

    fn opened_epoch(&self) -> &str {
        &self.opened_epoch
    }

    fn permit(&self, intent: WriteIntent) -> Result<WriterPermit> {
        let file = open_lock_file(&self.paths.writer_lock)?;
        FileExt::lock_shared(&file).context("acquire local-library writer permit")?;
        let state = read_state(&self.paths)?;
        if state.active && intent != WriteIntent::MaintenanceDrain {
            let _ = FileExt::unlock(&file);
            bail!("MAINTENANCE_IN_PROGRESS: local library is temporarily read-only");
        }
        if state.epoch != self.opened_epoch {
            let _ = FileExt::unlock(&file);
            bail!("STALE_LIBRARY_EPOCH: reopen the local library before writing");
        }
        Ok(WriterPermit {
            file: Some(file),
            paths: self.paths.clone(),
            epoch: self.opened_epoch.clone(),
            intent,
        })
    }
}

/// Run startup schema creation/migration under the same cross-process
/// exclusive gate used by restore and VACUUM. There are no local background
/// participants yet during startup; already-running processes observe the
/// sidecar and quiesce themselves.
pub(crate) fn run_schema_migration<T>(
    database: &Path,
    operation: impl FnOnce() -> Result<T>,
) -> Result<T> {
    let paths = CoordinationPaths::for_database(database);
    let (mut snapshot, owner) = begin_owned_run(database, MaintenanceOperation::SchemaMigration)?;

    let writer = open_lock_file(&paths.writer_lock)?;
    let deadline = Instant::now() + WRITER_DRAIN_TIMEOUT;
    while FileExt::try_lock_exclusive(&writer).is_err() {
        if Instant::now() >= deadline {
            snapshot.active = false;
            snapshot.stage = MaintenanceStage::Finished;
            snapshot.status = MaintenanceStatus::Failed;
            snapshot.failure_kind = Some(MaintenanceFailureKind::WriterTimeout);
            snapshot.technical_detail = Some("SCHEMA_WRITER_DRAIN_TIMEOUT".into());
            write_snapshot(&paths, &snapshot)?;
            let _ = FileExt::unlock(&owner);
            bail!("WRITER_DRAIN_TIMEOUT: schema migration could not obtain exclusive access");
        }
        std::thread::sleep(Duration::from_millis(25));
    }

    snapshot.epoch = rotate_epoch(&paths)?;
    snapshot.stage = MaintenanceStage::CreatingSafetyBackup;
    snapshot.updated_at = Utc::now().timestamp();
    write_snapshot(&paths, &snapshot)?;

    let backup_directory = database
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("backups");
    let safety_result = (|| -> Result<BackupEntry> {
        let conn = Connection::open(database)?;
        let raw_library = Db {
            conn,
            path: Some(database.to_path_buf()),
            _maintenance_fence: None,
        };
        BackupStore::open(backup_directory)?.create_safety(&raw_library)
    })();
    let safety = match safety_result {
        Ok(safety) => safety,
        Err(error) => {
            snapshot.active = false;
            snapshot.stage = MaintenanceStage::Finished;
            snapshot.status = MaintenanceStatus::Failed;
            snapshot.failure_kind = Some(MaintenanceFailureKind::SafetyBackup);
            snapshot.user_message = Some("无法创建升级前安全副本，数据库尚未修改".into());
            snapshot.technical_detail = Some(sanitize_detail(&format!("{error:#}")));
            snapshot.updated_at = Utc::now().timestamp();
            write_snapshot(&paths, &snapshot)?;
            let _ = FileExt::unlock(&writer);
            let _ = FileExt::unlock(&owner);
            return Err(error);
        }
    };
    snapshot.safety_backup = Some(safety.path);
    snapshot.stage = MaintenanceStage::Executing;
    snapshot.updated_at = Utc::now().timestamp();
    write_snapshot(&paths, &snapshot)?;
    let result = operation();
    snapshot.active = false;
    snapshot.stage = MaintenanceStage::Finished;
    snapshot.updated_at = Utc::now().timestamp();
    match &result {
        Ok(_) => {
            snapshot.status = MaintenanceStatus::Succeeded;
            snapshot.user_message = Some("数据库结构升级完成".into());
        }
        Err(error) => {
            snapshot.status = MaintenanceStatus::Failed;
            snapshot.failure_kind = Some(MaintenanceFailureKind::Storage);
            snapshot.user_message = Some("数据库结构升级失败".into());
            snapshot.technical_detail = Some(sanitize_detail(&format!("{error:#}")));
        }
    }
    write_snapshot(&paths, &snapshot)?;
    let _ = FileExt::unlock(&writer);
    let _ = FileExt::unlock(&owner);
    result
}

#[derive(Debug)]
struct WriterPermit {
    file: Option<File>,
    paths: CoordinationPaths,
    epoch: String,
    intent: WriteIntent,
}

impl WriterPermit {
    fn validate(&self) -> Result<()> {
        let state = read_state(&self.paths)?;
        if state.active && self.intent == WriteIntent::Mutation {
            bail!("MAINTENANCE_IN_PROGRESS: local library is temporarily read-only");
        }
        if state.epoch != self.epoch {
            bail!("STALE_LIBRARY_EPOCH: writer result belongs to an older library epoch");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WriteIntent {
    Mutation,
    LinearizedMutation,
    MaintenanceDrain,
}

impl Drop for WriterPermit {
    fn drop(&mut self) {
        if let Some(file) = self.file.take() {
            let _ = FileExt::unlock(&file);
        }
    }
}

pub(crate) struct MaintenanceEngine {
    database: PathBuf,
    backup_store: BackupStore,
    participants: Vec<Arc<dyn MaintenanceParticipant>>,
    active: Arc<AtomicBool>,
    notice_tx: mpsc::Sender<MaintenanceNotice>,
    notice_rx: Mutex<mpsc::Receiver<MaintenanceNotice>>,
    joins: Mutex<Vec<JoinHandle<()>>>,
}

impl MaintenanceEngine {
    pub(crate) fn start(
        database: PathBuf,
        backup_store: BackupStore,
        participants: Vec<Arc<dyn MaintenanceParticipant>>,
    ) -> Result<Self> {
        ensure_state(&CoordinationPaths::for_database(&database))?;
        let (notice_tx, notice_rx) = mpsc::channel();
        let engine = Self {
            database,
            backup_store,
            participants,
            active: Arc::new(AtomicBool::new(false)),
            notice_tx,
            notice_rx: Mutex::new(notice_rx),
            joins: Mutex::new(Vec::new()),
        };
        engine.recover_interrupted_run()?;
        Ok(engine)
    }

    pub(crate) fn request(&self, request: MaintenanceRequest) -> Result<MaintenanceSnapshot> {
        if self
            .active
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            bail!("MAINTENANCE_IN_PROGRESS: another maintenance run is active");
        }

        let database = self.database.clone();
        let backup_store = self.backup_store.clone();
        let participants = self.participants.clone();
        let active = Arc::clone(&self.active);
        let notice_tx = self.notice_tx.clone();
        let (initial, owner) = match begin_owned_run(&database, request.operation()) {
            Ok(value) => value,
            Err(error) => {
                self.active.store(false, Ordering::SeqCst);
                return Err(error);
            }
        };
        let initial_for_thread = initial.clone();
        let join = match std::thread::Builder::new()
            .name("shiyue-data-maintenance".into())
            .spawn(move || {
                let result = execute_run(
                    &database,
                    &backup_store,
                    &participants,
                    request,
                    &initial_for_thread,
                    owner,
                    &notice_tx,
                );
                if let Err(error) = result {
                    let _ = finish_failed(
                        &CoordinationPaths::for_database(&database),
                        &initial_for_thread,
                        MaintenanceFailureKind::Coordination,
                        "资料维护模块异常停止",
                        &format!("{error:#}"),
                        &participants,
                        &notice_tx,
                    );
                    let _ = notice_tx.send(MaintenanceNotice::ModuleFault {
                        user_message: "资料维护模块异常停止".into(),
                        technical_detail: sanitize_detail(&format!("{error:#}")),
                    });
                }
                active.store(false, Ordering::SeqCst);
            }) {
            Ok(join) => join,
            Err(error) => {
                self.active.store(false, Ordering::SeqCst);
                let mut failed = initial.clone();
                failed.active = false;
                failed.stage = MaintenanceStage::Finished;
                failed.status = MaintenanceStatus::Failed;
                failed.failure_kind = Some(MaintenanceFailureKind::Coordination);
                failed.technical_detail = Some("MAINTENANCE_THREAD_START_FAILED".into());
                write_snapshot(&CoordinationPaths::for_database(&self.database), &failed)?;
                return Err(error.into());
            }
        };
        self.joins
            .lock()
            .expect("maintenance joins poisoned")
            .push(join);
        let _ = self
            .notice_tx
            .send(MaintenanceNotice::Changed(initial.clone()));
        Ok(initial)
    }

    pub(crate) fn snapshot(&self) -> Result<Option<MaintenanceSnapshot>> {
        let state = read_state(&CoordinationPaths::for_database(&self.database))?;
        Ok(state.run)
    }

    pub(crate) fn try_notices(&self) -> Vec<MaintenanceNotice> {
        let receiver = self
            .notice_rx
            .lock()
            .expect("maintenance notice receiver poisoned");
        std::iter::from_fn(|| receiver.try_recv().ok()).collect()
    }

    fn recover_interrupted_run(&self) -> Result<()> {
        let paths = CoordinationPaths::for_database(&self.database);
        let state = read_state(&paths)?;
        if !state.active {
            return Ok(());
        }
        let owner = open_lock_file(&paths.owner_lock)?;
        if FileExt::try_lock_exclusive(&owner).is_err() {
            return Ok(());
        }
        let writer = open_lock_file(&paths.writer_lock)?;
        let deadline = Instant::now() + WRITER_DRAIN_TIMEOUT;
        while FileExt::try_lock_exclusive(&writer).is_err() {
            if Instant::now() >= deadline {
                let _ = FileExt::unlock(&owner);
                bail!("WRITER_DRAIN_TIMEOUT: interrupted maintenance recovery is still locked");
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        let mut snapshot = state
            .run
            .context("active maintenance state has no run snapshot")?;
        let check = Db::open_for_maintenance(&self.database).and_then(|db| db.integrity_check());
        snapshot.active = false;
        snapshot.stage = MaintenanceStage::Finished;
        snapshot.updated_at = Utc::now().timestamp();
        if check.as_ref().is_ok_and(|check| check.ok) {
            snapshot.status = MaintenanceStatus::Failed;
            snapshot.failure_kind = Some(MaintenanceFailureKind::Coordination);
            snapshot.user_message =
                Some("上次资料维护被程序退出中断；数据库完整，已恢复使用".into());
            snapshot.technical_detail = Some("INTERRUPTED_MAINTENANCE_RECOVERED".into());
        } else if let Some(safety) = snapshot.safety_backup.clone() {
            self.backup_store
                .restore_safety_file(&self.database, &safety)?;
            let db = Db::open_for_maintenance(&self.database)?;
            let check = db.integrity_check()?;
            if !check.ok {
                bail!(
                    "safety backup restore failed integrity check: {}",
                    check.details
                );
            }
            snapshot.status = MaintenanceStatus::Failed;
            snapshot.failure_kind = Some(MaintenanceFailureKind::Integrity);
            snapshot.user_message = Some("上次资料维护被中断；已从恢复前安全副本回滚".into());
            snapshot.technical_detail = Some("INTERRUPTED_MAINTENANCE_ROLLED_BACK".into());
        } else {
            bail!("interrupted maintenance left an invalid database without a safety backup");
        }
        write_snapshot(&paths, &snapshot)?;
        let _ = FileExt::unlock(&writer);
        let _ = FileExt::unlock(&owner);
        let _ = self.notice_tx.send(MaintenanceNotice::Changed(snapshot));
        Ok(())
    }
}

impl Drop for MaintenanceEngine {
    fn drop(&mut self) {
        for join in self
            .joins
            .get_mut()
            .expect("maintenance joins poisoned")
            .drain(..)
        {
            let _ = join.join();
        }
    }
}

fn execute_run(
    database: &Path,
    backup_store: &BackupStore,
    participants: &[Arc<dyn MaintenanceParticipant>],
    request: MaintenanceRequest,
    initial: &MaintenanceSnapshot,
    owner: File,
    notice_tx: &mpsc::Sender<MaintenanceNotice>,
) -> Result<()> {
    let paths = CoordinationPaths::for_database(database);

    let deadline = Instant::now() + WRITER_DRAIN_TIMEOUT;
    for participant in participants {
        if let Err(error) = participant.quiesce(deadline, &initial.epoch) {
            let result = finish_failed(
                &paths,
                initial,
                MaintenanceFailureKind::WriterTimeout,
                "后台写入未能及时停止，资料维护已取消",
                &format!("{}: {error:#}", participant.name()),
                participants,
                notice_tx,
            );
            let _ = FileExt::unlock(&owner);
            return result;
        }
    }

    let writer = open_lock_file(&paths.writer_lock)?;
    while FileExt::try_lock_exclusive(&writer).is_err() {
        if Instant::now() >= deadline {
            let result = finish_failed(
                &paths,
                initial,
                MaintenanceFailureKind::WriterTimeout,
                "另一进程仍在写入资料库，资料维护已取消",
                "WRITER_PERMIT_DRAIN_TIMEOUT",
                participants,
                notice_tx,
            );
            let _ = FileExt::unlock(&owner);
            return result;
        }
        std::thread::sleep(Duration::from_millis(25));
    }

    let mut snapshot = initial.clone();
    // Only fence the old generation after every shared writer lease has
    // drained. If maintenance aborts before this point, existing handles stay
    // valid and participants can resume without reopening a stale epoch.
    snapshot.epoch = rotate_epoch(&paths)?;
    write_snapshot(&paths, &snapshot)?;
    let _ = notice_tx.send(MaintenanceNotice::Changed(snapshot.clone()));
    let storage_result = (|| -> Result<()> {
        let mut db = Db::open_for_maintenance(database)?;
        match &request {
            MaintenanceRequest::Restore(entry) => {
                set_stage(
                    &paths,
                    &mut snapshot,
                    MaintenanceStage::CreatingSafetyBackup,
                    notice_tx,
                )?;
                let safety = backup_store.create_safety(&db)?;
                snapshot.safety_backup = Some(safety.path.clone());
                write_snapshot(&paths, &snapshot)?;
                set_stage(
                    &paths,
                    &mut snapshot,
                    MaintenanceStage::Executing,
                    notice_tx,
                )?;
                backup_store.restore_after_safety(&mut db, entry)?;
            }
            MaintenanceRequest::Compact => {
                set_stage(
                    &paths,
                    &mut snapshot,
                    MaintenanceStage::Executing,
                    notice_tx,
                )?;
                db.compact_uncoordinated()?;
            }
        }
        set_stage(
            &paths,
            &mut snapshot,
            MaintenanceStage::Validating,
            notice_tx,
        )?;
        let check = db.integrity_check()?;
        if !check.ok {
            bail!("database integrity check failed: {}", check.details);
        }
        drop(db);
        set_stage(
            &paths,
            &mut snapshot,
            MaintenanceStage::Reopening,
            notice_tx,
        )?;
        Db::open_for_maintenance(database)?;
        Ok(())
    })();

    let _ = FileExt::unlock(&writer);
    if let Err(error) = storage_result {
        let result = finish_failed(
            &paths,
            &snapshot,
            if snapshot.stage == MaintenanceStage::CreatingSafetyBackup {
                MaintenanceFailureKind::SafetyBackup
            } else if snapshot.stage == MaintenanceStage::Validating {
                MaintenanceFailureKind::Integrity
            } else if snapshot.stage == MaintenanceStage::Reopening {
                MaintenanceFailureKind::Reopen
            } else {
                MaintenanceFailureKind::Storage
            },
            "资料维护失败，原资料库或安全副本仍然保留",
            &format!("{error:#}"),
            participants,
            notice_tx,
        );
        let _ = FileExt::unlock(&owner);
        return result;
    }

    snapshot.active = false;
    set_stage(
        &paths,
        &mut snapshot,
        MaintenanceStage::ResumingParticipants,
        notice_tx,
    )?;
    let failed = resume_participants(participants, &snapshot.epoch);
    snapshot.failed_participants = failed;
    snapshot.stage = MaintenanceStage::Finished;
    snapshot.updated_at = Utc::now().timestamp();
    if snapshot.failed_participants.is_empty() {
        snapshot.status = MaintenanceStatus::Succeeded;
        snapshot.user_message = Some(match snapshot.operation {
            MaintenanceOperation::Restore => "资料库恢复完成".into(),
            MaintenanceOperation::Compact => "数据库压缩完成".into(),
            MaintenanceOperation::SchemaMigration => "数据库升级完成".into(),
        });
    } else {
        snapshot.status = MaintenanceStatus::Degraded;
        snapshot.failure_kind = Some(MaintenanceFailureKind::ParticipantResume);
        snapshot.user_message = Some("数据库操作完成，但部分后台任务未能恢复".into());
        snapshot.technical_detail = Some(format!(
            "PARTICIPANT_RESUME_FAILED: {}",
            snapshot.failed_participants.join(", ")
        ));
    }
    write_snapshot(&paths, &snapshot)?;
    let _ = FileExt::unlock(&owner);
    let _ = notice_tx.send(MaintenanceNotice::Changed(snapshot));
    Ok(())
}

fn finish_failed(
    paths: &CoordinationPaths,
    initial: &MaintenanceSnapshot,
    kind: MaintenanceFailureKind,
    user_message: &str,
    technical_detail: &str,
    participants: &[Arc<dyn MaintenanceParticipant>],
    notice_tx: &mpsc::Sender<MaintenanceNotice>,
) -> Result<()> {
    let mut snapshot = initial.clone();
    snapshot.active = false;
    snapshot.stage = MaintenanceStage::ResumingParticipants;
    snapshot.status = MaintenanceStatus::Failed;
    snapshot.failure_kind = Some(kind);
    snapshot.user_message = Some(user_message.into());
    snapshot.technical_detail = Some(sanitize_detail(technical_detail));
    // Publish the released gate before asking self-pausing participants to
    // reopen. Otherwise every resume attempt still observes `active=true`.
    snapshot.updated_at = Utc::now().timestamp();
    write_snapshot(paths, &snapshot)?;
    let _ = notice_tx.send(MaintenanceNotice::Changed(snapshot.clone()));
    snapshot.failed_participants = resume_participants(participants, &snapshot.epoch);
    snapshot.stage = MaintenanceStage::Finished;
    snapshot.updated_at = Utc::now().timestamp();
    write_snapshot(paths, &snapshot)?;
    let _ = notice_tx.send(MaintenanceNotice::Changed(snapshot));
    Ok(())
}

fn resume_participants(
    participants: &[Arc<dyn MaintenanceParticipant>],
    epoch: &str,
) -> Vec<String> {
    let mut failed = Vec::new();
    for participant in participants {
        let mut resumed = false;
        for attempt in 0..RESUME_ATTEMPTS {
            if participant.resume(epoch).is_ok() {
                resumed = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(50 * (attempt as u64 + 1)));
        }
        if !resumed {
            failed.push(participant.name().to_owned());
        }
    }
    failed
}

fn set_stage(
    paths: &CoordinationPaths,
    snapshot: &mut MaintenanceSnapshot,
    stage: MaintenanceStage,
    notice_tx: &mpsc::Sender<MaintenanceNotice>,
) -> Result<()> {
    snapshot.stage = stage;
    snapshot.updated_at = Utc::now().timestamp();
    write_snapshot(paths, snapshot)?;
    let _ = notice_tx.send(MaintenanceNotice::Changed(snapshot.clone()));
    Ok(())
}

fn begin_run(database: &Path, operation: MaintenanceOperation) -> Result<MaintenanceSnapshot> {
    let paths = CoordinationPaths::for_database(database);
    let state = read_state(&paths)?;
    if state.active {
        bail!("MAINTENANCE_IN_PROGRESS: another maintenance run is active");
    }
    let now = Utc::now().timestamp();
    // Publishing `active` stops new permits. Keep the stable generation until
    // all existing permits have drained and the exclusive lock is held.
    let epoch = state.epoch;
    let snapshot = MaintenanceSnapshot {
        run_id: new_epoch(),
        epoch,
        operation,
        stage: MaintenanceStage::WaitingForWriters,
        status: MaintenanceStatus::Running,
        active: true,
        safety_backup: None,
        failure_kind: None,
        user_message: Some("正在等待后台写入停止".into()),
        technical_detail: None,
        failed_participants: Vec::new(),
        started_at: now,
        updated_at: now,
    };
    write_snapshot(&paths, &snapshot)?;
    Ok(snapshot)
}

fn begin_owned_run(
    database: &Path,
    operation: MaintenanceOperation,
) -> Result<(MaintenanceSnapshot, File)> {
    let paths = CoordinationPaths::for_database(database);
    let owner = open_lock_file(&paths.owner_lock)?;
    if FileExt::try_lock_exclusive(&owner).is_err() {
        bail!("MAINTENANCE_IN_PROGRESS: another process owns local-library maintenance");
    }
    match begin_run(database, operation) {
        Ok(snapshot) => Ok((snapshot, owner)),
        Err(error) => {
            let _ = FileExt::unlock(&owner);
            Err(error)
        }
    }
}

fn rotate_epoch(paths: &CoordinationPaths) -> Result<String> {
    ensure_state(paths)?;
    let epoch = new_epoch();
    let conn = Connection::open(&paths.state)?;
    conn.execute(
        "UPDATE maintenance_state SET epoch=?1,updated_at=?2 WHERE singleton_id=1",
        params![epoch, Utc::now().timestamp()],
    )?;
    Ok(epoch)
}

#[derive(Debug)]
struct CoordinationState {
    epoch: String,
    active: bool,
    run: Option<MaintenanceSnapshot>,
}

fn ensure_state(paths: &CoordinationPaths) -> Result<()> {
    if let Some(parent) = paths.state.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let conn = Connection::open(&paths.state)?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS maintenance_state (
           singleton_id INTEGER PRIMARY KEY CHECK(singleton_id=1),
           protocol_version INTEGER NOT NULL,
           epoch TEXT NOT NULL,
           active INTEGER NOT NULL DEFAULT 0 CHECK(active IN (0,1)),
           run_id TEXT,
           operation TEXT,
           stage TEXT,
           status TEXT,
           safety_backup TEXT,
           failure_kind TEXT,
           user_message TEXT,
           technical_detail TEXT,
           failed_participants TEXT NOT NULL DEFAULT '[]',
           started_at INTEGER,
           updated_at INTEGER NOT NULL
         );",
    )?;
    conn.execute(
        "INSERT OR IGNORE INTO maintenance_state(singleton_id,protocol_version,epoch,updated_at)
         VALUES(1,?1,?2,?3)",
        params![PROTOCOL_VERSION, new_epoch(), Utc::now().timestamp()],
    )?;
    let version: i64 = conn.query_row(
        "SELECT protocol_version FROM maintenance_state WHERE singleton_id=1",
        [],
        |row| row.get(0),
    )?;
    if version > PROTOCOL_VERSION {
        bail!(
            "MAINTENANCE_PROTOCOL_NEWER: coordination version {version} is newer than supported {PROTOCOL_VERSION}"
        );
    }
    if version < PROTOCOL_VERSION {
        bail!("missing maintenance coordination migration from version {version}");
    }
    Ok(())
}

fn read_state(paths: &CoordinationPaths) -> Result<CoordinationState> {
    ensure_state(paths)?;
    let conn = Connection::open(&paths.state)?;
    let row = conn
        .query_row(
            "SELECT protocol_version,epoch,active,run_id,operation,stage,status,
                    safety_backup,failure_kind,user_message,technical_detail,
                    failed_participants,started_at,updated_at
             FROM maintenance_state WHERE singleton_id=1",
            [],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, bool>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, Option<String>>(5)?,
                    row.get::<_, Option<String>>(6)?,
                    row.get::<_, Option<String>>(7)?,
                    row.get::<_, Option<String>>(8)?,
                    row.get::<_, Option<String>>(9)?,
                    row.get::<_, Option<String>>(10)?,
                    row.get::<_, String>(11)?,
                    row.get::<_, Option<i64>>(12)?,
                    row.get::<_, i64>(13)?,
                ))
            },
        )
        .optional()?
        .context("maintenance coordination state is missing")?;
    if row.0 > PROTOCOL_VERSION {
        bail!("MAINTENANCE_PROTOCOL_NEWER: coordination file belongs to a newer application");
    }
    let run = match (row.3, row.4, row.5, row.6, row.12) {
        (Some(run_id), Some(operation), Some(stage), Some(status), Some(started_at)) => {
            Some(MaintenanceSnapshot {
                run_id,
                epoch: row.1.clone(),
                operation: MaintenanceOperation::parse(&operation)?,
                stage: MaintenanceStage::parse(&stage)?,
                status: MaintenanceStatus::parse(&status)?,
                active: row.2,
                safety_backup: row.7.map(PathBuf::from),
                failure_kind: row
                    .8
                    .as_deref()
                    .map(MaintenanceFailureKind::parse)
                    .transpose()?,
                user_message: row.9,
                technical_detail: row.10,
                failed_participants: serde_json::from_str(&row.11).unwrap_or_default(),
                started_at,
                updated_at: row.13,
            })
        }
        _ => None,
    };
    Ok(CoordinationState {
        epoch: row.1,
        active: row.2,
        run,
    })
}

fn write_snapshot(paths: &CoordinationPaths, snapshot: &MaintenanceSnapshot) -> Result<()> {
    ensure_state(paths)?;
    let conn = Connection::open(&paths.state)?;
    let changed = conn.execute(
        "UPDATE maintenance_state SET protocol_version=?1,epoch=?2,active=?3,
          run_id=?4,operation=?5,stage=?6,status=?7,safety_backup=?8,
          failure_kind=?9,user_message=?10,technical_detail=?11,
          failed_participants=?12,started_at=?13,updated_at=?14
         WHERE singleton_id=1",
        params![
            PROTOCOL_VERSION,
            snapshot.epoch,
            snapshot.active,
            snapshot.run_id,
            snapshot.operation.as_str(),
            snapshot.stage.as_str(),
            snapshot.status.as_str(),
            snapshot
                .safety_backup
                .as_ref()
                .map(|path| path.to_string_lossy().into_owned()),
            snapshot.failure_kind.map(MaintenanceFailureKind::as_str),
            snapshot.user_message,
            snapshot.technical_detail,
            serde_json::to_string(&snapshot.failed_participants)?,
            snapshot.started_at,
            snapshot.updated_at,
        ],
    )?;
    if changed != 1 {
        bail!("maintenance coordination state update did not affect the singleton row");
    }
    Ok(())
}

fn open_lock_file(path: &Path) -> Result<File> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .with_context(|| format!("open maintenance lock {}", path.display()))
}

fn new_epoch() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let sequence = EPOCH_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{:x}-{:x}-{:x}", std::process::id(), nanos, sequence)
}

fn sanitize_detail(value: &str) -> String {
    let mut value = value.replace(['\r', '\n'], " ");
    value.truncate(4_000);
    value
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize};

    struct FakeParticipant {
        name: &'static str,
        quiesced: AtomicBool,
        resumes: AtomicUsize,
        fail_resume: bool,
    }

    impl FakeParticipant {
        fn healthy(name: &'static str) -> Self {
            Self {
                name,
                quiesced: AtomicBool::new(false),
                resumes: AtomicUsize::new(0),
                fail_resume: false,
            }
        }

        fn failing_resume(name: &'static str) -> Self {
            Self {
                fail_resume: true,
                ..Self::healthy(name)
            }
        }
    }

    impl MaintenanceParticipant for FakeParticipant {
        fn name(&self) -> &'static str {
            self.name
        }

        fn quiesce(&self, _deadline: Instant, _epoch: &str) -> Result<()> {
            self.quiesced.store(true, Ordering::SeqCst);
            Ok(())
        }

        fn resume(&self, _epoch: &str) -> Result<()> {
            self.resumes.fetch_add(1, Ordering::SeqCst);
            if self.fail_resume {
                bail!("resume failed")
            }
            Ok(())
        }
    }

    fn temp_paths(name: &str) -> (PathBuf, BackupStore) {
        let root = std::env::temp_dir().join(format!("rrss-maintenance-{name}-{}", new_epoch()));
        std::fs::create_dir_all(&root).unwrap();
        let db = root.join("library.sqlite3");
        let backups = BackupStore::open(root.join("backups")).unwrap();
        (db, backups)
    }

    fn wait_terminal(engine: &MaintenanceEngine) -> MaintenanceSnapshot {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(snapshot) = engine.snapshot().unwrap()
                && snapshot.status != MaintenanceStatus::Running
            {
                return snapshot;
            }
            assert!(Instant::now() < deadline, "maintenance did not finish");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn writer_opened_before_run_is_stale_after_epoch_changes() {
        let (path, _) = temp_paths("stale");
        let raw = Db::open_for_maintenance(&path).unwrap();
        let witness = MaintenanceFence::witness(&path).unwrap();
        let old = witness.opened_epoch().to_owned();
        let snapshot = begin_run(&path, MaintenanceOperation::Compact).unwrap();
        assert_eq!(snapshot.epoch, old);
        assert!(
            witness
                .begin_write(&raw.conn)
                .unwrap_err()
                .to_string()
                .contains("MAINTENANCE")
        );
        let mut terminal = snapshot;
        terminal.epoch = rotate_epoch(&CoordinationPaths::for_database(&path)).unwrap();
        terminal.active = false;
        terminal.stage = MaintenanceStage::Finished;
        terminal.status = MaintenanceStatus::Failed;
        write_snapshot(&CoordinationPaths::for_database(&path), &terminal).unwrap();
        assert!(
            witness
                .begin_write(&raw.conn)
                .unwrap_err()
                .to_string()
                .contains("STALE_LIBRARY_EPOCH")
        );
    }

    #[test]
    fn fenced_commit_rechecks_maintenance_and_rolls_back() {
        let (path, _) = temp_paths("validate-active");
        let raw = Db::open_for_maintenance(&path).unwrap();
        let witness = MaintenanceFence::witness(&path).unwrap();
        let tx = witness.begin_write(&raw.conn).unwrap();
        tx.execute(
            "INSERT INTO feeds(url,next_fetch) VALUES('https://late.example/feed',1)",
            [],
        )
        .unwrap();
        let mut snapshot = begin_run(&path, MaintenanceOperation::Compact).unwrap();

        assert!(MaintenanceFence::observe(&path).unwrap().is_active());
        let error = tx.commit().unwrap_err().to_string();
        assert!(error.contains("MAINTENANCE_IN_PROGRESS"), "{error}");
        assert!(!error.contains("STALE_LIBRARY_EPOCH"), "{error}");

        snapshot.active = false;
        snapshot.stage = MaintenanceStage::Finished;
        snapshot.status = MaintenanceStatus::Failed;
        write_snapshot(&CoordinationPaths::for_database(&path), &snapshot).unwrap();
        let rows: i64 = raw
            .conn
            .query_row(
                "SELECT COUNT(*) FROM feeds WHERE url='https://late.example/feed'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(rows, 0);
    }

    #[test]
    fn linearized_mutation_acquired_before_maintenance_can_drain() {
        let (path, _) = temp_paths("linearized-drain");
        let mut db = Db::open(&path).unwrap();
        let tx = db.fenced_linearized_immediate_transaction().unwrap();
        tx.execute(
            "INSERT INTO feeds(url,next_fetch) VALUES('https://linearized.example/feed',1)",
            [],
        )
        .unwrap();
        let mut snapshot = begin_run(&path, MaintenanceOperation::Compact).unwrap();

        assert!(MaintenanceFence::observe(&path).unwrap().is_active());
        tx.commit().unwrap();

        snapshot.active = false;
        snapshot.stage = MaintenanceStage::Finished;
        snapshot.status = MaintenanceStatus::Failed;
        write_snapshot(&CoordinationPaths::for_database(&path), &snapshot).unwrap();
        let rows: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM feeds WHERE url='https://linearized.example/feed'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(rows, 1);
    }

    #[test]
    fn compact_runs_through_external_interface_and_resumes_participants() {
        let (path, backups) = temp_paths("compact");
        Db::open_for_maintenance(&path).unwrap();
        let rss = Arc::new(FakeParticipant::healthy("rss"));
        let knowledge = Arc::new(FakeParticipant::healthy("knowledge"));
        let engine =
            MaintenanceEngine::start(path, backups, vec![rss.clone(), knowledge.clone()]).unwrap();
        engine.request(MaintenanceRequest::Compact).unwrap();
        let snapshot = wait_terminal(&engine);
        assert_eq!(snapshot.status, MaintenanceStatus::Succeeded);
        assert!(rss.quiesced.load(Ordering::SeqCst));
        assert!(knowledge.quiesced.load(Ordering::SeqCst));
        assert_eq!(rss.resumes.load(Ordering::SeqCst), 1);
        assert_eq!(knowledge.resumes.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn maintenance_waits_until_a_live_database_connection_is_closed() {
        let (path, backups) = temp_paths("writer-drain");
        let db = Db::open(&path).unwrap();
        let engine = MaintenanceEngine::start(path, backups, Vec::new()).unwrap();
        engine.request(MaintenanceRequest::Compact).unwrap();
        assert!(
            engine
                .request(MaintenanceRequest::Compact)
                .unwrap_err()
                .to_string()
                .contains("MAINTENANCE_IN_PROGRESS")
        );
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(
            engine.snapshot().unwrap().unwrap().status,
            MaintenanceStatus::Running
        );
        drop(db);
        assert_eq!(wait_terminal(&engine).status, MaintenanceStatus::Succeeded);
    }

    #[test]
    fn restore_replaces_post_backup_rows_and_reopens_the_new_generation() {
        let (path, backups) = temp_paths("restore");
        let db = Db::open(&path).unwrap();
        db.add_feed("https://before.example/feed", 1).unwrap();
        let backup = backups
            .create(&db, crate::backup::BackupProtection::Plain)
            .unwrap();
        db.add_feed("https://after.example/feed", 2).unwrap();
        drop(db);

        let engine = MaintenanceEngine::start(path.clone(), backups, Vec::new()).unwrap();
        engine.request(MaintenanceRequest::Restore(backup)).unwrap();
        assert_eq!(wait_terminal(&engine).status, MaintenanceStatus::Succeeded);

        let restored = Db::open(&path).unwrap();
        let urls = restored
            .enabled_feeds()
            .unwrap()
            .into_iter()
            .map(|feed| feed.url)
            .collect::<Vec<_>>();
        assert!(urls.contains(&"https://before.example/feed".to_owned()));
        assert!(!urls.contains(&"https://after.example/feed".to_owned()));
    }

    #[test]
    fn schema_migration_uses_the_exclusive_generation_boundary() {
        let (path, _) = temp_paths("schema");
        ensure_state(&CoordinationPaths::for_database(&path)).unwrap();
        let old_epoch = read_state(&CoordinationPaths::for_database(&path))
            .unwrap()
            .epoch;
        run_schema_migration(&path, || {
            let conn = Connection::open(&path)?;
            conn.execute("CREATE TABLE migrated(value INTEGER NOT NULL)", [])?;
            Ok(())
        })
        .unwrap();
        let state = read_state(&CoordinationPaths::for_database(&path)).unwrap();
        assert!(!state.active);
        assert_ne!(state.epoch, old_epoch);
        let run = state.run.unwrap();
        assert_eq!(run.status, MaintenanceStatus::Succeeded);
        let safety = run
            .safety_backup
            .expect("schema migration records its pre-write safety copy");
        assert!(safety.exists());
    }

    #[test]
    fn interrupted_invalid_database_rolls_back_from_persisted_safety_copy() {
        let (path, backups) = temp_paths("crash-recovery");
        let db = Db::open(&path).unwrap();
        db.add_feed("https://safe.example/feed", 1).unwrap();
        let safety = backups.create_safety(&db).unwrap();
        drop(db);

        let paths = CoordinationPaths::for_database(&path);
        let mut interrupted = begin_run(&path, MaintenanceOperation::Restore).unwrap();
        interrupted.stage = MaintenanceStage::Executing;
        interrupted.safety_backup = Some(safety.path);
        write_snapshot(&paths, &interrupted).unwrap();
        std::fs::write(&path, b"not a sqlite database").unwrap();

        let engine = MaintenanceEngine::start(path.clone(), backups, Vec::new()).unwrap();
        let recovered = engine.snapshot().unwrap().unwrap();
        assert_eq!(recovered.status, MaintenanceStatus::Failed);
        assert_eq!(
            recovered.technical_detail.as_deref(),
            Some("INTERRUPTED_MAINTENANCE_ROLLED_BACK")
        );
        assert!(
            Db::open(&path)
                .unwrap()
                .enabled_feeds()
                .unwrap()
                .iter()
                .any(|feed| feed.url == "https://safe.example/feed")
        );
    }

    #[test]
    fn participant_resume_failure_is_degraded_not_false_success() {
        let (path, backups) = temp_paths("degraded");
        Db::open_for_maintenance(&path).unwrap();
        let failed = Arc::new(FakeParticipant::failing_resume("knowledge"));
        let engine = MaintenanceEngine::start(path, backups, vec![failed.clone()]).unwrap();
        engine.request(MaintenanceRequest::Compact).unwrap();
        let snapshot = wait_terminal(&engine);
        assert_eq!(snapshot.status, MaintenanceStatus::Degraded);
        assert_eq!(snapshot.failed_participants, vec!["knowledge"]);
        assert_eq!(failed.resumes.load(Ordering::SeqCst), RESUME_ATTEMPTS);
    }

    #[test]
    fn unknown_newer_protocol_fails_closed() {
        let (path, _) = temp_paths("protocol");
        let paths = CoordinationPaths::for_database(&path);
        ensure_state(&paths).unwrap();
        let conn = Connection::open(&paths.state).unwrap();
        conn.execute(
            "UPDATE maintenance_state SET protocol_version=?1 WHERE singleton_id=1",
            [PROTOCOL_VERSION + 1],
        )
        .unwrap();
        assert!(
            MaintenanceFence::witness(&path)
                .unwrap_err()
                .to_string()
                .contains("NEWER")
        );
    }
}
