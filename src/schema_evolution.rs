//! Versioned evolution for the local library schema (ADR-0011).
//!
//! This module is the only authority that classifies, creates, or advances a
//! Shiyue library schema.  Callers either receive a fully verified current
//! schema or a typed failure; they never install individual tables or indexes.

use anyhow::Context as _;
use chrono::Utc;
use rusqlite::Connection;
use std::collections::BTreeSet;
use thiserror::Error;

use crate::{
    excerpt_thought_lifecycle, library_projection_revision, library_search, web_clipping_lifecycle,
};

pub(crate) const CURRENT_SCHEMA_VERSION: i64 = 8;
const MAX_DETAIL_CHARS: usize = 2_000;

/// The oldest released Shiyue schema.  A brand-new library starts here and
/// then follows the same transitions as every historical library.
const V0_BOOTSTRAP_SCHEMA: &str = r#"
CREATE TABLE feeds (
  id            INTEGER PRIMARY KEY,
  url           TEXT NOT NULL UNIQUE,
  title         TEXT,
  interval_secs INTEGER,
  last_fetch    INTEGER,
  next_fetch    INTEGER NOT NULL DEFAULT 0,
  last_error    TEXT,
  fail_count    INTEGER NOT NULL DEFAULT 0,
  disabled      INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE articles (
  id         INTEGER PRIMARY KEY,
  feed_id    INTEGER NOT NULL REFERENCES feeds(id) ON DELETE CASCADE,
  entry_id   TEXT NOT NULL,
  url        TEXT,
  title      TEXT,
  author     TEXT,
  published  INTEGER,
  content    TEXT,
  is_read    INTEGER NOT NULL DEFAULT 0,
  starred    INTEGER NOT NULL DEFAULT 0,
  archived   INTEGER NOT NULL DEFAULT 0,
  fetched_at INTEGER NOT NULL,
  UNIQUE(feed_id, entry_id)
);
CREATE TABLE article_selections (
  id            INTEGER PRIMARY KEY,
  article_id    INTEGER NOT NULL REFERENCES articles(id) ON DELETE CASCADE,
  selected_text TEXT NOT NULL CHECK (length(trim(selected_text)) > 0),
  start_offset  INTEGER,
  end_offset    INTEGER,
  comment       TEXT,
  is_favorite   INTEGER NOT NULL DEFAULT 0 CHECK (is_favorite IN (0, 1)),
  created_at    INTEGER NOT NULL,
  updated_at    INTEGER NOT NULL
);
CREATE INDEX idx_article_selections_article
  ON article_selections(article_id, created_at DESC, id DESC);
CREATE INDEX idx_article_selections_favorite
  ON article_selections(is_favorite, created_at DESC, id DESC);
"#;

/// Version 1 completes every known unversioned release (v0.1.0-v0.5.0) into
/// one canonical foundation. Existing objects are accepted only after the
/// v0 precondition recognises the released core schema.
const V1_FOUNDATION_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS tags (
  id         INTEGER PRIMARY KEY,
  name       TEXT NOT NULL COLLATE NOCASE UNIQUE,
  created_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS article_tags (
  article_id INTEGER NOT NULL REFERENCES articles(id) ON DELETE CASCADE,
  tag_id     INTEGER NOT NULL REFERENCES tags(id) ON DELETE CASCADE,
  created_at INTEGER NOT NULL,
  PRIMARY KEY(article_id, tag_id)
);
CREATE INDEX IF NOT EXISTS idx_article_tags_tag ON article_tags(tag_id, article_id);
CREATE TABLE IF NOT EXISTS article_ai (
  article_id     INTEGER PRIMARY KEY REFERENCES articles(id) ON DELETE CASCADE,
  summary_zh     TEXT NOT NULL,
  translation_zh TEXT NOT NULL,
  model          TEXT NOT NULL,
  updated_at     INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS search_history (
  query         TEXT PRIMARY KEY COLLATE NOCASE,
  last_used_at  INTEGER NOT NULL,
  use_count     INTEGER NOT NULL DEFAULT 1,
  result_count  INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS resources (
  id                 INTEGER PRIMARY KEY,
  url                TEXT NOT NULL,
  canonical_url      TEXT NOT NULL UNIQUE,
  parent_resource_id INTEGER REFERENCES resources(id) ON DELETE SET NULL,
  linked_article_id  INTEGER REFERENCES articles(id) ON DELETE SET NULL,
  kind               TEXT NOT NULL CHECK (kind IN ('site', 'page', 'article')),
  title              TEXT,
  purpose_zh         TEXT,
  purpose_source     TEXT CHECK (purpose_source IS NULL OR purpose_source IN ('manual','ai')),
  use_when_zh        TEXT,
  use_when_source    TEXT CHECK (use_when_source IS NULL OR use_when_source IN ('manual','ai')),
  capabilities       TEXT NOT NULL DEFAULT '[]',
  limitations        TEXT NOT NULL DEFAULT '[]',
  pricing            TEXT CHECK (pricing IS NULL OR pricing IN ('free', 'freemium', 'paid', 'unknown')),
  requires_login     INTEGER CHECK (requires_login IS NULL OR requires_login IN (0, 1)),
  languages          TEXT NOT NULL DEFAULT '[]',
  private_note       TEXT,
  privacy            TEXT NOT NULL DEFAULT 'public' CHECK (privacy IN ('public', 'private')),
  status             TEXT NOT NULL CHECK (status IN ('pending_review', 'enrichment_pending', 'active', 'broken', 'archived')),
  source             TEXT NOT NULL CHECK (source IN ('gui', 'cli_agent', 'import')),
  manual_rating      INTEGER CHECK (manual_rating IS NULL OR manual_rating BETWEEN 1 AND 5),
  latest_snapshot_id INTEGER REFERENCES resource_snapshots(id) ON DELETE SET NULL,
  last_checked_at    INTEGER,
  created_at         INTEGER NOT NULL,
  updated_at         INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_resources_status_updated ON resources(status, updated_at DESC);
CREATE INDEX IF NOT EXISTS idx_resources_parent ON resources(parent_resource_id);
CREATE INDEX IF NOT EXISTS idx_resources_article ON resources(linked_article_id);
CREATE UNIQUE INDEX IF NOT EXISTS idx_resources_import_article
  ON resources(linked_article_id) WHERE source='import' AND linked_article_id IS NOT NULL;
CREATE TABLE IF NOT EXISTS resource_categories (
  resource_id INTEGER NOT NULL REFERENCES resources(id) ON DELETE CASCADE,
  category    TEXT NOT NULL CHECK (category IN ('tool', 'asset-library', 'docs', 'blog', 'inspiration', 'service', 'repository', 'other')),
  PRIMARY KEY(resource_id, category)
);
CREATE TABLE IF NOT EXISTS resource_tags (
  resource_id INTEGER NOT NULL REFERENCES resources(id) ON DELETE CASCADE,
  name        TEXT NOT NULL COLLATE NOCASE,
  language    TEXT NOT NULL CHECK (language IN ('zh', 'en', 'other')),
  source      TEXT NOT NULL CHECK (source IN ('manual', 'ai')),
  created_at  INTEGER NOT NULL,
  PRIMARY KEY(resource_id, name)
);
CREATE TABLE IF NOT EXISTS resource_snapshots (
  id              INTEGER PRIMARY KEY,
  resource_id     INTEGER NOT NULL REFERENCES resources(id) ON DELETE CASCADE,
  content_hash    TEXT,
  fetched_url     TEXT,
  http_status     INTEGER,
  title           TEXT,
  cleaned_content TEXT,
  fetched_at      INTEGER NOT NULL,
  fetch_error     TEXT,
  CHECK (content_hash IS NOT NULL OR fetch_error IS NOT NULL)
);
CREATE UNIQUE INDEX IF NOT EXISTS idx_resource_snapshot_success
  ON resource_snapshots(resource_id, content_hash) WHERE content_hash IS NOT NULL;
CREATE TABLE IF NOT EXISTS resource_enrichment_runs (
  id             INTEGER PRIMARY KEY,
  resource_id    INTEGER NOT NULL REFERENCES resources(id) ON DELETE CASCADE,
  snapshot_id    INTEGER REFERENCES resource_snapshots(id) ON DELETE SET NULL,
  provider       TEXT NOT NULL,
  model          TEXT NOT NULL,
  prompt_version TEXT NOT NULL,
  schema_version TEXT NOT NULL,
  started_at     INTEGER NOT NULL,
  finished_at    INTEGER,
  status         TEXT NOT NULL CHECK (status IN ('pending', 'running', 'succeeded', 'failed')),
  error_code     TEXT,
  error_message  TEXT
);
CREATE TABLE IF NOT EXISTS resource_usage_events (
  id          INTEGER PRIMARY KEY,
  resource_id INTEGER NOT NULL REFERENCES resources(id) ON DELETE CASCADE,
  event       TEXT NOT NULL CHECK (event IN ('returned', 'confirmed_used')),
  occurred_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_resource_usage_events
  ON resource_usage_events(resource_id, occurred_at DESC);
"#;

const V2_KNOWLEDGE_SCHEMA: &str = r#"
CREATE TABLE knowledge_tasks (
  id            INTEGER PRIMARY KEY,
  kind          TEXT NOT NULL CHECK (kind IN ('resource_completion', 'article_summary')),
  target_id     INTEGER NOT NULL,
  status        TEXT NOT NULL CHECK (status IN ('queued', 'running', 'succeeded', 'failed', 'interrupted')),
  current_stage TEXT,
  next_run_at   INTEGER NOT NULL DEFAULT 0,
  created_at    INTEGER NOT NULL,
  updated_at    INTEGER NOT NULL
);
CREATE UNIQUE INDEX idx_knowledge_tasks_active_target
  ON knowledge_tasks(kind, target_id) WHERE status IN ('queued', 'running');
CREATE INDEX idx_knowledge_tasks_target_history
  ON knowledge_tasks(kind, target_id, created_at DESC, id DESC);
CREATE TABLE knowledge_task_attempts (
  id               INTEGER PRIMARY KEY,
  task_id          INTEGER NOT NULL REFERENCES knowledge_tasks(id) ON DELETE CASCADE,
  attempt_number   INTEGER NOT NULL,
  status           TEXT NOT NULL CHECK (status IN ('queued', 'running', 'succeeded', 'failed', 'interrupted')),
  current_stage    TEXT,
  automatic_retry INTEGER NOT NULL DEFAULT 0 CHECK (automatic_retry IN (0, 1)),
  started_at       INTEGER,
  finished_at      INTEGER,
  error_kind       TEXT CHECK (error_kind IS NULL OR error_kind IN ('transient', 'authentication', 'security', 'input', 'provider_output', 'storage', 'interrupted')),
  user_message     TEXT,
  technical_detail TEXT,
  created_at       INTEGER NOT NULL,
  UNIQUE(task_id, attempt_number)
);
CREATE INDEX idx_knowledge_attempts_task
  ON knowledge_task_attempts(task_id, attempt_number DESC);
"#;

const V3_EXECUTOR_SCHEMA: &str = r#"
CREATE TABLE knowledge_executor_lease (
  singleton_id INTEGER PRIMARY KEY CHECK (singleton_id = 1),
  owner_id TEXT,
  generation INTEGER NOT NULL DEFAULT 0,
  heartbeat_at INTEGER NOT NULL DEFAULT 0
);
INSERT INTO knowledge_executor_lease(singleton_id) VALUES(1);
CREATE TABLE knowledge_change_clock (
  singleton_id INTEGER PRIMARY KEY CHECK (singleton_id = 1),
  sequence INTEGER NOT NULL DEFAULT 0
);
INSERT INTO knowledge_change_clock(singleton_id) VALUES(1);
"#;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SchemaReadiness {
    Uninitialized,
    Ready { version: i64 },
    NeedsEvolution { from: i64, to: i64 },
    Drifted { version: i64, detail: String },
    NewerUnsupported { found: i64, supported: i64 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EvolutionStage {
    Inspecting,
    Bootstrapping,
    ApplyingTransition,
    VerifyingTransition,
    VerifyingLibrary,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EvolutionFailureKind {
    Storage,
    SchemaDrift,
    NewerUnsupported,
    Validation,
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
#[error("{user_message}: {technical_detail}")]
pub(crate) struct EvolutionFailure {
    pub(crate) kind: EvolutionFailureKind,
    pub(crate) stage: EvolutionStage,
    pub(crate) transition: Option<(i64, i64)>,
    pub(crate) last_complete_version: Option<i64>,
    pub(crate) user_message: String,
    pub(crate) technical_detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EvolutionReport {
    pub(crate) initial: SchemaReadiness,
    pub(crate) target_version: i64,
    pub(crate) completed_transitions: Vec<(i64, i64)>,
    pub(crate) final_readiness: SchemaReadiness,
    pub(crate) verification_summary: String,
}

impl EvolutionFailure {
    fn new(
        kind: EvolutionFailureKind,
        stage: EvolutionStage,
        transition: Option<(i64, i64)>,
        last_complete_version: Option<i64>,
        user_message: impl Into<String>,
        technical_detail: impl std::fmt::Display,
    ) -> Self {
        Self {
            kind,
            stage,
            transition,
            last_complete_version,
            user_message: user_message.into(),
            technical_detail: bounded_detail(technical_detail),
        }
    }

    fn storage(
        stage: EvolutionStage,
        transition: Option<(i64, i64)>,
        last_complete_version: Option<i64>,
        error: impl std::fmt::Display,
    ) -> Self {
        Self::new(
            EvolutionFailureKind::Storage,
            stage,
            transition,
            last_complete_version,
            "无法更新本地资料库结构",
            error,
        )
    }

    fn drift(version: i64, detail: impl std::fmt::Display) -> Self {
        Self::new(
            EvolutionFailureKind::SchemaDrift,
            EvolutionStage::Inspecting,
            None,
            Some(version),
            "资料库版本与实际结构不一致",
            detail,
        )
    }
}

pub(crate) fn inspect(conn: &Connection) -> Result<SchemaReadiness, EvolutionFailure> {
    let version = schema_version(conn).map_err(|error| {
        EvolutionFailure::storage(EvolutionStage::Inspecting, None, None, error)
    })?;
    if version > CURRENT_SCHEMA_VERSION {
        return Ok(SchemaReadiness::NewerUnsupported {
            found: version,
            supported: CURRENT_SCHEMA_VERSION,
        });
    }

    let owned_tables = owned_table_names(conn).map_err(|error| {
        EvolutionFailure::storage(EvolutionStage::Inspecting, None, Some(version), error)
    })?;
    if owned_tables.is_empty() {
        return if version == 0 {
            Ok(SchemaReadiness::Uninitialized)
        } else {
            Ok(SchemaReadiness::Drifted {
                version,
                detail: "SCHEMA_VERSION_WITHOUT_LIBRARY_TABLES".into(),
            })
        };
    }

    if let Err(detail) = verify_version(conn, version) {
        return Ok(SchemaReadiness::Drifted {
            version,
            detail: bounded_detail(detail),
        });
    }
    if version == CURRENT_SCHEMA_VERSION {
        Ok(SchemaReadiness::Ready { version })
    } else {
        Ok(SchemaReadiness::NeedsEvolution {
            from: version,
            to: CURRENT_SCHEMA_VERSION,
        })
    }
}

/// Prepare one connection while its caller owns the exclusive maintenance
/// window. Each version transition commits independently and is verified
/// before its `user_version` advances.
pub(crate) fn evolve(conn: &Connection) -> Result<EvolutionReport, EvolutionFailure> {
    let initial = inspect(conn)?;
    match &initial {
        SchemaReadiness::Drifted { version, detail } => {
            return Err(EvolutionFailure::drift(*version, detail));
        }
        SchemaReadiness::NewerUnsupported { found, supported } => {
            return Err(EvolutionFailure::new(
                EvolutionFailureKind::NewerUnsupported,
                EvolutionStage::Inspecting,
                None,
                Some(*found),
                "该资料库由更新版本的拾阅创建，请升级程序",
                format!("NEWER_SCHEMA_VERSION: found={found}, supported={supported}"),
            ));
        }
        SchemaReadiness::Uninitialized
        | SchemaReadiness::Ready { .. }
        | SchemaReadiness::NeedsEvolution { .. } => {}
    }

    if matches!(initial, SchemaReadiness::Uninitialized) {
        let tx = conn.unchecked_transaction().map_err(|error| {
            EvolutionFailure::storage(EvolutionStage::Bootstrapping, None, Some(0), error)
        })?;
        tx.execute_batch(V0_BOOTSTRAP_SCHEMA).map_err(|error| {
            EvolutionFailure::storage(EvolutionStage::Bootstrapping, None, Some(0), error)
        })?;
        verify_version(&tx, 0).map_err(|error| {
            EvolutionFailure::new(
                EvolutionFailureKind::Validation,
                EvolutionStage::Bootstrapping,
                None,
                Some(0),
                "无法创建本地资料库结构",
                error,
            )
        })?;
        tx.commit().map_err(|error| {
            EvolutionFailure::storage(EvolutionStage::Bootstrapping, None, Some(0), error)
        })?;
    }

    let mut version = schema_version(conn).map_err(|error| {
        EvolutionFailure::storage(EvolutionStage::Inspecting, None, None, error)
    })?;
    let mut completed = Vec::new();
    while version < CURRENT_SCHEMA_VERSION {
        let next = version + 1;
        let transition = Some((version, next));
        let tx = conn.unchecked_transaction().map_err(|error| {
            EvolutionFailure::storage(
                EvolutionStage::ApplyingTransition,
                transition,
                Some(version),
                error,
            )
        })?;
        apply_transition(&tx, version).map_err(|error| {
            EvolutionFailure::storage(
                EvolutionStage::ApplyingTransition,
                transition,
                Some(version),
                error,
            )
        })?;
        verify_version(&tx, next).map_err(|error| {
            EvolutionFailure::new(
                EvolutionFailureKind::Validation,
                EvolutionStage::VerifyingTransition,
                transition,
                Some(version),
                format!("资料库版本转换 v{version}→v{next} 验证失败"),
                error,
            )
        })?;
        verify_foreign_keys(&tx).map_err(|error| {
            EvolutionFailure::new(
                EvolutionFailureKind::Validation,
                EvolutionStage::VerifyingTransition,
                transition,
                Some(version),
                format!("资料库版本转换 v{version}→v{next} 外键验证失败"),
                error,
            )
        })?;
        tx.pragma_update(None, "user_version", next)
            .map_err(|error| {
                EvolutionFailure::storage(
                    EvolutionStage::ApplyingTransition,
                    transition,
                    Some(version),
                    error,
                )
            })?;
        tx.commit().map_err(|error| {
            EvolutionFailure::storage(
                EvolutionStage::ApplyingTransition,
                transition,
                Some(version),
                error,
            )
        })?;
        completed.push((version, next));
        version = next;
    }

    verify_integrity(conn).map_err(|error| {
        EvolutionFailure::new(
            EvolutionFailureKind::Validation,
            EvolutionStage::VerifyingLibrary,
            None,
            Some(version),
            "资料库结构已转换，但完整性验证失败",
            error,
        )
    })?;
    let final_readiness = inspect(conn)?;
    if !matches!(final_readiness, SchemaReadiness::Ready { .. }) {
        return Err(EvolutionFailure::drift(
            version,
            format!("FINAL_SCHEMA_NOT_READY: {final_readiness:?}"),
        ));
    }
    Ok(EvolutionReport {
        initial,
        target_version: CURRENT_SCHEMA_VERSION,
        completed_transitions: completed,
        final_readiness,
        verification_summary: "integrity_check=ok; foreign_key_check=ok".into(),
    })
}

pub(crate) fn require_ready(conn: &Connection) -> Result<(), EvolutionFailure> {
    match inspect(conn)? {
        SchemaReadiness::Ready { .. } => Ok(()),
        SchemaReadiness::NewerUnsupported { found, supported } => Err(EvolutionFailure::new(
            EvolutionFailureKind::NewerUnsupported,
            EvolutionStage::Inspecting,
            None,
            Some(found),
            "该资料库由更新版本的拾阅创建，请升级程序",
            format!("NEWER_SCHEMA_VERSION: found={found}, supported={supported}"),
        )),
        SchemaReadiness::Drifted { version, detail } => {
            Err(EvolutionFailure::drift(version, detail))
        }
        readiness => Err(EvolutionFailure::new(
            EvolutionFailureKind::Validation,
            EvolutionStage::Inspecting,
            None,
            readiness_version(&readiness),
            "资料库结构尚未准备完成",
            format!("SCHEMA_NOT_READY: {readiness:?}"),
        )),
    }
}

/// Builds an exact intermediate schema for migration tests without exposing
/// transition functions to production callers.
#[cfg(test)]
pub(crate) fn evolve_fixture_to(conn: &Connection, target: i64) -> anyhow::Result<()> {
    anyhow::ensure!((0..=CURRENT_SCHEMA_VERSION).contains(&target));
    if matches!(inspect(conn)?, SchemaReadiness::Uninitialized) {
        conn.execute_batch(V0_BOOTSTRAP_SCHEMA)?;
    }
    let mut version = schema_version(conn)?;
    anyhow::ensure!(version <= target, "fixture is newer than requested target");
    while version < target {
        let tx = conn.unchecked_transaction()?;
        apply_transition(&tx, version)?;
        verify_version(&tx, version + 1)?;
        verify_foreign_keys(&tx)?;
        tx.pragma_update(None, "user_version", version + 1)?;
        tx.commit()?;
        version += 1;
    }
    verify_version(conn, target)
}

fn apply_transition(conn: &Connection, from: i64) -> anyhow::Result<()> {
    match from {
        0 => migrate_v0_to_v1(conn),
        1 => migrate_v1_to_v2(conn),
        2 => migrate_v2_to_v3(conn),
        3 => migrate_v3_to_v4(conn),
        4 => library_search::migrate_to_v5(conn),
        5 => web_clipping_lifecycle::migrate_to_v6(conn),
        6 => excerpt_thought_lifecycle::migrate_to_v7(conn),
        7 => library_projection_revision::migrate_to_v8(conn),
        _ => anyhow::bail!("MISSING_SCHEMA_TRANSITION: from={from}"),
    }
}

fn migrate_v0_to_v1(conn: &Connection) -> anyhow::Result<()> {
    add_column_if_missing(
        conn,
        "articles",
        "archived",
        "ALTER TABLE articles ADD COLUMN archived INTEGER NOT NULL DEFAULT 0",
    )?;
    add_column_if_missing(
        conn,
        "articles",
        "read_later",
        "ALTER TABLE articles ADD COLUMN read_later INTEGER NOT NULL DEFAULT 0",
    )?;
    add_column_if_missing(
        conn,
        "article_selections",
        "anchor_prefix",
        "ALTER TABLE article_selections ADD COLUMN anchor_prefix TEXT NOT NULL DEFAULT ''",
    )?;
    add_column_if_missing(
        conn,
        "article_selections",
        "anchor_suffix",
        "ALTER TABLE article_selections ADD COLUMN anchor_suffix TEXT NOT NULL DEFAULT ''",
    )?;
    conn.execute_batch(V1_FOUNDATION_SCHEMA)?;
    add_column_if_missing(
        conn,
        "resources",
        "purpose_source",
        "ALTER TABLE resources ADD COLUMN purpose_source TEXT CHECK (purpose_source IS NULL OR purpose_source IN ('manual','ai'))",
    )?;
    add_column_if_missing(
        conn,
        "resources",
        "use_when_source",
        "ALTER TABLE resources ADD COLUMN use_when_source TEXT CHECK (use_when_source IS NULL OR use_when_source IN ('manual','ai'))",
    )?;
    Ok(())
}

fn migrate_v1_to_v2(conn: &Connection) -> anyhow::Result<()> {
    if has_table(conn, "knowledge_tasks")? || has_table(conn, "knowledge_task_attempts")? {
        verify_required_tables(conn, &["knowledge_tasks", "knowledge_task_attempts"])?;
    } else {
        conn.execute_batch(V2_KNOWLEDGE_SCHEMA)?;
    }
    conn.execute(
        "UPDATE resource_enrichment_runs
         SET status='failed', finished_at=COALESCE(finished_at, strftime('%s','now')),
             error_code=COALESCE(error_code, 'UPGRADE_INTERRUPTED'),
             error_message=COALESCE(error_message, '升级前任务已中断')
         WHERE status IN ('pending', 'running')",
        [],
    )?;
    Ok(())
}

fn migrate_v2_to_v3(conn: &Connection) -> anyhow::Result<()> {
    add_column_if_missing(
        conn,
        "knowledge_tasks",
        "change_seq",
        "ALTER TABLE knowledge_tasks ADD COLUMN change_seq INTEGER NOT NULL DEFAULT 0",
    )?;
    add_column_if_missing(
        conn,
        "knowledge_task_attempts",
        "claim_generation",
        "ALTER TABLE knowledge_task_attempts ADD COLUMN claim_generation INTEGER",
    )?;
    add_column_if_missing(
        conn,
        "resource_enrichment_runs",
        "attempt_id",
        "ALTER TABLE resource_enrichment_runs ADD COLUMN attempt_id INTEGER REFERENCES knowledge_task_attempts(id) ON DELETE SET NULL",
    )?;
    if has_table(conn, "knowledge_executor_lease")? || has_table(conn, "knowledge_change_clock")? {
        verify_required_tables(
            conn,
            &["knowledge_executor_lease", "knowledge_change_clock"],
        )?;
    } else {
        conn.execute_batch(V3_EXECUTOR_SCHEMA)?;
    }
    let now = Utc::now().timestamp();
    conn.execute(
        "UPDATE knowledge_task_attempts
         SET status='interrupted',finished_at=COALESCE(finished_at,?1),
             error_kind='interrupted',user_message='程序升级后需要重新执行',
             technical_detail='WORKFLOW_UPGRADE_INTERRUPTED: v2 running attempt had no fencing generation'
         WHERE status='running'",
        [now],
    )?;
    conn.execute(
        "UPDATE knowledge_tasks SET status='interrupted',current_stage=NULL,updated_at=?1
         WHERE status='running'",
        [now],
    )?;
    conn.execute(
        "UPDATE resource_enrichment_runs
         SET status='failed',finished_at=COALESCE(finished_at,?1),
             error_code=COALESCE(error_code,'WORKFLOW_UPGRADE_INTERRUPTED'),
             error_message=COALESCE(error_message,'知识处理工作流升级中断了旧执行')
         WHERE status IN ('pending','running')",
        [now],
    )?;
    conn.execute(
        "UPDATE knowledge_change_clock
         SET sequence=COALESCE((SELECT MAX(id) FROM knowledge_tasks),0) WHERE singleton_id=1",
        [],
    )?;
    conn.execute(
        "UPDATE knowledge_tasks SET change_seq=id WHERE change_seq=0",
        [],
    )?;
    Ok(())
}

fn migrate_v3_to_v4(conn: &Connection) -> anyhow::Result<()> {
    for (name, sql) in [
        (
            "curation_state",
            "ALTER TABLE resources ADD COLUMN curation_state TEXT NOT NULL DEFAULT 'active' CHECK (curation_state IN ('pending_review', 'active', 'archived'))",
        ),
        (
            "health",
            "ALTER TABLE resources ADD COLUMN health TEXT NOT NULL DEFAULT 'unknown' CHECK (health IN ('unknown', 'healthy', 'broken'))",
        ),
        (
            "categories_source",
            "ALTER TABLE resources ADD COLUMN categories_source TEXT NOT NULL DEFAULT 'ai' CHECK (categories_source IN ('manual', 'ai'))",
        ),
        (
            "tags_source",
            "ALTER TABLE resources ADD COLUMN tags_source TEXT NOT NULL DEFAULT 'ai' CHECK (tags_source IN ('manual', 'ai'))",
        ),
        (
            "source_failure_count",
            "ALTER TABLE resources ADD COLUMN source_failure_count INTEGER NOT NULL DEFAULT 0",
        ),
    ] {
        add_column_if_missing(conn, "resources", name, sql)?;
    }
    conn.execute_batch(
        "UPDATE resources
         SET curation_state=CASE status WHEN 'pending_review' THEN 'pending_review' WHEN 'archived' THEN 'archived' ELSE 'active' END,
             health=CASE WHEN status='broken' THEN 'broken'
                         WHEN EXISTS (SELECT 1 FROM resource_snapshots s WHERE s.resource_id=resources.id AND s.content_hash IS NOT NULL) THEN 'healthy'
                         ELSE 'unknown' END,
             categories_source=CASE WHEN EXISTS (SELECT 1 FROM resource_enrichment_runs e WHERE e.resource_id=resources.id AND e.status='succeeded') THEN 'ai' ELSE 'manual' END,
             tags_source=CASE WHEN EXISTS (SELECT 1 FROM resource_tags t WHERE t.resource_id=resources.id AND t.source='manual') THEN 'manual' ELSE 'ai' END;",
    )?;
    if !has_index(conn, "idx_resources_curation_updated")? {
        conn.execute_batch(
            "CREATE INDEX idx_resources_curation_updated
             ON resources(curation_state, updated_at DESC, id DESC);",
        )?;
    }
    if !has_index(conn, "idx_resources_health_updated")? {
        conn.execute_batch(
            "CREATE INDEX idx_resources_health_updated
             ON resources(health, updated_at DESC, id DESC);",
        )?;
    }
    Ok(())
}

fn verify_version(conn: &Connection, version: i64) -> anyhow::Result<()> {
    verify_required_tables(conn, &["feeds", "articles", "article_selections"])?;
    verify_required_columns(
        conn,
        "feeds",
        &[
            "id",
            "url",
            "title",
            "interval_secs",
            "last_fetch",
            "next_fetch",
            "last_error",
            "fail_count",
            "disabled",
        ],
    )?;
    verify_required_columns(
        conn,
        "articles",
        &[
            "id",
            "feed_id",
            "entry_id",
            "url",
            "title",
            "author",
            "published",
            "content",
            "is_read",
            "starred",
            "fetched_at",
        ],
    )?;
    verify_required_columns(
        conn,
        "article_selections",
        &[
            "id",
            "article_id",
            "selected_text",
            "start_offset",
            "end_offset",
            "comment",
            "is_favorite",
            "created_at",
            "updated_at",
        ],
    )?;
    if version == 0 {
        return Ok(());
    }

    verify_required_tables(
        conn,
        &[
            "tags",
            "article_tags",
            "article_ai",
            "search_history",
            "resources",
            "resource_categories",
            "resource_tags",
            "resource_snapshots",
            "resource_enrichment_runs",
            "resource_usage_events",
        ],
    )?;
    verify_required_columns(conn, "articles", &["read_later", "archived"])?;
    verify_required_columns(
        conn,
        "article_selections",
        &["anchor_prefix", "anchor_suffix"],
    )?;
    verify_required_columns(conn, "resources", &["purpose_source", "use_when_source"])?;
    if version == 1 {
        return Ok(());
    }

    verify_required_tables(conn, &["knowledge_tasks", "knowledge_task_attempts"])?;
    if version == 2 {
        return Ok(());
    }

    verify_required_tables(
        conn,
        &["knowledge_executor_lease", "knowledge_change_clock"],
    )?;
    verify_required_columns(conn, "knowledge_tasks", &["change_seq"])?;
    verify_required_columns(conn, "knowledge_task_attempts", &["claim_generation"])?;
    verify_required_columns(conn, "resource_enrichment_runs", &["attempt_id"])?;
    if version == 3 {
        return Ok(());
    }

    verify_required_columns(
        conn,
        "resources",
        &[
            "curation_state",
            "health",
            "categories_source",
            "tags_source",
            "source_failure_count",
        ],
    )?;
    verify_required_indexes(
        conn,
        &[
            "idx_resources_curation_updated",
            "idx_resources_health_updated",
        ],
    )?;
    if version == 4 {
        return Ok(());
    }

    library_search::verify_schema(conn)?;
    if version == 5 {
        return Ok(());
    }

    web_clipping_lifecycle::verify_schema_v6(conn)?;
    if version == 6 {
        return Ok(());
    }

    excerpt_thought_lifecycle::verify_schema_v7(conn)?;
    if version == 7 {
        verify_current_constraints(conn)?;
        return Ok(());
    }

    library_projection_revision::verify_schema_v8(conn)?;
    verify_current_constraints(conn)?;
    Ok(())
}

fn verify_current_constraints(conn: &Connection) -> anyhow::Result<()> {
    let resources_sql = table_sql(conn, "resources")?.to_ascii_lowercase();
    for required in [
        "curation_state",
        "pending_review",
        "health",
        "source_failure_count",
    ] {
        anyhow::ensure!(
            resources_sql.contains(required),
            "SCHEMA_FINGERPRINT_MISMATCH: resources missing {required}"
        );
    }
    let clipping_sql = table_sql(conn, "web_clippings")?.to_ascii_lowercase();
    for required in ["input_kind", "provenance_state", "final_url", "pasted_html"] {
        anyhow::ensure!(
            clipping_sql.contains(required),
            "SCHEMA_FINGERPRINT_MISMATCH: web_clippings missing {required}"
        );
    }
    Ok(())
}

fn verify_required_tables(conn: &Connection, required: &[&str]) -> anyhow::Result<()> {
    let tables = owned_table_names(conn)?;
    for table in required {
        anyhow::ensure!(tables.contains(*table), "SCHEMA_MISSING_TABLE: {table}");
    }
    Ok(())
}

fn verify_required_columns(
    conn: &Connection,
    table: &str,
    required: &[&str],
) -> anyhow::Result<()> {
    let columns = table_columns(conn, table)?;
    for column in required {
        anyhow::ensure!(
            columns.contains(*column),
            "SCHEMA_MISSING_COLUMN: {table}.{column}"
        );
    }
    Ok(())
}

fn verify_required_indexes(conn: &Connection, required: &[&str]) -> anyhow::Result<()> {
    for index in required {
        anyhow::ensure!(has_index(conn, index)?, "SCHEMA_MISSING_INDEX: {index}");
    }
    Ok(())
}

fn verify_foreign_keys(conn: &Connection) -> anyhow::Result<()> {
    let mut statement = conn.prepare("PRAGMA foreign_key_check")?;
    let failures = statement
        .query_map([], |row| {
            Ok(format!(
                "{}:{}->{}",
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    anyhow::ensure!(
        failures.is_empty(),
        "FOREIGN_KEY_CHECK_FAILED: {}",
        failures.join(",")
    );
    Ok(())
}

fn verify_integrity(conn: &Connection) -> anyhow::Result<()> {
    verify_foreign_keys(conn)?;
    let result: String = conn.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
    anyhow::ensure!(
        result.eq_ignore_ascii_case("ok"),
        "INTEGRITY_CHECK_FAILED: {result}"
    );
    Ok(())
}

fn schema_version(conn: &Connection) -> anyhow::Result<i64> {
    conn.query_row("PRAGMA user_version", [], |row| row.get(0))
        .context("read PRAGMA user_version")
}

fn owned_table_names(conn: &Connection) -> anyhow::Result<BTreeSet<String>> {
    let mut statement = conn.prepare(
        "SELECT name FROM sqlite_master
         WHERE type='table' AND name NOT LIKE 'sqlite_%' AND name NOT LIKE '%_data'
           AND name NOT LIKE '%_idx' AND name NOT LIKE '%_content'
           AND name NOT LIKE '%_docsize' AND name NOT LIKE '%_config'",
    )?;
    Ok(statement
        .query_map([], |row| row.get(0))?
        .collect::<rusqlite::Result<BTreeSet<_>>>()?)
}

fn table_columns(conn: &Connection, table: &str) -> anyhow::Result<BTreeSet<String>> {
    let mut statement = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    Ok(statement
        .query_map([], |row| row.get(1))?
        .collect::<rusqlite::Result<BTreeSet<_>>>()?)
}

fn table_sql(conn: &Connection, table: &str) -> anyhow::Result<String> {
    conn.query_row(
        "SELECT sql FROM sqlite_master WHERE type='table' AND name=?1",
        [table],
        |row| row.get(0),
    )
    .with_context(|| format!("read schema for table {table}"))
}

fn has_table(conn: &Connection, table: &str) -> anyhow::Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
        [table],
        |row| row.get(0),
    )?)
}

fn has_index(conn: &Connection, index: &str) -> anyhow::Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='index' AND name=?1)",
        [index],
        |row| row.get(0),
    )?)
}

fn add_column_if_missing(
    conn: &Connection,
    table: &str,
    column: &str,
    sql: &str,
) -> anyhow::Result<()> {
    anyhow::ensure!(has_table(conn, table)?, "SCHEMA_MISSING_TABLE: {table}");
    if !table_columns(conn, table)?.contains(column) {
        conn.execute(sql, [])?;
    }
    Ok(())
}

fn readiness_version(readiness: &SchemaReadiness) -> Option<i64> {
    match readiness {
        SchemaReadiness::Ready { version } | SchemaReadiness::Drifted { version, .. } => {
            Some(*version)
        }
        SchemaReadiness::NeedsEvolution { from, .. } => Some(*from),
        SchemaReadiness::NewerUnsupported { found, .. } => Some(*found),
        SchemaReadiness::Uninitialized => Some(0),
    }
}

fn bounded_detail(detail: impl std::fmt::Display) -> String {
    detail
        .to_string()
        .replace(['\r', '\n'], " ")
        .chars()
        .take(MAX_DETAIL_CHARS)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn current() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        evolve(&conn).unwrap();
        conn
    }

    #[test]
    fn a_new_library_replays_every_transition() {
        let conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        let report = evolve(&conn).unwrap();
        assert_eq!(report.initial, SchemaReadiness::Uninitialized);
        assert_eq!(
            report.completed_transitions,
            vec![
                (0, 1),
                (1, 2),
                (2, 3),
                (3, 4),
                (4, 5),
                (5, 6),
                (6, 7),
                (7, 8),
            ]
        );
        assert_eq!(
            inspect(&conn).unwrap(),
            SchemaReadiness::Ready { version: 8 }
        );
    }

    #[test]
    fn current_version_with_missing_index_is_drifted_and_unchanged() {
        let conn = current();
        conn.execute_batch("DROP INDEX idx_resources_health_updated")
            .unwrap();
        let readiness = inspect(&conn).unwrap();
        assert!(matches!(
            readiness,
            SchemaReadiness::Drifted { version: 8, .. }
        ));
        assert_eq!(
            conn.query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            8
        );
        let error = evolve(&conn).unwrap_err();
        assert_eq!(error.kind, EvolutionFailureKind::SchemaDrift);
        assert!(!has_index(&conn, "idx_resources_health_updated").unwrap());
    }

    #[test]
    fn newer_schema_is_never_modified() {
        let conn = current();
        conn.pragma_update(None, "user_version", 99).unwrap();
        assert!(matches!(
            inspect(&conn).unwrap(),
            SchemaReadiness::NewerUnsupported {
                found: 99,
                supported: 8
            }
        ));
        assert_eq!(
            evolve(&conn).unwrap_err().kind,
            EvolutionFailureKind::NewerUnsupported
        );
        assert_eq!(
            conn.query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            99
        );
    }

    #[test]
    fn failed_transition_does_not_advance_its_version() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(V0_BOOTSTRAP_SCHEMA).unwrap();
        conn.pragma_update(None, "user_version", 1).unwrap();
        // Version 1 requires the complete resource foundation. This fixture is
        // deliberately drifted, so evolution must reject it before v1->v2.
        let error = evolve(&conn).unwrap_err();
        assert_eq!(error.kind, EvolutionFailureKind::SchemaDrift);
        assert_eq!(
            conn.query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            1
        );
    }

    #[test]
    fn every_released_schema_fixture_reaches_current_without_losing_library_data() {
        let fixtures = [
            (
                "v0.1.0",
                include_str!("../tests/fixtures/schema/v0_1_0.sql"),
                false,
            ),
            (
                "v0.1.1",
                include_str!("../tests/fixtures/schema/v0_1_1.sql"),
                false,
            ),
            (
                "v0.2.0",
                include_str!("../tests/fixtures/schema/v0_2_0.sql"),
                false,
            ),
            (
                "v0.2.1",
                include_str!("../tests/fixtures/schema/v0_2_1.sql"),
                false,
            ),
            (
                "v0.3.0",
                include_str!("../tests/fixtures/schema/v0_3_0.sql"),
                false,
            ),
            (
                "v0.4.0",
                include_str!("../tests/fixtures/schema/v0_4_0.sql"),
                true,
            ),
            (
                "v0.5.0",
                include_str!("../tests/fixtures/schema/v0_5_0.sql"),
                true,
            ),
        ];

        for (release, fixture, had_resource) in fixtures {
            let conn = Connection::open_in_memory().unwrap();
            conn.pragma_update(None, "foreign_keys", "ON").unwrap();
            conn.execute_batch(fixture)
                .unwrap_or_else(|error| panic!("{release} fixture invalid: {error}"));

            let report =
                evolve(&conn).unwrap_or_else(|error| panic!("{release} failed to evolve: {error}"));
            assert_eq!(report.completed_transitions.len(), 8, "{release}");
            assert_eq!(
                inspect(&conn).unwrap(),
                SchemaReadiness::Ready { version: 8 }
            );
            assert_eq!(
                conn.query_row("SELECT title FROM articles WHERE id=1", [], |row| {
                    row.get::<_, String>(0)
                })
                .unwrap(),
                "Preserved article",
                "{release}"
            );
            assert_eq!(
                conn.query_row(
                    "SELECT selected_text,comment,is_favorite FROM article_selections WHERE id=1",
                    [],
                    |row| Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, bool>(2)?
                    )),
                )
                .unwrap(),
                ("preserved quote".into(), "preserved thought".into(), true),
                "{release}"
            );
            let resource_count: i64 = conn
                .query_row("SELECT COUNT(*) FROM resources", [], |row| row.get(0))
                .unwrap();
            assert_eq!(resource_count, i64::from(had_resource), "{release}");
        }
    }
}
