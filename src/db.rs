//! SQLite 访问层（ADR-3）。GUI、CLI 与后台工作流共享同一库，WAL 模式扛并发。

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, Row, params};
use std::path::{Path, PathBuf};

use crate::config::Config;
use crate::local_data_maintenance::{WriterGate, WriterPermit, run_schema_migration};
use crate::model::{
    Article, ArticleSelection, Feed, NewArticle, SearchHistoryEntry, SearchHit, SearchHitKind,
    TextAnchor,
};

/// Hidden, non-network feed used to reuse the normal article reader and its
/// annotations for locally saved web pages.
pub const WEB_CLIPPINGS_FEED_URL: &str = "shiyue://web-clippings";
const WEB_CLIPPINGS_FEED_TITLE: &str = "网页收藏";
const CURRENT_SCHEMA_VERSION: i64 = 4;

#[derive(Debug, Clone)]
pub struct ArticleAiContent {
    pub summary_zh: String,
    pub translation_zh: String,
    pub model: String,
    pub updated_at: i64,
}

pub(crate) const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS feeds (
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
CREATE TABLE IF NOT EXISTS articles (
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
  read_later INTEGER NOT NULL DEFAULT 0,
  archived   INTEGER NOT NULL DEFAULT 0,
  fetched_at INTEGER NOT NULL,
  UNIQUE(feed_id, entry_id)
);
CREATE TABLE IF NOT EXISTS article_selections (
  id            INTEGER PRIMARY KEY,
  article_id    INTEGER NOT NULL REFERENCES articles(id) ON DELETE CASCADE,
  selected_text TEXT NOT NULL CHECK (length(trim(selected_text)) > 0),
  start_offset  INTEGER,
  end_offset    INTEGER,
  anchor_prefix TEXT NOT NULL DEFAULT '',
  anchor_suffix TEXT NOT NULL DEFAULT '',
  comment       TEXT,
  is_favorite   INTEGER NOT NULL DEFAULT 0 CHECK (is_favorite IN (0, 1)),
  created_at    INTEGER NOT NULL,
  updated_at    INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_article_selections_article
  ON article_selections(article_id, created_at DESC, id DESC);
CREATE INDEX IF NOT EXISTS idx_article_selections_favorite
  ON article_selections(is_favorite, created_at DESC, id DESC);
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
  curation_state     TEXT NOT NULL DEFAULT 'active' CHECK (curation_state IN ('pending_review', 'active', 'archived')),
  health             TEXT NOT NULL DEFAULT 'unknown' CHECK (health IN ('unknown', 'healthy', 'broken')),
  categories_source  TEXT NOT NULL DEFAULT 'ai' CHECK (categories_source IN ('manual', 'ai')),
  tags_source        TEXT NOT NULL DEFAULT 'ai' CHECK (tags_source IN ('manual', 'ai')),
  source_failure_count INTEGER NOT NULL DEFAULT 0,
  source             TEXT NOT NULL CHECK (source IN ('gui', 'cli_agent', 'import')),
  manual_rating      INTEGER CHECK (manual_rating IS NULL OR manual_rating BETWEEN 1 AND 5),
  latest_snapshot_id INTEGER REFERENCES resource_snapshots(id) ON DELETE SET NULL,
  last_checked_at    INTEGER,
  created_at         INTEGER NOT NULL,
  updated_at         INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_resources_status_updated ON resources(status, updated_at DESC);
CREATE INDEX IF NOT EXISTS idx_resources_curation_updated ON resources(curation_state, updated_at DESC, id DESC);
CREATE INDEX IF NOT EXISTS idx_resources_health_updated ON resources(health, updated_at DESC, id DESC);
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
  id            INTEGER PRIMARY KEY,
  resource_id   INTEGER NOT NULL REFERENCES resources(id) ON DELETE CASCADE,
  snapshot_id   INTEGER REFERENCES resource_snapshots(id) ON DELETE SET NULL,
  provider      TEXT NOT NULL,
  model         TEXT NOT NULL,
  prompt_version TEXT NOT NULL,
  schema_version TEXT NOT NULL,
  started_at    INTEGER NOT NULL,
  finished_at   INTEGER,
  status        TEXT NOT NULL CHECK (status IN ('pending', 'running', 'succeeded', 'failed')),
  error_code    TEXT,
  error_message TEXT,
  attempt_id    INTEGER REFERENCES knowledge_task_attempts(id) ON DELETE SET NULL
);
CREATE TABLE IF NOT EXISTS resource_usage_events (
  id          INTEGER PRIMARY KEY,
  resource_id INTEGER NOT NULL REFERENCES resources(id) ON DELETE CASCADE,
  event       TEXT NOT NULL CHECK (event IN ('returned', 'confirmed_used')),
  occurred_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_resource_usage_events ON resource_usage_events(resource_id, occurred_at DESC);
CREATE TABLE IF NOT EXISTS knowledge_tasks (
  id            INTEGER PRIMARY KEY,
  kind          TEXT NOT NULL CHECK (kind IN ('resource_completion', 'article_summary')),
  target_id     INTEGER NOT NULL,
  status        TEXT NOT NULL CHECK (status IN ('queued', 'running', 'succeeded', 'failed', 'interrupted')),
  current_stage TEXT,
  next_run_at   INTEGER NOT NULL DEFAULT 0,
  created_at    INTEGER NOT NULL,
  updated_at    INTEGER NOT NULL,
  change_seq    INTEGER NOT NULL DEFAULT 0
);
CREATE UNIQUE INDEX IF NOT EXISTS idx_knowledge_tasks_active_target
  ON knowledge_tasks(kind, target_id)
  WHERE status IN ('queued', 'running');
CREATE INDEX IF NOT EXISTS idx_knowledge_tasks_target_history
  ON knowledge_tasks(kind, target_id, created_at DESC, id DESC);
CREATE TABLE IF NOT EXISTS knowledge_task_attempts (
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
  claim_generation INTEGER,
  created_at       INTEGER NOT NULL,
  UNIQUE(task_id, attempt_number)
);
CREATE INDEX IF NOT EXISTS idx_knowledge_attempts_task
  ON knowledge_task_attempts(task_id, attempt_number DESC);
CREATE TABLE IF NOT EXISTS knowledge_executor_lease (
  singleton_id INTEGER PRIMARY KEY CHECK (singleton_id = 1),
  owner_id     TEXT,
  generation   INTEGER NOT NULL DEFAULT 0,
  heartbeat_at INTEGER NOT NULL DEFAULT 0
);
INSERT OR IGNORE INTO knowledge_executor_lease(singleton_id) VALUES(1);
CREATE TABLE IF NOT EXISTS knowledge_change_clock (
  singleton_id INTEGER PRIMARY KEY CHECK (singleton_id = 1),
  sequence     INTEGER NOT NULL DEFAULT 0
);
INSERT OR IGNORE INTO knowledge_change_clock(singleton_id) VALUES(1);
CREATE VIRTUAL TABLE IF NOT EXISTS resource_fts USING fts5(
  source_id UNINDEXED,
  body,
  tokenize='trigram'
);
CREATE VIRTUAL TABLE IF NOT EXISTS library_fts USING fts5(
  kind UNINDEXED,
  source_id UNINDEXED,
  article_id UNINDEXED,
  body,
  tokenize='trigram'
);
CREATE TRIGGER IF NOT EXISTS articles_fts_insert AFTER INSERT ON articles BEGIN
  INSERT INTO library_fts(kind, source_id, article_id, body)
  VALUES (
    CASE WHEN (SELECT url FROM feeds WHERE id = new.feed_id) = 'shiyue://web-clippings' THEN 1 ELSE 0 END,
    new.id,
    new.id,
    trim(COALESCE(new.title, '') || char(10) || COALESCE(new.author, '') || char(10) ||
         COALESCE(new.content, '') || char(10) || COALESCE(new.url, ''))
  );
END;
CREATE TRIGGER IF NOT EXISTS articles_fts_update AFTER UPDATE OF title, author, content, url, feed_id ON articles BEGIN
  DELETE FROM library_fts WHERE kind IN (0, 1) AND source_id = old.id;
  INSERT INTO library_fts(kind, source_id, article_id, body)
  VALUES (
    CASE WHEN (SELECT url FROM feeds WHERE id = new.feed_id) = 'shiyue://web-clippings' THEN 1 ELSE 0 END,
    new.id,
    new.id,
    trim(COALESCE(new.title, '') || char(10) || COALESCE(new.author, '') || char(10) ||
         COALESCE(new.content, '') || char(10) || COALESCE(new.url, '') || char(10) ||
         COALESCE((SELECT group_concat(t.name, ' ') FROM article_tags at
                   JOIN tags t ON t.id = at.tag_id WHERE at.article_id = new.id), ''))
  );
END;
CREATE TRIGGER IF NOT EXISTS articles_fts_delete AFTER DELETE ON articles BEGIN
  DELETE FROM library_fts WHERE article_id = old.id;
END;
CREATE TRIGGER IF NOT EXISTS selections_fts_insert AFTER INSERT ON article_selections BEGIN
  INSERT INTO library_fts(kind, source_id, article_id, body)
    SELECT 2, new.id, new.article_id, new.selected_text WHERE new.is_favorite = 1;
  INSERT INTO library_fts(kind, source_id, article_id, body)
    SELECT 3, new.id, new.article_id, new.comment
    WHERE new.comment IS NOT NULL AND length(trim(new.comment)) > 0;
END;
CREATE TRIGGER IF NOT EXISTS selections_fts_update AFTER UPDATE ON article_selections BEGIN
  DELETE FROM library_fts WHERE kind IN (2, 3) AND source_id = old.id;
  INSERT INTO library_fts(kind, source_id, article_id, body)
    SELECT 2, new.id, new.article_id, new.selected_text WHERE new.is_favorite = 1;
  INSERT INTO library_fts(kind, source_id, article_id, body)
    SELECT 3, new.id, new.article_id, new.comment
    WHERE new.comment IS NOT NULL AND length(trim(new.comment)) > 0;
END;
CREATE TRIGGER IF NOT EXISTS selections_fts_delete AFTER DELETE ON article_selections BEGIN
  DELETE FROM library_fts WHERE kind IN (2, 3) AND source_id = old.id;
END;
CREATE TRIGGER IF NOT EXISTS article_tags_fts_insert AFTER INSERT ON article_tags BEGIN
  DELETE FROM library_fts WHERE kind IN (0, 1) AND source_id = new.article_id;
  INSERT INTO library_fts(kind, source_id, article_id, body)
    SELECT CASE WHEN f.url = 'shiyue://web-clippings' THEN 1 ELSE 0 END, a.id, a.id,
           trim(COALESCE(a.title, '') || char(10) || COALESCE(a.author, '') || char(10) ||
                COALESCE(a.content, '') || char(10) || COALESCE(a.url, '') || char(10) ||
                COALESCE((SELECT group_concat(t.name, ' ') FROM article_tags at
                          JOIN tags t ON t.id = at.tag_id WHERE at.article_id = a.id), ''))
    FROM articles a JOIN feeds f ON f.id = a.feed_id WHERE a.id = new.article_id;
END;
CREATE TRIGGER IF NOT EXISTS article_tags_fts_delete AFTER DELETE ON article_tags BEGIN
  DELETE FROM library_fts WHERE kind IN (0, 1) AND source_id = old.article_id;
  INSERT INTO library_fts(kind, source_id, article_id, body)
    SELECT CASE WHEN f.url = 'shiyue://web-clippings' THEN 1 ELSE 0 END, a.id, a.id,
           trim(COALESCE(a.title, '') || char(10) || COALESCE(a.author, '') || char(10) ||
                COALESCE(a.content, '') || char(10) || COALESCE(a.url, '') || char(10) ||
                COALESCE((SELECT group_concat(t.name, ' ') FROM article_tags at
                          JOIN tags t ON t.id = at.tag_id WHERE at.article_id = a.id), ''))
    FROM articles a JOIN feeds f ON f.id = a.feed_id WHERE a.id = old.article_id;
END;
"#;

const FEED_COLS: &str =
    "id, url, title, interval_secs, last_fetch, next_fetch, last_error, fail_count, disabled";
const ARTICLE_COLS: &str = "id, feed_id, entry_id, url, title, author, published, content, \
                            is_read, starred, read_later, archived, fetched_at";
const SELECTION_COLS: &str = "id, article_id, selected_text, start_offset, end_offset, \
                              anchor_prefix, anchor_suffix, comment, is_favorite, created_at, updated_at";

pub struct Db {
    pub(crate) conn: Connection,
    pub(crate) path: Option<PathBuf>,
    pub(crate) _writer_gate: Option<WriterGate>,
    // A normal database connection holds a shared writer lease for its whole
    // lifetime. Exclusive maintenance therefore cannot begin until every
    // cooperating connection has actually closed.
    pub(crate) _lifetime_permit: Option<WriterPermit>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DatabaseCheck {
    pub ok: bool,
    pub details: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompactionReport {
    pub before_bytes: u64,
    pub after_bytes: u64,
}

fn map_feed(row: &Row) -> rusqlite::Result<Feed> {
    Ok(Feed {
        id: row.get(0)?,
        url: row.get(1)?,
        title: row.get(2)?,
        interval_secs: row.get(3)?,
        last_fetch: row.get(4)?,
        next_fetch: row.get(5)?,
        last_error: row.get(6)?,
        fail_count: row.get(7)?,
        disabled: row.get(8)?,
    })
}

fn map_article(row: &Row) -> rusqlite::Result<Article> {
    Ok(Article {
        id: row.get(0)?,
        feed_id: row.get(1)?,
        entry_id: row.get(2)?,
        url: row.get(3)?,
        title: row.get(4)?,
        author: row.get(5)?,
        published: row.get(6)?,
        content: row.get(7)?,
        is_read: row.get(8)?,
        starred: row.get(9)?,
        read_later: row.get(10)?,
        archived: row.get(11)?,
        fetched_at: row.get(12)?,
    })
}

fn has_column(conn: &Connection, table: &str, expected: &str) -> Result<bool> {
    let has_column = {
        let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
        let columns = stmt.query_map([], |row| row.get::<_, String>(1))?;
        columns
            .collect::<rusqlite::Result<Vec<_>>>()?
            .iter()
            .any(|name| name == expected)
    };
    Ok(has_column)
}

fn has_table(conn: &Connection, table: &str) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
        [table],
        |row| row.get(0),
    )?)
}

fn migrate(conn: &Connection) -> Result<()> {
    let mut version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version > CURRENT_SCHEMA_VERSION {
        bail!("数据库版本 {version} 高于当前程序支持的版本 {CURRENT_SCHEMA_VERSION}，请升级拾阅");
    }
    while version < CURRENT_SCHEMA_VERSION {
        let tx = conn.unchecked_transaction()?;
        match version {
            0 => migrate_v0_to_v1(&tx)?,
            1 => migrate_v1_to_v2(&tx)?,
            2 => migrate_v2_to_v3(&tx)?,
            3 => migrate_v3_to_v4(&tx)?,
            _ => bail!("缺少从数据库版本 {version} 开始的迁移"),
        }
        version += 1;
        tx.pragma_update(None, "user_version", version)?;
        tx.commit()?;
    }
    Ok(())
}

fn migrate_v3_to_v4(conn: &Connection) -> Result<()> {
    if !has_table(conn, "resources")? {
        return Ok(());
    }
    if !has_column(conn, "resources", "curation_state")? {
        conn.execute(
            "ALTER TABLE resources ADD COLUMN curation_state TEXT NOT NULL DEFAULT 'active' CHECK (curation_state IN ('pending_review', 'active', 'archived'))",
            [],
        )?;
    }
    if !has_column(conn, "resources", "health")? {
        conn.execute(
            "ALTER TABLE resources ADD COLUMN health TEXT NOT NULL DEFAULT 'unknown' CHECK (health IN ('unknown', 'healthy', 'broken'))",
            [],
        )?;
    }
    if !has_column(conn, "resources", "categories_source")? {
        conn.execute(
            "ALTER TABLE resources ADD COLUMN categories_source TEXT NOT NULL DEFAULT 'ai' CHECK (categories_source IN ('manual', 'ai'))",
            [],
        )?;
    }
    if !has_column(conn, "resources", "tags_source")? {
        conn.execute(
            "ALTER TABLE resources ADD COLUMN tags_source TEXT NOT NULL DEFAULT 'ai' CHECK (tags_source IN ('manual', 'ai'))",
            [],
        )?;
    }
    if !has_column(conn, "resources", "source_failure_count")? {
        conn.execute(
            "ALTER TABLE resources ADD COLUMN source_failure_count INTEGER NOT NULL DEFAULT 0",
            [],
        )?;
    }
    if !has_column(conn, "resources", "status")? || !has_column(conn, "resources", "updated_at")? {
        return Ok(());
    }
    conn.execute_batch(
        "UPDATE resources
         SET curation_state=CASE status
               WHEN 'pending_review' THEN 'pending_review'
               WHEN 'archived' THEN 'archived'
               ELSE 'active'
             END,
             health=CASE
               WHEN status='broken' THEN 'broken'
               WHEN EXISTS (
                 SELECT 1 FROM resource_snapshots s
                 WHERE s.resource_id=resources.id AND s.content_hash IS NOT NULL
               ) THEN 'healthy'
               ELSE 'unknown'
             END,
             categories_source=CASE
               WHEN EXISTS (
                 SELECT 1 FROM resource_enrichment_runs e
                 WHERE e.resource_id=resources.id AND e.status='succeeded'
               ) THEN 'ai'
               ELSE 'manual'
             END,
             tags_source=CASE
               WHEN EXISTS (
                 SELECT 1 FROM resource_tags t
                 WHERE t.resource_id=resources.id AND t.source='manual'
               ) THEN 'manual'
               ELSE 'ai'
             END;
         CREATE INDEX IF NOT EXISTS idx_resources_curation_updated
           ON resources(curation_state, updated_at DESC, id DESC);
         CREATE INDEX IF NOT EXISTS idx_resources_health_updated
           ON resources(health, updated_at DESC, id DESC);",
    )?;
    Ok(())
}

fn migrate_v1_to_v2(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS knowledge_tasks (
           id INTEGER PRIMARY KEY,
           kind TEXT NOT NULL CHECK (kind IN ('resource_completion', 'article_summary')),
           target_id INTEGER NOT NULL,
           status TEXT NOT NULL CHECK (status IN ('queued', 'running', 'succeeded', 'failed', 'interrupted')),
           current_stage TEXT,
           next_run_at INTEGER NOT NULL DEFAULT 0,
           created_at INTEGER NOT NULL,
           updated_at INTEGER NOT NULL
         );
         CREATE UNIQUE INDEX IF NOT EXISTS idx_knowledge_tasks_active_target
           ON knowledge_tasks(kind, target_id) WHERE status IN ('queued', 'running');
         CREATE INDEX IF NOT EXISTS idx_knowledge_tasks_target_history
           ON knowledge_tasks(kind, target_id, created_at DESC, id DESC);
         CREATE TABLE IF NOT EXISTS knowledge_task_attempts (
           id INTEGER PRIMARY KEY,
           task_id INTEGER NOT NULL REFERENCES knowledge_tasks(id) ON DELETE CASCADE,
           attempt_number INTEGER NOT NULL,
           status TEXT NOT NULL CHECK (status IN ('queued', 'running', 'succeeded', 'failed', 'interrupted')),
           current_stage TEXT,
           automatic_retry INTEGER NOT NULL DEFAULT 0 CHECK (automatic_retry IN (0, 1)),
           started_at INTEGER,
           finished_at INTEGER,
           error_kind TEXT CHECK (error_kind IS NULL OR error_kind IN ('transient', 'authentication', 'security', 'input', 'provider_output', 'storage', 'interrupted')),
           user_message TEXT,
           technical_detail TEXT,
           created_at INTEGER NOT NULL,
           UNIQUE(task_id, attempt_number)
         );
         CREATE INDEX IF NOT EXISTS idx_knowledge_attempts_task
           ON knowledge_task_attempts(task_id, attempt_number DESC);",
    )?;
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

fn migrate_v2_to_v3(conn: &Connection) -> Result<()> {
    if !has_column(conn, "knowledge_tasks", "change_seq")? {
        conn.execute(
            "ALTER TABLE knowledge_tasks ADD COLUMN change_seq INTEGER NOT NULL DEFAULT 0",
            [],
        )?;
    }
    if !has_column(conn, "knowledge_task_attempts", "claim_generation")? {
        conn.execute(
            "ALTER TABLE knowledge_task_attempts ADD COLUMN claim_generation INTEGER",
            [],
        )?;
    }
    if !has_column(conn, "resource_enrichment_runs", "attempt_id")? {
        conn.execute(
            "ALTER TABLE resource_enrichment_runs ADD COLUMN attempt_id INTEGER REFERENCES knowledge_task_attempts(id) ON DELETE SET NULL",
            [],
        )?;
    }
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS knowledge_executor_lease (
           singleton_id INTEGER PRIMARY KEY CHECK (singleton_id = 1),
           owner_id TEXT,
           generation INTEGER NOT NULL DEFAULT 0,
           heartbeat_at INTEGER NOT NULL DEFAULT 0
         );
         INSERT OR IGNORE INTO knowledge_executor_lease(singleton_id) VALUES(1);
         CREATE TABLE IF NOT EXISTS knowledge_change_clock (
           singleton_id INTEGER PRIMARY KEY CHECK (singleton_id = 1),
           sequence INTEGER NOT NULL DEFAULT 0
         );
         INSERT OR IGNORE INTO knowledge_change_clock(singleton_id) VALUES(1);",
    )?;

    let now = chrono::Utc::now().timestamp();
    conn.execute(
        "UPDATE knowledge_task_attempts
         SET status='interrupted',finished_at=COALESCE(finished_at,?1),
             error_kind='interrupted',
             user_message='程序升级后需要重新执行',
             technical_detail='WORKFLOW_UPGRADE_INTERRUPTED: v2 running attempt had no fencing generation'
         WHERE status='running'",
        [now],
    )?;
    conn.execute(
        "UPDATE knowledge_tasks
         SET status='interrupted',current_stage=NULL,updated_at=?1
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
         SET sequence=COALESCE((SELECT MAX(id) FROM knowledge_tasks),0)
         WHERE singleton_id=1",
        [],
    )?;
    conn.execute(
        "UPDATE knowledge_tasks
         SET change_seq=id
         WHERE change_seq=0",
        [],
    )?;
    Ok(())
}

fn migrate_v0_to_v1(conn: &Connection) -> Result<()> {
    if !has_column(conn, "resources", "purpose_source")? {
        conn.execute("ALTER TABLE resources ADD COLUMN purpose_source TEXT CHECK (purpose_source IS NULL OR purpose_source IN ('manual','ai'))", [])?;
    }
    if !has_column(conn, "resources", "use_when_source")? {
        conn.execute("ALTER TABLE resources ADD COLUMN use_when_source TEXT CHECK (use_when_source IS NULL OR use_when_source IN ('manual','ai'))", [])?;
    }
    if !has_column(conn, "articles", "archived")? {
        conn.execute(
            "ALTER TABLE articles ADD COLUMN archived INTEGER NOT NULL DEFAULT 0",
            [],
        )?;
    }
    if !has_column(conn, "articles", "read_later")? {
        conn.execute(
            "ALTER TABLE articles ADD COLUMN read_later INTEGER NOT NULL DEFAULT 0",
            [],
        )?;
    }
    if !has_column(conn, "article_selections", "anchor_prefix")? {
        conn.execute(
            "ALTER TABLE article_selections ADD COLUMN anchor_prefix TEXT NOT NULL DEFAULT ''",
            [],
        )?;
    }
    if !has_column(conn, "article_selections", "anchor_suffix")? {
        conn.execute(
            "ALTER TABLE article_selections ADD COLUMN anchor_suffix TEXT NOT NULL DEFAULT ''",
            [],
        )?;
    }
    let indexed_rows: i64 =
        conn.query_row("SELECT COUNT(*) FROM library_fts", [], |row| row.get(0))?;
    if indexed_rows == 0 {
        conn.execute(
            "INSERT INTO library_fts(kind, source_id, article_id, body)
         SELECT CASE WHEN f.url = ?1 THEN 1 ELSE 0 END, a.id, a.id,
                trim(COALESCE(a.title, '') || char(10) || COALESCE(a.author, '') || char(10) ||
                     COALESCE(a.content, '') || char(10) || COALESCE(a.url, '') || char(10) ||
                     COALESCE((SELECT group_concat(t.name, ' ') FROM article_tags at
                               JOIN tags t ON t.id = at.tag_id WHERE at.article_id = a.id), ''))
         FROM articles a JOIN feeds f ON f.id = a.feed_id",
            params![WEB_CLIPPINGS_FEED_URL],
        )?;
        conn.execute(
            "INSERT INTO library_fts(kind, source_id, article_id, body)
         SELECT 2, id, article_id, selected_text FROM article_selections WHERE is_favorite = 1",
            [],
        )?;
        conn.execute(
            "INSERT INTO library_fts(kind, source_id, article_id, body)
         SELECT 3, id, article_id, comment FROM article_selections
         WHERE comment IS NOT NULL AND length(trim(comment)) > 0",
            [],
        )?;
    }
    let resource_indexed: i64 =
        conn.query_row("SELECT COUNT(*) FROM resource_fts", [], |r| r.get(0))?;
    if resource_indexed == 0 {
        conn.execute("INSERT INTO resource_fts(source_id,body) SELECT r.id,trim(COALESCE(r.title,'')||char(10)||r.url||char(10)||COALESCE(r.purpose_zh,'')||char(10)||COALESCE(r.use_when_zh,'')||char(10)||r.capabilities||char(10)||r.limitations||char(10)||r.languages||char(10)||COALESCE(r.private_note,'')||char(10)||COALESCE((SELECT group_concat(category,' ') FROM resource_categories c WHERE c.resource_id=r.id),'')||char(10)||COALESCE((SELECT group_concat(name,' ') FROM resource_tags t WHERE t.resource_id=r.id),'')||char(10)||COALESCE((SELECT cleaned_content FROM resource_snapshots s WHERE s.id=r.latest_snapshot_id),'')) FROM resources r", [])?;
    }
    Ok(())
}

fn map_selection(row: &Row) -> rusqlite::Result<ArticleSelection> {
    Ok(ArticleSelection {
        id: row.get(0)?,
        article_id: row.get(1)?,
        selected_text: row.get(2)?,
        start_offset: row.get(3)?,
        end_offset: row.get(4)?,
        anchor_prefix: row.get(5)?,
        anchor_suffix: row.get(6)?,
        comment: row.get(7)?,
        is_favorite: row.get(8)?,
        created_at: row.get(9)?,
        updated_at: row.get(10)?,
    })
}

fn map_search_hit(row: &Row) -> rusqlite::Result<SearchHit> {
    let kind = match row.get::<_, i64>(0)? {
        0 => SearchHitKind::Article,
        1 => SearchHitKind::WebClipping,
        2 => SearchHitKind::Excerpt,
        3 => SearchHitKind::Thought,
        value => return Err(rusqlite::Error::IntegralValueOutOfRange(0, value)),
    };
    Ok(SearchHit {
        kind,
        selection_id: matches!(kind, SearchHitKind::Excerpt | SearchHitKind::Thought)
            .then(|| row.get(1))
            .transpose()?,
        article_id: row.get(2)?,
        feed_id: row.get(3)?,
        article_title: row.get(4)?,
        snippet: row.get(5)?,
        timestamp: row.get(6)?,
        archived: row.get(7)?,
    })
}

impl Db {
    pub fn open(path: &std::path::Path) -> Result<Self> {
        loop {
            let writer_gate = WriterGate::open(path)?;
            let permit = writer_gate.permit()?;
            let conn = Connection::open(path)
                .with_context(|| format!("打开数据库失败: {}", path.display()))?;
            conn.pragma_update(None, "journal_mode", "WAL")?;
            conn.pragma_update(None, "foreign_keys", "ON")?;
            let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
            if version < CURRENT_SCHEMA_VERSION {
                drop(conn);
                drop(permit);
                run_schema_migration(path, || {
                    let conn = Connection::open(path)?;
                    conn.pragma_update(None, "journal_mode", "WAL")?;
                    conn.pragma_update(None, "foreign_keys", "ON")?;
                    conn.execute_batch(SCHEMA)?;
                    migrate(&conn)
                })?;
                continue;
            }
            migrate(&conn)?;
            permit.validate()?;
            return Ok(Self {
                conn,
                path: Some(path.to_path_buf()),
                _writer_gate: Some(writer_gate),
                _lifetime_permit: Some(permit),
            });
        }
    }

    /// Open while the caller holds the exclusive maintenance writer lock.
    pub(crate) fn open_for_maintenance(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)
            .with_context(|| format!("open database for maintenance: {}", path.display()))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.execute_batch(SCHEMA)?;
        migrate(&conn)?;
        Ok(Self {
            conn,
            path: Some(path.to_path_buf()),
            _writer_gate: None,
            _lifetime_permit: None,
        })
    }

    #[allow(dead_code)]
    pub(crate) fn write_permit(&self) -> Result<Option<WriterPermit>> {
        self._writer_gate
            .as_ref()
            .map(WriterGate::permit)
            .transpose()
    }

    // ---- 源的增删查改 ----

    /// 添加源（幂等：已存在则返回既有 id）。
    #[cfg(test)]
    pub(crate) fn add_feed(&self, url: &str, now: i64) -> Result<i64> {
        Ok(self.add_feed_with_disposition(url, now)?.0)
    }

    pub(crate) fn add_feed_with_disposition(&self, url: &str, now: i64) -> Result<(i64, bool)> {
        self.conn.execute(
            "INSERT OR IGNORE INTO feeds (url, next_fetch) VALUES (?1, ?2)",
            params![url, now],
        )?;
        let created = self.conn.changes() > 0;
        let id = self
            .conn
            .query_row("SELECT id FROM feeds WHERE url = ?1", params![url], |r| {
                r.get(0)
            })?;
        Ok((id, created))
    }

    /// 按 id 或 url 删除，返回删除行数。
    pub(crate) fn remove_feed(&self, target: &str) -> Result<usize> {
        let n = if let Ok(id) = target.parse::<i64>() {
            self.conn.execute(
                "DELETE FROM feeds WHERE id = ?1 AND url <> ?2",
                params![id, WEB_CLIPPINGS_FEED_URL],
            )?
        } else {
            self.conn.execute(
                "DELETE FROM feeds WHERE url = ?1 AND url <> ?2",
                params![target, WEB_CLIPPINGS_FEED_URL],
            )?
        };
        Ok(n)
    }

    pub fn get_feed(&self, id: i64) -> Result<Feed> {
        let sql = format!("SELECT {FEED_COLS} FROM feeds WHERE id = ?1");
        Ok(self.conn.query_row(&sql, params![id], map_feed)?)
    }

    pub(crate) fn find_feed(&self, id: i64) -> Result<Option<Feed>> {
        let sql = format!("SELECT {FEED_COLS} FROM feeds WHERE id = ?1");
        Ok(self
            .conn
            .query_row(&sql, params![id], map_feed)
            .optional()?)
    }

    pub(crate) fn find_feed_by_url(&self, url: &str) -> Result<Option<Feed>> {
        let sql = format!("SELECT {FEED_COLS} FROM feeds WHERE url = ?1");
        Ok(self
            .conn
            .query_row(&sql, params![url], map_feed)
            .optional()?)
    }

    fn query_feeds(&self, where_clause: &str, args: &[&dyn rusqlite::ToSql]) -> Result<Vec<Feed>> {
        let sql = format!("SELECT {FEED_COLS} FROM feeds {where_clause}");
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(args, map_feed)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// 到期且未禁用的源（RSS Refresh Workflow 自动调度用）。
    pub fn due_feeds(&self, now: i64) -> Result<Vec<Feed>> {
        self.query_feeds(
            "WHERE disabled = 0 AND url <> ?2 AND next_fetch <= ?1 ORDER BY id",
            params![now, WEB_CLIPPINGS_FEED_URL],
        )
    }

    /// 所有未禁用的源（update 用）。
    pub fn enabled_feeds(&self) -> Result<Vec<Feed>> {
        self.query_feeds(
            "WHERE disabled = 0 AND url <> ?1 ORDER BY id",
            params![WEB_CLIPPINGS_FEED_URL],
        )
    }

    /// 最近一个到期时间（RSS Refresh Workflow 决定 sleep 多久）。
    pub fn earliest_next_fetch(&self) -> Result<Option<i64>> {
        let v: Option<i64> = self.conn.query_row(
            "SELECT MIN(next_fetch) FROM feeds WHERE disabled = 0 AND url <> ?1",
            params![WEB_CLIPPINGS_FEED_URL],
            |r| r.get(0),
        )?;
        Ok(v)
    }

    /// 源列表 + 未读数（list / TUI 用）。
    pub fn feeds_with_unread(&self) -> Result<Vec<(Feed, i64)>> {
        let sql = format!(
            "SELECT {FEED_COLS}, \
             (SELECT COUNT(*) FROM articles a \
              WHERE a.feed_id = feeds.id AND a.is_read = 0 AND a.archived = 0) \
             FROM feeds WHERE feeds.url <> ?1 ORDER BY id"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(params![WEB_CLIPPINGS_FEED_URL], |row| {
            Ok((map_feed(row)?, row.get::<_, i64>(9)?))
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    // ---- Locally saved web pages ----

    /// Creates the hidden storage feed if necessary and returns its stable id.
    ///
    /// The row is always forced back to `disabled = 1`. Network scheduling
    /// queries also exclude it by URL as a second line of defence.
    pub fn ensure_web_clippings_feed(&self, now: i64) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO feeds (url, title, next_fetch, disabled) \
             VALUES (?1, ?2, ?3, 1) \
             ON CONFLICT(url) DO UPDATE SET \
               title = excluded.title, disabled = 1, last_error = NULL, fail_count = 0",
            params![WEB_CLIPPINGS_FEED_URL, WEB_CLIPPINGS_FEED_TITLE, now],
        )?;
        Ok(self.conn.query_row(
            "SELECT id FROM feeds WHERE url = ?1",
            params![WEB_CLIPPINGS_FEED_URL],
            |row| row.get(0),
        )?)
    }

    /// Saves an immutable local HTML snapshot and returns its article id.
    ///
    /// Every save receives a fresh random entry id, including repeated saves
    /// of the same source URL. The original URL is retained as article
    /// metadata, while the captured body is never overwritten; annotations
    /// can therefore keep referring to the exact snapshot they were made on.
    pub fn save_web_clipping(
        &self,
        source_url: Option<&str>,
        title: Option<&str>,
        html: &str,
        now: i64,
    ) -> Result<i64> {
        if html.trim().is_empty() {
            bail!("网页内容不能为空");
        }

        let source_url = source_url.map(str::trim).filter(|value| !value.is_empty());
        let title = title.map(str::trim).filter(|value| !value.is_empty());
        let feed_id = self.ensure_web_clippings_feed(now)?;

        self.conn.execute(
            "INSERT INTO articles \
             (feed_id, entry_id, url, title, author, published, content, \
              is_read, starred, archived, fetched_at) \
             VALUES (?1, 'clip:' || lower(hex(randomblob(16))), ?2, ?3, ?4, ?5, ?6, \
                     1, 1, 0, ?5)",
            params![
                feed_id,
                source_url,
                title,
                WEB_CLIPPINGS_FEED_TITLE,
                now,
                html
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn web_clippings(&self) -> Result<Vec<Article>> {
        let sql = format!(
            "SELECT {ARTICLE_COLS} FROM articles \
             WHERE feed_id = (SELECT id FROM feeds WHERE url = ?1) \
             ORDER BY COALESCE(published, fetched_at) DESC, id DESC"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(params![WEB_CLIPPINGS_FEED_URL], map_article)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn is_web_clipping(&self, article_id: i64) -> Result<bool> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM articles a \
             JOIN feeds f ON f.id = a.feed_id WHERE a.id = ?1 AND f.url = ?2",
            params![article_id, WEB_CLIPPINGS_FEED_URL],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    /// Permanently deletes only articles belonging to the hidden clipping
    /// feed. Passing a normal RSS article id is intentionally a no-op.
    pub fn delete_web_clipping(&self, article_id: i64) -> Result<usize> {
        Ok(self.conn.execute(
            "DELETE FROM articles \
             WHERE id = ?1 AND feed_id = \
               (SELECT id FROM feeds WHERE url = ?2)",
            params![article_id, WEB_CLIPPINGS_FEED_URL],
        )?)
    }

    pub(crate) fn set_subscription_interval(&self, id: i64, secs: i64, now: i64) -> Result<usize> {
        Ok(self.conn.execute(
            "UPDATE feeds
             SET interval_secs = ?2,
                 next_fetch = COALESCE(last_fetch, ?3) + ?2
             WHERE id = ?1",
            params![id, secs, now],
        )?)
    }

    pub(crate) fn request_subscription_refresh(&self, id: i64, now: i64) -> Result<usize> {
        Ok(self.conn.execute(
            "UPDATE feeds SET next_fetch = ?2 WHERE id = ?1",
            params![id, now],
        )?)
    }

    pub(crate) fn set_disabled(&self, id: i64, disabled: bool, now: i64) -> Result<usize> {
        // 启用时清空失败状态并让它尽快重抓。
        Ok(if disabled {
            self.conn
                .execute("UPDATE feeds SET disabled = 1 WHERE id = ?1", params![id])?
        } else {
            self.conn.execute(
                "UPDATE feeds SET disabled = 0, fail_count = 0, last_error = NULL, next_fetch = ?2 WHERE id = ?1",
                params![id, now],
            )?
        })
    }

    // ---- 抓取结果落库（ADR-8 去重 / ADR-11 退避禁用）----

    /// 抓取成功：插入新文章（INSERT OR IGNORE 去重），刷新源状态，返回新增条数。
    pub fn record_success(
        &self,
        feed: &Feed,
        now: i64,
        cfg: &Config,
        title: Option<String>,
        articles: &[NewArticle],
    ) -> Result<usize> {
        let mut new = 0usize;
        for a in articles {
            let inserted = self.conn.execute(
                "INSERT OR IGNORE INTO articles \
                 (feed_id, entry_id, url, title, author, published, content, fetched_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    feed.id,
                    a.entry_id,
                    a.url,
                    a.title,
                    a.author,
                    a.published,
                    a.content,
                    now
                ],
            )?;
            new += inserted;
            // Refresh mutable feed data on later fetches. Besides fixing corrected titles,
            // this lets improved parsers recover lazy-loaded/MediaRSS images for old rows.
            if inserted == 0 {
                self.conn.execute(
                    "UPDATE articles SET \
                     url = COALESCE(?3, url), title = COALESCE(?4, title), \
                     author = COALESCE(?5, author), published = COALESCE(?6, published), \
                     content = CASE WHEN ?7 IS NULL OR ?7 = '' THEN content ELSE ?7 END \
                     WHERE feed_id = ?1 AND entry_id = ?2",
                    params![
                        feed.id,
                        a.entry_id,
                        a.url,
                        a.title,
                        a.author,
                        a.published,
                        a.content
                    ],
                )?;
            }
        }
        let interval = feed.interval_secs.unwrap_or(cfg.default_interval_secs);
        self.conn.execute(
            "UPDATE feeds SET title = COALESCE(title, ?2), last_fetch = ?3, \
             next_fetch = ?4, fail_count = 0, last_error = NULL WHERE id = ?1",
            params![feed.id, title, now, now + interval],
        )?;
        Ok(new)
    }

    /// 抓取失败：记录错误、指数退避、超阈值自动禁用。
    pub fn record_failure(&self, feed: &Feed, now: i64, cfg: &Config, err: &str) -> Result<()> {
        let fc = feed.fail_count + 1;
        let mult = 2i64.saturating_pow(fc.clamp(0, 16) as u32);
        let backoff = cfg
            .backoff_base_secs
            .saturating_mul(mult)
            .min(cfg.backoff_cap_secs);
        let disabled = (fc >= cfg.disable_after_failures) as i64;
        self.conn.execute(
            "UPDATE feeds SET fail_count = ?2, last_error = ?3, next_fetch = ?4, disabled = ?5 WHERE id = ?1",
            params![feed.id, fc, err, now + backoff, disabled],
        )?;
        Ok(())
    }

    // ---- 文章（TUI 用）----

    pub fn get_article(&self, article_id: i64) -> Result<Article> {
        let sql = format!("SELECT {ARTICLE_COLS} FROM articles WHERE id = ?1");
        Ok(self
            .conn
            .query_row(&sql, params![article_id], map_article)?)
    }

    pub fn article_ai(&self, article_id: i64) -> Result<Option<ArticleAiContent>> {
        self.conn
            .query_row(
                "SELECT summary_zh, translation_zh, model, updated_at FROM article_ai WHERE article_id=?1",
                [article_id],
                |row| {
                    Ok(ArticleAiContent {
                        summary_zh: row.get(0)?,
                        translation_zh: row.get(1)?,
                        model: row.get(2)?,
                        updated_at: row.get(3)?,
                    })
                },
            )
            .optional()
            .map_err(Into::into)
    }

    /// 在文章、网页快照、摘录与想法中进行统一全文搜索。
    ///
    /// 三个字符以上的查询使用 FTS5 trigram 分词与 BM25 相关性排序；
    /// 一两个字符的查询回退到精确子串匹配，避免中文短词被 trigram 丢弃。
    /// 每次有效搜索都会去重写入搜索历史。
    pub fn search_library(&self, query: &str, limit: usize) -> Result<Vec<SearchHit>> {
        let query = query.trim();
        if query.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let use_fts = query
            .split_whitespace()
            .all(|term| term.chars().count() >= 3);
        let hits = if use_fts {
            let match_query = query
                .split_whitespace()
                .map(|term| format!("\"{}\"", term.replace('"', "\"\"")))
                .collect::<Vec<_>>()
                .join(" AND ");
            let mut stmt = self.conn.prepare(
                "SELECT CAST(l.kind AS INTEGER), l.source_id, l.article_id, a.feed_id, a.title,
                        snippet(library_fts, 3, '', '', ' … ', 32),
                        CASE WHEN CAST(l.kind AS INTEGER) IN (2, 3)
                             THEN COALESCE(s.updated_at, a.fetched_at)
                             ELSE COALESCE(a.published, a.fetched_at) END,
                        a.archived
                 FROM library_fts l
                 JOIN articles a ON a.id = l.article_id
                 LEFT JOIN article_selections s
                        ON s.id = l.source_id AND CAST(l.kind AS INTEGER) IN (2, 3)
                 WHERE library_fts MATCH ?1
                 ORDER BY bm25(library_fts), 7 DESC, l.article_id DESC
                 LIMIT ?2",
            )?;
            let rows = stmt.query_map(params![match_query, limit], map_search_hit)?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        } else {
            let mut stmt = self.conn.prepare(
            "WITH hits AS (\
               SELECT CASE WHEN f.url = ?2 THEN 1 ELSE 0 END AS kind, a.id AS source_id, \
                      a.id AS article_id, a.feed_id, a.title AS article_title, \
                      trim(COALESCE(a.title, '') || CASE \
                        WHEN a.title IS NOT NULL AND a.content IS NOT NULL THEN char(10) ELSE '' END \
                        || COALESCE(a.content, '') || CASE \
                        WHEN a.url IS NOT NULL THEN char(10) || a.url ELSE '' END) AS snippet, \
                      COALESCE(a.published, a.fetched_at) AS timestamp, a.archived \
               FROM articles a JOIN feeds f ON f.id = a.feed_id \
               WHERE instr(lower(COALESCE(a.title, '') || char(10) || \
                         COALESCE(a.author, '') || char(10) || COALESCE(a.content, '') || \
                         char(10) || COALESCE(a.url, '') || char(10) || \
                         COALESCE((SELECT group_concat(t.name, ' ') FROM article_tags at \
                                   JOIN tags t ON t.id = at.tag_id \
                                   WHERE at.article_id = a.id), '')), lower(?1)) > 0 \
               UNION ALL \
               SELECT 2, s.id, a.id, a.feed_id, a.title, s.selected_text, \
                      s.updated_at, a.archived \
               FROM article_selections s JOIN articles a ON a.id = s.article_id \
               WHERE s.is_favorite = 1 \
                 AND instr(lower(s.selected_text), lower(?1)) > 0 \
               UNION ALL \
               SELECT 3, s.id, a.id, a.feed_id, a.title, s.comment, \
                      s.updated_at, a.archived \
               FROM article_selections s JOIN articles a ON a.id = s.article_id \
               WHERE s.comment IS NOT NULL AND length(trim(s.comment)) > 0 \
                 AND instr(lower(s.comment), lower(?1)) > 0 \
             ) \
             SELECT kind, source_id, article_id, feed_id, article_title, snippet, timestamp, archived \
             FROM hits ORDER BY timestamp DESC, article_id DESC LIMIT ?3",
            )?;
            let rows = stmt.query_map(
                params![query, WEB_CLIPPINGS_FEED_URL, limit],
                map_search_hit,
            )?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        self.record_search_history(query, hits.len())?;
        Ok(hits)
    }

    fn record_search_history(&self, query: &str, result_count: usize) -> Result<()> {
        self.conn.execute(
            "INSERT INTO search_history(query, last_used_at, use_count, result_count)
             VALUES (?1, unixepoch(), 1, ?2)
             ON CONFLICT(query) DO UPDATE SET
               last_used_at = excluded.last_used_at,
               use_count = search_history.use_count + 1,
               result_count = excluded.result_count",
            params![query, i64::try_from(result_count).unwrap_or(i64::MAX)],
        )?;
        Ok(())
    }

    pub fn search_history(&self, limit: usize) -> Result<Vec<SearchHistoryEntry>> {
        let mut stmt = self.conn.prepare(
            "SELECT query, last_used_at, use_count, result_count
             FROM search_history ORDER BY last_used_at DESC, query LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![i64::try_from(limit).unwrap_or(i64::MAX)], |row| {
            Ok(SearchHistoryEntry {
                query: row.get(0)?,
                last_used_at: row.get(1)?,
                use_count: row.get(2)?,
                result_count: row.get(3)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn clear_search_history(&self) -> Result<usize> {
        Ok(self.conn.execute("DELETE FROM search_history", [])?)
    }

    // ---- 文章选区：评论与收藏 ----

    /// 保存一段文章选区。评论和收藏可以同时存在，因此使用同一条记录承载。
    ///
    /// `start_offset`/`end_offset` 采用字符偏移而不是字节偏移；它们是可选的，
    /// 仅用于 UI 重新定位选区，正文变化后仍以 `selected_text` 为准。
    #[allow(dead_code, clippy::too_many_arguments)]
    pub fn add_selection(
        &self,
        article_id: i64,
        selected_text: &str,
        start_offset: Option<i64>,
        end_offset: Option<i64>,
        comment: Option<&str>,
        is_favorite: bool,
        now: i64,
    ) -> Result<i64> {
        let anchor = TextAnchor {
            start_offset,
            end_offset,
            prefix: String::new(),
            suffix: String::new(),
        };
        self.add_selection_with_anchor(
            article_id,
            selected_text,
            &anchor,
            comment,
            is_favorite,
            now,
        )
    }

    pub fn add_selection_with_anchor(
        &self,
        article_id: i64,
        selected_text: &str,
        anchor: &TextAnchor,
        comment: Option<&str>,
        is_favorite: bool,
        now: i64,
    ) -> Result<i64> {
        let selected_text = selected_text.trim();
        if selected_text.is_empty() {
            bail!("选中的文字不能为空");
        }
        if let (Some(start), Some(end)) = (anchor.start_offset, anchor.end_offset)
            && (start < 0 || end < start)
        {
            bail!("选区偏移无效");
        }
        let comment = comment
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned);
        self.conn.execute(
            "INSERT INTO article_selections \
             (article_id, selected_text, start_offset, end_offset, anchor_prefix, anchor_suffix, \
              comment, is_favorite, created_at, updated_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?9)",
            params![
                article_id,
                selected_text,
                anchor.start_offset,
                anchor.end_offset,
                anchor.prefix,
                anchor.suffix,
                comment,
                is_favorite,
                now
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// 只添加评论的便捷接口；选区不存在时会创建一条选区记录。
    #[allow(dead_code)]
    pub fn add_comment(
        &self,
        article_id: i64,
        selected_text: &str,
        start_offset: Option<i64>,
        end_offset: Option<i64>,
        comment: &str,
        now: i64,
    ) -> Result<i64> {
        if comment.trim().is_empty() {
            bail!("评论内容不能为空");
        }
        self.add_selection(
            article_id,
            selected_text,
            start_offset,
            end_offset,
            Some(comment),
            false,
            now,
        )
    }

    pub fn add_comment_with_anchor(
        &self,
        article_id: i64,
        selected_text: &str,
        anchor: &TextAnchor,
        comment: &str,
        now: i64,
    ) -> Result<i64> {
        if comment.trim().is_empty() {
            bail!("评论内容不能为空");
        }
        self.add_selection_with_anchor(article_id, selected_text, anchor, Some(comment), false, now)
    }

    /// 只收藏一段选区的便捷接口。
    #[allow(dead_code)]
    pub fn add_favorite_selection(
        &self,
        article_id: i64,
        selected_text: &str,
        start_offset: Option<i64>,
        end_offset: Option<i64>,
        now: i64,
    ) -> Result<i64> {
        self.add_selection(
            article_id,
            selected_text,
            start_offset,
            end_offset,
            None,
            true,
            now,
        )
    }

    pub fn add_favorite_selection_with_anchor(
        &self,
        article_id: i64,
        selected_text: &str,
        anchor: &TextAnchor,
        now: i64,
    ) -> Result<i64> {
        self.add_selection_with_anchor(article_id, selected_text, anchor, None, true, now)
    }

    pub fn get_selection(&self, selection_id: i64) -> Result<ArticleSelection> {
        let sql = format!("SELECT {SELECTION_COLS} FROM article_selections WHERE id = ?1");
        Ok(self
            .conn
            .query_row(&sql, params![selection_id], map_selection)?)
    }

    /// 返回文章中的选区，最新添加的排在前面。
    pub fn selections_for_article(&self, article_id: i64) -> Result<Vec<ArticleSelection>> {
        let sql = format!(
            "SELECT {SELECTION_COLS} FROM article_selections \
             WHERE article_id = ?1 ORDER BY created_at DESC, id DESC"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(params![article_id], map_selection)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// 返回所有收藏的文字选区，供单独的“收藏/摘录”视图使用。
    #[allow(dead_code)]
    pub fn favorite_selections(&self) -> Result<Vec<ArticleSelection>> {
        let sql = format!(
            "SELECT {SELECTION_COLS} FROM article_selections \
             WHERE is_favorite = 1 ORDER BY created_at DESC, id DESC"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map([], map_selection)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// 返回“摘录与想法”视图需要的全部有效记录，并附带跳回文章所需的
    /// feed_id 与文章标题。既没有收藏标记也没有想法内容的历史空记录不显示。
    pub fn saved_selections(&self) -> Result<Vec<(ArticleSelection, i64, Option<String>)>> {
        let mut stmt = self.conn.prepare(
            "SELECT s.id, s.article_id, s.selected_text, s.start_offset, s.end_offset, \
                    s.anchor_prefix, s.anchor_suffix, s.comment, s.is_favorite, \
                    s.created_at, s.updated_at, \
                    a.feed_id, a.title \
             FROM article_selections s \
             JOIN articles a ON a.id = s.article_id \
             WHERE s.is_favorite = 1 \
                OR (s.comment IS NOT NULL AND length(trim(s.comment)) > 0) \
             ORDER BY s.updated_at DESC, s.id DESC",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((map_selection(row)?, row.get(11)?, row.get(12)?))
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn saved_selection_count(&self) -> Result<usize> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM article_selections \
             WHERE is_favorite = 1 \
                OR (comment IS NOT NULL AND length(trim(comment)) > 0)",
            [],
            |row| row.get(0),
        )?;
        Ok(count.max(0) as usize)
    }

    #[allow(dead_code)]
    pub fn set_selection_comment(
        &self,
        selection_id: i64,
        comment: Option<&str>,
        now: i64,
    ) -> Result<usize> {
        let comment = comment
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned);
        Ok(self.conn.execute(
            "UPDATE article_selections SET comment = ?2, updated_at = ?3 WHERE id = ?1",
            params![selection_id, comment, now],
        )?)
    }

    #[allow(dead_code)]
    pub fn set_selection_favorite(
        &self,
        selection_id: i64,
        is_favorite: bool,
        now: i64,
    ) -> Result<usize> {
        Ok(self.conn.execute(
            "UPDATE article_selections SET is_favorite = ?2, updated_at = ?3 WHERE id = ?1",
            params![selection_id, is_favorite, now],
        )?)
    }

    #[allow(dead_code)]
    pub fn toggle_selection_favorite(&self, selection_id: i64, now: i64) -> Result<usize> {
        Ok(self.conn.execute(
            "UPDATE article_selections \
             SET is_favorite = 1 - is_favorite, updated_at = ?2 WHERE id = ?1",
            params![selection_id, now],
        )?)
    }

    pub fn delete_selection(&self, selection_id: i64) -> Result<usize> {
        Ok(self.conn.execute(
            "DELETE FROM article_selections WHERE id = ?1",
            params![selection_id],
        )?)
    }

    pub fn integrity_check(&self) -> Result<DatabaseCheck> {
        check_connection(&self.conn)
    }

    pub fn backup_to(&self, path: &Path) -> Result<()> {
        if path.exists() {
            bail!("备份目标已存在：{}", path.display());
        }
        let backup = self
            .conn
            .backup(rusqlite::MAIN_DB, path, None)
            .with_context(|| format!("创建数据库备份失败：{}", path.display()));
        if let Err(error) = backup {
            remove_database_files(path);
            return Err(error);
        }
        let check = match check_database_file(path) {
            Ok(check) => check,
            Err(error) => {
                remove_database_files(path);
                return Err(error);
            }
        };
        remove_database_sidecars(path);
        if !check.ok {
            let _ = std::fs::remove_file(path);
            bail!("备份校验失败：{}", check.details);
        }
        Ok(())
    }

    #[allow(dead_code)]
    pub fn restore_from(&mut self, path: &Path) -> Result<()> {
        let permit = self.write_permit()?;
        self.restore_from_uncoordinated(path)?;
        if let Some(permit) = permit {
            permit.validate()?;
        }
        Ok(())
    }

    pub(crate) fn restore_from_uncoordinated(&mut self, path: &Path) -> Result<()> {
        let check = check_database_file(path)?;
        if !check.ok {
            bail!("拒绝恢复损坏的备份：{}", check.details);
        }
        self.conn
            .restore(
                rusqlite::MAIN_DB,
                path,
                None::<fn(rusqlite::backup::Progress)>,
            )
            .with_context(|| format!("恢复数据库失败：{}", path.display()))?;
        self.conn.pragma_update(None, "foreign_keys", "ON")?;
        migrate(&self.conn)?;
        Ok(())
    }

    #[allow(dead_code)]
    pub fn compact(&self) -> Result<CompactionReport> {
        let permit = self.write_permit()?;
        let report = self.compact_uncoordinated()?;
        if let Some(permit) = permit {
            permit.validate()?;
        }
        Ok(report)
    }

    pub(crate) fn compact_uncoordinated(&self) -> Result<CompactionReport> {
        let before_bytes = self.disk_bytes();
        self.conn
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE); VACUUM;")
            .context("压缩数据库失败")?;
        Ok(CompactionReport {
            before_bytes,
            after_bytes: self.disk_bytes(),
        })
    }

    pub fn disk_bytes(&self) -> u64 {
        let Some(path) = self.path.as_deref() else {
            return 0;
        };
        database_files(path)
            .into_iter()
            .filter_map(|path| std::fs::metadata(path).ok().map(|meta| meta.len()))
            .sum()
    }
}

fn database_files(path: &Path) -> [PathBuf; 3] {
    let display = path.as_os_str().to_string_lossy();
    [
        path.to_path_buf(),
        PathBuf::from(format!("{display}-wal")),
        PathBuf::from(format!("{display}-shm")),
    ]
}

fn remove_database_sidecars(path: &Path) {
    for sidecar in database_files(path).into_iter().skip(1) {
        let _ = std::fs::remove_file(sidecar);
    }
}

fn remove_database_files(path: &Path) {
    for file in database_files(path) {
        let _ = std::fs::remove_file(file);
    }
}

fn check_database_file(path: &Path) -> Result<DatabaseCheck> {
    let conn = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("打开备份校验失败：{}", path.display()))?;
    check_connection(&conn)
}

fn check_connection(conn: &Connection) -> Result<DatabaseCheck> {
    let integrity = {
        let mut stmt = conn.prepare("PRAGMA integrity_check")?;
        stmt.query_map([], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };
    let foreign_keys = {
        let mut stmt = conn.prepare("PRAGMA foreign_key_check")?;
        stmt.query_map([], |row| {
            Ok(format!(
                "{} row {} references {}",
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?
    };
    let ok =
        integrity.len() == 1 && integrity[0].eq_ignore_ascii_case("ok") && foreign_keys.is_empty();
    let details = if ok {
        "完整性检查通过，未发现页损坏或外键异常".to_owned()
    } else {
        integrity
            .into_iter()
            .chain(foreign_keys)
            .collect::<Vec<_>>()
            .join("；")
    };
    Ok(DatabaseCheck { ok, details })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::NewArticle;

    fn mem() -> Db {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        Db {
            conn,
            path: None,
            _writer_gate: None,
            _lifetime_permit: None,
        }
    }

    fn art(id: &str) -> NewArticle {
        NewArticle {
            entry_id: id.into(),
            url: Some(format!("http://x/{id}")),
            title: Some(id.into()),
            author: None,
            published: Some(100),
            content: Some("body".into()),
        }
    }

    fn feed_articles(db: &Db, feed_id: i64) -> Vec<Article> {
        let sql = format!(
            "SELECT {ARTICLE_COLS} FROM articles \
             WHERE feed_id = ?1 AND archived = 0 \
             ORDER BY COALESCE(published, fetched_at) DESC, id DESC"
        );
        let mut statement = db.conn.prepare(&sql).unwrap();
        statement
            .query_map([feed_id], map_article)
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
    }

    #[test]
    fn dedup_and_counts() {
        let db = mem();
        let cfg = Config::default();
        let id = db.add_feed("http://x/feed", 0).unwrap();
        let feed = db.get_feed(id).unwrap();
        // 首轮两条都是新的
        let n = db
            .record_success(&feed, 0, &cfg, Some("X".into()), &[art("a"), art("b")])
            .unwrap();
        assert_eq!(n, 2);
        // 次轮同样两条 + 一条新的 → 只 1 条新增（ADR-8 去重）
        let feed = db.get_feed(id).unwrap();
        let n = db
            .record_success(&feed, 0, &cfg, None, &[art("a"), art("b"), art("c")])
            .unwrap();
        assert_eq!(n, 1);
        assert_eq!(db.feeds_with_unread().unwrap()[0].1, 3);
    }

    #[test]
    fn archived_article_stays_hidden_after_refetch_and_can_be_restored() {
        let db = mem();
        let cfg = Config::default();
        let feed_id = db.add_feed("http://x/feed", 0).unwrap();
        let feed = db.get_feed(feed_id).unwrap();
        db.record_success(&feed, 10, &cfg, None, &[art("article")])
            .unwrap();
        let article_id = feed_articles(&db, feed_id)[0].id;

        assert_eq!(
            db.conn
                .execute("UPDATE articles SET archived=1 WHERE id=?1", [article_id])
                .unwrap(),
            1
        );
        assert!(feed_articles(&db, feed_id).is_empty());
        assert_eq!(db.feeds_with_unread().unwrap()[0].1, 0);

        let mut refreshed = art("article");
        refreshed.title = Some("refreshed title".into());
        let feed = db.get_feed(feed_id).unwrap();
        assert_eq!(
            db.record_success(&feed, 20, &cfg, None, &[refreshed])
                .unwrap(),
            0
        );

        assert!(feed_articles(&db, feed_id).is_empty());
        let archived = db.get_article(article_id).unwrap();
        assert_eq!(archived.id, article_id);
        assert_eq!(archived.feed_id, feed_id);
        assert_eq!(archived.title.as_deref(), Some("refreshed title"));
        assert!(archived.archived);
        assert_eq!(db.feeds_with_unread().unwrap()[0].1, 0);

        assert_eq!(
            db.conn
                .execute("UPDATE articles SET archived=0 WHERE id=?1", [article_id])
                .unwrap(),
            1
        );
        let restored = feed_articles(&db, feed_id);
        assert_eq!(restored.len(), 1);
        assert_eq!(restored[0].id, article_id);
        assert!(!restored[0].archived);
        assert_eq!(db.feeds_with_unread().unwrap()[0].1, 1);
    }

    #[test]
    fn internal_clippings_feed_is_hidden_and_never_scheduled() {
        let db = mem();
        let normal_id = db.add_feed("https://example.com/feed.xml", 10).unwrap();
        let internal_id = db.ensure_web_clippings_feed(0).unwrap();
        assert_eq!(internal_id, db.ensure_web_clippings_feed(99).unwrap());

        // Even accidental/manual re-enabling must not put this pseudo-feed on
        // the network scheduler.
        db.conn
            .execute(
                "UPDATE feeds SET disabled = 0, next_fetch = 0 WHERE id = ?1",
                params![internal_id],
            )
            .unwrap();

        let listed = db.feeds_with_unread().unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].0.id, normal_id);
        assert_eq!(db.enabled_feeds().unwrap().len(), 1);
        assert_eq!(db.due_feeds(10).unwrap().len(), 1);
        assert_eq!(db.earliest_next_fetch().unwrap(), Some(10));

        assert_eq!(db.remove_feed(WEB_CLIPPINGS_FEED_URL).unwrap(), 0);
        assert_eq!(db.remove_feed(&internal_id.to_string()).unwrap(), 0);
        assert!(db.get_feed(internal_id).is_ok());
    }

    #[test]
    fn web_clipping_saves_immutable_snapshots_for_urls_and_raw_html() {
        let db = mem();
        let first_id = db
            .save_web_clipping(
                Some(" https://example.com/story "),
                Some(" First title "),
                "<p>first snapshot</p>",
                10,
            )
            .unwrap();
        let second_id = db
            .save_web_clipping(
                Some("https://example.com/story"),
                None,
                "<p>second snapshot</p>",
                20,
            )
            .unwrap();
        assert_ne!(first_id, second_id);

        let raw_a = db
            .save_web_clipping(None, Some("Pasted A"), "<p>A</p>", 30)
            .unwrap();
        let raw_b = db
            .save_web_clipping(None, Some("Pasted B"), "<p>B</p>", 30)
            .unwrap();
        assert_ne!(raw_a, raw_b);
        assert_eq!(db.web_clippings().unwrap().len(), 4);

        let clips = db.web_clippings().unwrap();
        let first = clips.iter().find(|article| article.id == first_id).unwrap();
        let second = clips
            .iter()
            .find(|article| article.id == second_id)
            .unwrap();
        assert_ne!(first.entry_id, second.entry_id);
        assert!(first.entry_id.starts_with("clip:"));
        assert!(second.entry_id.starts_with("clip:"));
        assert_eq!(first.title.as_deref(), Some("First title"));
        assert_eq!(first.content.as_deref(), Some("<p>first snapshot</p>"));
        assert_eq!(second.title, None);
        assert_eq!(second.content.as_deref(), Some("<p>second snapshot</p>"));
        assert_eq!(first.url.as_deref(), Some("https://example.com/story"));
        assert_eq!(second.url.as_deref(), Some("https://example.com/story"));
        assert!(first.starred);
        assert!(first.is_read);
        assert!(!first.archived);
        assert!(db.is_web_clipping(first_id).unwrap());
        assert!(db.is_web_clipping(second_id).unwrap());
    }

    #[test]
    fn web_clipping_delete_is_scoped_to_hidden_feed() {
        let db = mem();
        let cfg = Config::default();
        let feed_id = db.add_feed("http://x/feed", 0).unwrap();
        let feed = db.get_feed(feed_id).unwrap();
        db.record_success(&feed, 1, &cfg, None, &[art("starred"), art("plain")])
            .unwrap();
        let normal = feed_articles(&db, feed_id);
        let starred_id = normal
            .iter()
            .find(|article| article.entry_id == "starred")
            .unwrap()
            .id;

        let clipping_id = db
            .save_web_clipping(
                Some("https://example.com/saved"),
                Some("Saved page"),
                "<main>saved</main>",
                2,
            )
            .unwrap();

        assert_eq!(db.delete_web_clipping(starred_id).unwrap(), 0);
        assert!(
            feed_articles(&db, feed_id)
                .iter()
                .any(|a| a.id == starred_id)
        );
        assert_eq!(db.delete_web_clipping(clipping_id).unwrap(), 1);
        assert!(db.web_clippings().unwrap().is_empty());
        assert!(!db.is_web_clipping(clipping_id).unwrap());
    }

    #[test]
    fn web_clipping_rejects_empty_html() {
        let db = mem();
        assert!(
            db.save_web_clipping(Some("https://example.com"), Some("Empty"), " \n\t ", 0)
                .is_err()
        );
        assert!(db.web_clippings().unwrap().is_empty());
    }

    #[test]
    fn migration_adds_archived_to_existing_articles_table() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE articles (\
               id INTEGER PRIMARY KEY, feed_id INTEGER NOT NULL, entry_id TEXT NOT NULL, \
               url TEXT, title TEXT, author TEXT, published INTEGER, content TEXT, \
               is_read INTEGER NOT NULL DEFAULT 0, starred INTEGER NOT NULL DEFAULT 0, \
               fetched_at INTEGER NOT NULL, UNIQUE(feed_id, entry_id)\
             ); \
             INSERT INTO articles (feed_id, entry_id, fetched_at) VALUES (7, 'old', 11);",
        )
        .unwrap();

        conn.execute_batch(SCHEMA).unwrap();
        migrate(&conn).unwrap();
        migrate(&conn).unwrap();

        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, CURRENT_SCHEMA_VERSION);

        let archived: bool = conn
            .query_row(
                "SELECT archived FROM articles WHERE entry_id = 'old'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(!archived);
    }

    #[test]
    fn migration_rejects_a_database_from_a_newer_application() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        conn.pragma_update(None, "user_version", CURRENT_SCHEMA_VERSION + 1)
            .unwrap();

        let error = migrate(&conn).unwrap_err().to_string();
        assert!(error.contains("高于当前程序支持的版本"));
    }

    #[test]
    fn version_one_migration_adds_workflow_tables_and_interrupts_legacy_runs() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE resource_enrichment_runs (
               id INTEGER PRIMARY KEY,
               status TEXT NOT NULL,
               finished_at INTEGER,
               error_code TEXT,
               error_message TEXT
             );
             INSERT INTO resource_enrichment_runs(id,status) VALUES(1,'running');
             PRAGMA user_version=1;",
        )
        .unwrap();

        migrate(&conn).unwrap();

        let task_table: String = conn
            .query_row(
                "SELECT name FROM sqlite_master WHERE type='table' AND name='knowledge_tasks'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let legacy_status: (String, String) = conn
            .query_row(
                "SELECT status,error_code FROM resource_enrichment_runs WHERE id=1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(task_table, "knowledge_tasks");
        assert_eq!(
            legacy_status,
            ("failed".into(), "UPGRADE_INTERRUPTED".into())
        );
    }

    #[test]
    fn version_two_migration_adds_executor_fencing_and_preserves_queued_work() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE resources(id INTEGER PRIMARY KEY);
             CREATE TABLE resource_enrichment_runs (
               id INTEGER PRIMARY KEY,
               status TEXT NOT NULL,
               finished_at INTEGER,
               error_code TEXT,
               error_message TEXT
             );
             CREATE TABLE knowledge_tasks (
               id INTEGER PRIMARY KEY,
               kind TEXT NOT NULL,
               target_id INTEGER NOT NULL,
               status TEXT NOT NULL,
               current_stage TEXT,
               next_run_at INTEGER NOT NULL DEFAULT 0,
               created_at INTEGER NOT NULL,
               updated_at INTEGER NOT NULL
             );
             CREATE TABLE knowledge_task_attempts (
               id INTEGER PRIMARY KEY,
               task_id INTEGER NOT NULL,
               attempt_number INTEGER NOT NULL,
               status TEXT NOT NULL,
               current_stage TEXT,
               automatic_retry INTEGER NOT NULL DEFAULT 0,
               started_at INTEGER,
               finished_at INTEGER,
               error_kind TEXT,
               user_message TEXT,
               technical_detail TEXT,
               created_at INTEGER NOT NULL
             );
             INSERT INTO knowledge_tasks(id,kind,target_id,status,current_stage,created_at,updated_at)
               VALUES(1,'resource_completion',1,'running','organizing',10,10),
                     (2,'resource_completion',2,'queued',NULL,11,11);
             INSERT INTO knowledge_task_attempts(id,task_id,attempt_number,status,current_stage,created_at)
               VALUES(1,1,1,'running','organizing',10),(2,2,1,'queued',NULL,11);
             PRAGMA user_version=2;",
        )
        .unwrap();

        migrate(&conn).unwrap();

        let statuses: (String, String) = conn
            .query_row(
                "SELECT (SELECT status FROM knowledge_tasks WHERE id=1),
                        (SELECT status FROM knowledge_tasks WHERE id=2)",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(statuses, ("interrupted".into(), "queued".into()));
        assert!(has_column(&conn, "knowledge_tasks", "change_seq").unwrap());
        assert!(has_column(&conn, "knowledge_task_attempts", "claim_generation").unwrap());
        assert!(has_column(&conn, "resource_enrichment_runs", "attempt_id").unwrap());
        let lease_generation: i64 = conn
            .query_row(
                "SELECT generation FROM knowledge_executor_lease WHERE singleton_id=1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(lease_generation, 0);
    }

    #[test]
    fn version_three_migration_splits_resource_curation_health_and_provenance() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        conn.execute_batch(
            "INSERT INTO resources(id,url,canonical_url,kind,status,source,created_at,updated_at)
               VALUES(1,'https://example.com/review','https://example.com/review','page','pending_review','cli_agent',1,1),
                     (2,'https://example.com/healthy','https://example.com/healthy','page','active','gui',2,2),
                     (3,'https://example.com/broken','https://example.com/broken','page','broken','gui',3,3),
                     (4,'https://example.com/archived','https://example.com/archived','page','archived','gui',4,4);
             INSERT INTO resource_snapshots(resource_id,content_hash,fetched_at)
               VALUES(2,'snapshot-hash',2);
             INSERT INTO resource_enrichment_runs(
               resource_id,provider,model,prompt_version,schema_version,started_at,finished_at,status
             ) VALUES(2,'fixture','fixture','v1','v1',2,2,'succeeded');
             INSERT INTO resource_tags(resource_id,name,language,source,created_at)
               VALUES(3,'manual-tag','en','manual',3);
             UPDATE resources
               SET curation_state='active',health='unknown',categories_source='ai',tags_source='ai';
             PRAGMA user_version=3;",
        )
        .unwrap();

        migrate(&conn).unwrap();

        let states = conn
            .prepare(
                "SELECT curation_state,health,categories_source,tags_source
                 FROM resources ORDER BY id",
            )
            .unwrap()
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(
            states,
            vec![
                (
                    "pending_review".into(),
                    "unknown".into(),
                    "manual".into(),
                    "ai".into()
                ),
                ("active".into(), "healthy".into(), "ai".into(), "ai".into()),
                (
                    "active".into(),
                    "broken".into(),
                    "manual".into(),
                    "manual".into()
                ),
                (
                    "archived".into(),
                    "unknown".into(),
                    "manual".into(),
                    "ai".into()
                ),
            ]
        );
        assert_eq!(
            conn.query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            4
        );
    }

    #[test]
    fn backoff_and_disable() {
        let db = mem();
        let cfg = Config {
            disable_after_failures: 2,
            ..Config::default()
        };
        let id = db.add_feed("http://x/feed", 0).unwrap();
        let feed = db.get_feed(id).unwrap();
        db.record_failure(&feed, 0, &cfg, "boom").unwrap();
        let feed = db.get_feed(id).unwrap();
        assert_eq!(feed.fail_count, 1);
        assert!(!feed.disabled);
        db.record_failure(&feed, 0, &cfg, "boom").unwrap();
        let feed = db.get_feed(id).unwrap();
        assert!(feed.disabled); // 连续 2 次 → 禁用
    }
    #[test]
    fn selection_comment_and_favorite_crud() {
        let db = mem();
        let cfg = Config::default();
        let feed_id = db.add_feed("http://x/feed", 0).unwrap();
        let feed = db.get_feed(feed_id).unwrap();
        db.record_success(&feed, 0, &cfg, None, &[art("article")])
            .unwrap();
        let article_id = feed_articles(&db, feed_id)[0].id;

        let comment_id = db
            .add_comment(
                article_id,
                "一段被选中的文字",
                Some(10),
                Some(18),
                "这里需要进一步核实",
                100,
            )
            .unwrap();
        let favorite_id = db
            .add_favorite_selection(article_id, "另一段摘录", None, None, 101)
            .unwrap();

        let rows = db.selections_for_article(article_id).unwrap();
        assert_eq!(rows.len(), 2);
        let saved = db.saved_selections().unwrap();
        assert_eq!(saved.len(), 2);
        assert_eq!(saved[0].1, feed_id);
        assert_eq!(saved[0].2.as_deref(), Some("article"));
        assert_eq!(db.saved_selection_count().unwrap(), 2);
        let comment = db.get_selection(comment_id).unwrap();
        assert_eq!(comment.comment.as_deref(), Some("这里需要进一步核实"));
        assert_eq!(comment.start_offset, Some(10));
        assert!(!comment.is_favorite);

        db.set_selection_favorite(comment_id, true, 102).unwrap();
        db.set_selection_comment(comment_id, None, 103).unwrap();
        let updated = db.get_selection(comment_id).unwrap();
        assert!(updated.is_favorite);
        assert_eq!(updated.comment, None);
        assert_eq!(db.favorite_selections().unwrap().len(), 2);

        db.toggle_selection_favorite(favorite_id, 104).unwrap();
        assert_eq!(db.favorite_selections().unwrap().len(), 1);
        assert_eq!(db.saved_selection_count().unwrap(), 1);
        assert_eq!(db.delete_selection(comment_id).unwrap(), 1);
        assert_eq!(db.selections_for_article(article_id).unwrap().len(), 1);
        assert_eq!(db.saved_selection_count().unwrap(), 0);
    }

    #[test]
    fn selection_rejects_empty_values_and_bad_offsets() {
        let db = mem();
        let cfg = Config::default();
        let feed_id = db.add_feed("http://x/feed", 0).unwrap();
        let feed = db.get_feed(feed_id).unwrap();
        db.record_success(&feed, 0, &cfg, None, &[art("article")])
            .unwrap();
        let article_id = feed_articles(&db, feed_id)[0].id;

        assert!(
            db.add_selection(article_id, "  ", None, None, None, true, 0)
                .is_err()
        );
        assert!(
            db.add_comment(article_id, "text", None, None, "  ", 0)
                .is_err()
        );
        assert!(
            db.add_selection(article_id, "text", Some(5), Some(3), None, false, 0)
                .is_err()
        );
    }

    #[test]
    fn full_text_search_covers_articles_clippings_excerpts_and_thoughts() {
        let db = mem();
        let cfg = Config::default();
        let feed_id = db.add_feed("https://example.com/feed.xml", 0).unwrap();
        let feed = db.get_feed(feed_id).unwrap();
        let mut article = art("rust-architecture");
        article.title = Some("Rust 架构笔记".into());
        article.author = Some("Alice".into());
        article.content = Some("<p>正文讨论分层设计和事件驱动。</p>".into());
        db.record_success(&feed, 100, &cfg, None, &[article])
            .unwrap();
        let article_id = feed_articles(&db, feed_id)[0].id;
        db.add_favorite_selection(article_id, "重要的领域模型摘录", None, None, 110)
            .unwrap();
        db.add_comment(
            article_id,
            "另一段正文",
            None,
            None,
            "想到用状态机梳理流程",
            120,
        )
        .unwrap();
        let clipping_id = db
            .save_web_clipping(
                Some("https://example.com/guide"),
                Some("离线网页指南"),
                "<main>网页快照包含缓存策略</main>",
                130,
            )
            .unwrap();

        let article_hits = db.search_library("事件驱动", 20).unwrap();
        assert_eq!(article_hits.len(), 1);
        assert_eq!(article_hits[0].kind, SearchHitKind::Article);
        assert_eq!(article_hits[0].article_id, article_id);

        let clip_hits = db.search_library("缓存策略", 20).unwrap();
        assert_eq!(clip_hits.len(), 1);
        assert_eq!(clip_hits[0].kind, SearchHitKind::WebClipping);
        assert_eq!(clip_hits[0].article_id, clipping_id);

        let excerpt_hits = db.search_library("领域模型", 20).unwrap();
        assert_eq!(excerpt_hits.len(), 1);
        assert_eq!(excerpt_hits[0].kind, SearchHitKind::Excerpt);

        let thought_hits = db.search_library("状态机", 20).unwrap();
        assert_eq!(thought_hits.len(), 1);
        assert_eq!(thought_hits[0].kind, SearchHitKind::Thought);
    }

    #[test]
    fn full_text_search_handles_empty_queries_limits_and_archived_results() {
        let db = mem();
        let cfg = Config::default();
        let feed_id = db.add_feed("https://example.com/feed.xml", 0).unwrap();
        let feed = db.get_feed(feed_id).unwrap();
        let mut first = art("first");
        first.content = Some("shared keyword".into());
        let mut second = art("second");
        second.content = Some("shared keyword".into());
        db.record_success(&feed, 100, &cfg, None, &[first, second])
            .unwrap();
        let archived_id = feed_articles(&db, feed_id)
            .into_iter()
            .map(|article| article.id)
            .max()
            .unwrap();
        db.conn
            .execute("UPDATE articles SET archived=1 WHERE id=?1", [archived_id])
            .unwrap();

        assert!(db.search_library("   ", 20).unwrap().is_empty());
        assert!(db.search_library("shared", 0).unwrap().is_empty());
        assert_eq!(db.search_library("SHARED", 1).unwrap().len(), 1);
        let hits = db.search_library("SHARED", 20).unwrap();
        assert_eq!(hits.len(), 2);
        assert!(
            hits.iter()
                .any(|hit| hit.article_id == archived_id && hit.archived)
        );
    }

    #[test]
    fn anchored_selection_and_search_history_round_trip() {
        let db = mem();
        let cfg = Config::default();
        let feed_id = db.add_feed("https://example.com/feed.xml", 0).unwrap();
        let feed = db.get_feed(feed_id).unwrap();
        db.record_success(&feed, 100, &cfg, None, &[art("anchor")])
            .unwrap();
        let article_id = feed_articles(&db, feed_id)[0].id;
        let anchor = TextAnchor {
            start_offset: Some(3),
            end_offset: Some(7),
            prefix: "前文".to_owned(),
            suffix: "后文".to_owned(),
        };
        let selection_id = db
            .add_favorite_selection_with_anchor(article_id, "稳定锚点", &anchor, 120)
            .unwrap();
        let selection = db.get_selection(selection_id).unwrap();
        assert_eq!(selection.anchor_prefix, "前文");
        assert_eq!(selection.anchor_suffix, "后文");

        assert_eq!(db.search_library("稳定锚点", 20).unwrap().len(), 1);
        assert_eq!(db.search_library("稳定锚点", 20).unwrap().len(), 1);
        let history = db.search_history(10).unwrap();
        assert_eq!(history[0].query, "稳定锚点");
        assert_eq!(history[0].use_count, 2);
        assert_eq!(history[0].result_count, 1);
    }

    #[test]
    fn file_database_can_be_checked_backed_up_and_compacted() {
        let root = std::env::temp_dir().join(format!(
            "shiyue-db-maintenance-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let live = root.join("live.db");
        let backup = root.join("backup.db");
        let db = Db::open(&live).unwrap();
        db.add_feed("https://example.com/feed.xml", 1).unwrap();

        let check = db.integrity_check().unwrap();
        assert!(check.ok, "{}", check.details);
        db.backup_to(&backup).unwrap();
        assert!(backup.exists());
        let report = db.compact().unwrap();
        assert!(report.after_bytes > 0);

        drop(db);
        std::fs::remove_dir_all(root).unwrap();
    }
}
