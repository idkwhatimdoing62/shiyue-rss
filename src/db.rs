//! SQLite 访问层（ADR-3）。daemon 与 TUI 两进程共享同一库，WAL 模式扛并发。

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, Row, params};
use std::path::{Path, PathBuf};

use crate::config::Config;
use crate::model::{
    Article, ArticleBatchAction, ArticleSelection, Feed, NewArticle, SearchHistoryEntry, SearchHit,
    SearchHitKind, Tag, TextAnchor,
};

/// Hidden, non-network feed used to reuse the normal article reader and its
/// annotations for locally saved web pages.
pub const WEB_CLIPPINGS_FEED_URL: &str = "shiyue://web-clippings";
const WEB_CLIPPINGS_FEED_TITLE: &str = "网页收藏";

const SCHEMA: &str = r#"
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
CREATE TABLE IF NOT EXISTS search_history (
  query         TEXT PRIMARY KEY COLLATE NOCASE,
  last_used_at  INTEGER NOT NULL,
  use_count     INTEGER NOT NULL DEFAULT 1,
  result_count  INTEGER NOT NULL DEFAULT 0
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
    conn: Connection,
    path: Option<PathBuf>,
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

fn migrate(conn: &Connection) -> Result<()> {
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
        let conn = Connection::open(path)
            .with_context(|| format!("打开数据库失败: {}", path.display()))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.execute_batch(SCHEMA)?;
        migrate(&conn)?;
        Ok(Self {
            conn,
            path: Some(path.to_path_buf()),
        })
    }

    // ---- 源的增删查改 ----

    /// 添加源（幂等：已存在则返回既有 id）。
    pub fn add_feed(&self, url: &str, now: i64) -> Result<i64> {
        self.conn.execute(
            "INSERT OR IGNORE INTO feeds (url, next_fetch) VALUES (?1, ?2)",
            params![url, now],
        )?;
        let id = self
            .conn
            .query_row("SELECT id FROM feeds WHERE url = ?1", params![url], |r| {
                r.get(0)
            })?;
        Ok(id)
    }

    /// 按 id 或 url 删除，返回删除行数。
    pub fn remove_feed(&self, target: &str) -> Result<usize> {
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

    fn query_feeds(&self, where_clause: &str, args: &[&dyn rusqlite::ToSql]) -> Result<Vec<Feed>> {
        let sql = format!("SELECT {FEED_COLS} FROM feeds {where_clause}");
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(args, map_feed)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// 到期且未禁用的源（daemon 用）。
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

    /// 最近一个到期时间（daemon 决定 sleep 多久）。
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

    /// Unified article-level saved view: regular starred RSS articles and all
    /// locally saved web pages, newest first.
    pub fn saved_articles(&self) -> Result<Vec<Article>> {
        let sql = format!(
            "SELECT {ARTICLE_COLS} FROM articles \
             WHERE archived = 0 AND (starred = 1 \
                OR feed_id = (SELECT id FROM feeds WHERE url = ?1)) \
             ORDER BY COALESCE(published, fetched_at) DESC, id DESC"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(params![WEB_CLIPPINGS_FEED_URL], map_article)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn saved_article_count(&self) -> Result<usize> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM articles a \
             JOIN feeds f ON f.id = a.feed_id \
             WHERE a.archived = 0 AND (a.starred = 1 OR f.url = ?1)",
            params![WEB_CLIPPINGS_FEED_URL],
            |row| row.get(0),
        )?;
        Ok(count.max(0) as usize)
    }

    pub fn read_later_articles(&self) -> Result<Vec<Article>> {
        let sql = format!(
            "SELECT {ARTICLE_COLS} FROM articles
             WHERE archived = 0 AND read_later = 1
             ORDER BY COALESCE(published, fetched_at) DESC, id DESC"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map([], map_article)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn read_later_count(&self) -> Result<usize> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM articles WHERE archived = 0 AND read_later = 1",
            [],
            |row| row.get(0),
        )?;
        Ok(count.max(0) as usize)
    }

    pub fn set_article_read_later(&self, article_id: i64, read_later: bool) -> Result<usize> {
        Ok(self.conn.execute(
            "UPDATE articles SET read_later = ?2 WHERE id = ?1",
            params![article_id, read_later],
        )?)
    }

    /// Applies one explicit target state to every selected article in one SQL statement.
    pub fn apply_article_batch(
        &self,
        article_ids: &[i64],
        action: ArticleBatchAction,
    ) -> Result<usize> {
        if article_ids.is_empty() {
            return Ok(0);
        }
        let assignment = match action {
            ArticleBatchAction::Archive => "archived = 1",
            ArticleBatchAction::Bookmark => "starred = 1",
            ArticleBatchAction::ReadLater => "read_later = 1",
        };
        let placeholders = std::iter::repeat_n("?", article_ids.len())
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!("UPDATE articles SET {assignment} WHERE id IN ({placeholders})");
        Ok(self
            .conn
            .execute(&sql, rusqlite::params_from_iter(article_ids.iter()))?)
    }

    pub fn tags_for_article(&self, article_id: i64) -> Result<Vec<Tag>> {
        let mut stmt = self.conn.prepare(
            "SELECT t.id, t.name FROM tags t
             JOIN article_tags at ON at.tag_id = t.id
             WHERE at.article_id = ?1 ORDER BY t.name COLLATE NOCASE",
        )?;
        let rows = stmt.query_map(params![article_id], |row| {
            Ok(Tag {
                id: row.get(0)?,
                name: row.get(1)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Replaces an article's complete tag set atomically. Empty and duplicate
    /// names are normalized away inside this module.
    pub fn replace_article_tags(&self, article_id: i64, names: &[String], now: i64) -> Result<()> {
        let mut normalized = names
            .iter()
            .map(|name| name.trim())
            .filter(|name| !name.is_empty())
            .map(str::to_owned)
            .collect::<Vec<_>>();
        normalized.sort_by_key(|name| name.to_lowercase());
        normalized.dedup_by(|left, right| left.eq_ignore_ascii_case(right));

        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "DELETE FROM article_tags WHERE article_id = ?1",
            params![article_id],
        )?;
        for name in normalized {
            tx.execute(
                "INSERT INTO tags(name, created_at) VALUES (?1, ?2)
                 ON CONFLICT(name) DO NOTHING",
                params![name, now],
            )?;
            tx.execute(
                "INSERT INTO article_tags(article_id, tag_id, created_at)
                 SELECT ?1, id, ?3 FROM tags WHERE name = ?2 COLLATE NOCASE",
                params![article_id, name, now],
            )?;
        }
        tx.execute(
            "DELETE FROM tags WHERE NOT EXISTS
             (SELECT 1 FROM article_tags WHERE article_tags.tag_id = tags.id)",
            [],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn set_interval(&self, id: i64, secs: i64) -> Result<usize> {
        Ok(self.conn.execute(
            "UPDATE feeds SET interval_secs = ?2 WHERE id = ?1",
            params![id, secs],
        )?)
    }

    pub fn set_disabled(&self, id: i64, disabled: bool, now: i64) -> Result<usize> {
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

    pub fn articles_for_feed(&self, feed_id: i64) -> Result<Vec<Article>> {
        let sql = format!(
            "SELECT {ARTICLE_COLS} FROM articles \
             WHERE feed_id = ?1 AND archived = 0 \
             ORDER BY COALESCE(published, fetched_at) DESC"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(params![feed_id], map_article)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn get_article(&self, article_id: i64) -> Result<Article> {
        let sql = format!("SELECT {ARTICLE_COLS} FROM articles WHERE id = ?1");
        Ok(self
            .conn
            .query_row(&sql, params![article_id], map_article)?)
    }

    pub fn archived_articles(&self) -> Result<Vec<Article>> {
        let sql = format!(
            "SELECT {ARTICLE_COLS} FROM articles \
             WHERE archived = 1 ORDER BY COALESCE(published, fetched_at) DESC, id DESC"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map([], map_article)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn archived_article_count(&self) -> Result<usize> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM articles WHERE archived = 1",
            [],
            |row| row.get(0),
        )?;
        Ok(count.max(0) as usize)
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

    pub fn set_article_archived(&self, article_id: i64, archived: bool) -> Result<usize> {
        Ok(self.conn.execute(
            "UPDATE articles SET archived = ?2 WHERE id = ?1",
            params![article_id, archived],
        )?)
    }

    pub fn mark_read(&self, article_id: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE articles SET is_read = 1 WHERE id = ?1",
            params![article_id],
        )?;
        Ok(())
    }

    pub fn mark_unread(&self, article_id: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE articles SET is_read = 0 WHERE id = ?1",
            params![article_id],
        )?;
        Ok(())
    }

    pub fn toggle_star(&self, article_id: i64) -> Result<()> {
        let changed = self.conn.execute(
            "UPDATE articles SET starred = 1 - starred WHERE id = ?1",
            params![article_id],
        )?;
        if changed == 0 {
            bail!("文章不存在或已被删除");
        }
        Ok(())
    }

    // ---- 文章选区：评论与收藏 ----

    /// 保存一段文章选区。评论和收藏可以同时存在，因此使用同一条记录承载。
    ///
    /// `start_offset`/`end_offset` 采用字符偏移而不是字节偏移；它们是可选的，
    /// 仅用于 UI 重新定位选区，正文变化后仍以 `selected_text` 为准。
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
        if let (Some(start), Some(end)) = (anchor.start_offset, anchor.end_offset) {
            if start < 0 || end < start {
                bail!("选区偏移无效");
            }
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

    pub fn restore_from(&mut self, path: &Path) -> Result<()> {
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

    pub fn compact(&self) -> Result<CompactionReport> {
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
        Db { conn, path: None }
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
        let article_id = db.articles_for_feed(feed_id).unwrap()[0].id;

        assert_eq!(db.set_article_archived(article_id, true).unwrap(), 1);
        assert!(db.articles_for_feed(feed_id).unwrap().is_empty());
        assert_eq!(db.archived_article_count().unwrap(), 1);
        assert_eq!(db.feeds_with_unread().unwrap()[0].1, 0);

        let mut refreshed = art("article");
        refreshed.title = Some("refreshed title".into());
        let feed = db.get_feed(feed_id).unwrap();
        assert_eq!(
            db.record_success(&feed, 20, &cfg, None, &[refreshed])
                .unwrap(),
            0
        );

        assert!(db.articles_for_feed(feed_id).unwrap().is_empty());
        let archived = db.archived_articles().unwrap();
        assert_eq!(archived.len(), 1);
        assert_eq!(archived[0].id, article_id);
        assert_eq!(archived[0].feed_id, feed_id);
        assert_eq!(archived[0].title.as_deref(), Some("refreshed title"));
        assert!(archived[0].archived);
        assert_eq!(db.feeds_with_unread().unwrap()[0].1, 0);

        assert_eq!(db.set_article_archived(article_id, false).unwrap(), 1);
        assert_eq!(db.archived_article_count().unwrap(), 0);
        assert!(db.archived_articles().unwrap().is_empty());
        let restored = db.articles_for_feed(feed_id).unwrap();
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
    fn saved_articles_unifies_stars_and_clippings_and_delete_is_scoped() {
        let db = mem();
        let cfg = Config::default();
        let feed_id = db.add_feed("http://x/feed", 0).unwrap();
        let feed = db.get_feed(feed_id).unwrap();
        db.record_success(&feed, 1, &cfg, None, &[art("starred"), art("plain")])
            .unwrap();
        let normal = db.articles_for_feed(feed_id).unwrap();
        let starred_id = normal
            .iter()
            .find(|article| article.entry_id == "starred")
            .unwrap()
            .id;
        let plain_id = normal
            .iter()
            .find(|article| article.entry_id == "plain")
            .unwrap()
            .id;
        db.toggle_star(starred_id).unwrap();

        let clipping_id = db
            .save_web_clipping(
                Some("https://example.com/saved"),
                Some("Saved page"),
                "<main>saved</main>",
                2,
            )
            .unwrap();

        let saved = db.saved_articles().unwrap();
        assert_eq!(saved.len(), 2);
        assert_eq!(db.saved_article_count().unwrap(), 2);
        assert!(saved.iter().any(|article| article.id == starred_id));
        assert!(saved.iter().any(|article| article.id == clipping_id));
        assert!(!saved.iter().any(|article| article.id == plain_id));

        assert_eq!(db.delete_web_clipping(starred_id).unwrap(), 0);
        assert!(
            db.articles_for_feed(feed_id)
                .unwrap()
                .iter()
                .any(|a| a.id == starred_id)
        );
        assert_eq!(db.delete_web_clipping(clipping_id).unwrap(), 1);
        assert!(db.web_clippings().unwrap().is_empty());
        assert!(!db.is_web_clipping(clipping_id).unwrap());
        assert_eq!(db.saved_article_count().unwrap(), 1);
    }

    #[test]
    fn saved_articles_do_not_reopen_archived_items() {
        let db = mem();
        let cfg = Config::default();
        let feed_id = db.add_feed("http://x/feed", 0).unwrap();
        let feed = db.get_feed(feed_id).unwrap();
        db.record_success(&feed, 1, &cfg, None, &[art("starred")])
            .unwrap();
        let article_id = db.articles_for_feed(feed_id).unwrap()[0].id;
        db.toggle_star(article_id).unwrap();
        db.set_article_archived(article_id, true).unwrap();

        assert!(db.saved_articles().unwrap().is_empty());
        assert_eq!(db.saved_article_count().unwrap(), 0);
    }

    #[test]
    fn toggling_a_missing_article_reports_failure() {
        let db = mem();
        assert!(db.toggle_star(999_999).is_err());
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
    fn backoff_and_disable() {
        let db = mem();
        let mut cfg = Config::default();
        cfg.disable_after_failures = 2;
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
        let article_id = db.articles_for_feed(feed_id).unwrap()[0].id;

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
        let article_id = db.articles_for_feed(feed_id).unwrap()[0].id;

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
        let article_id = db.articles_for_feed(feed_id).unwrap()[0].id;
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
        let archived_id = db
            .articles_for_feed(feed_id)
            .unwrap()
            .into_iter()
            .map(|article| article.id)
            .max()
            .unwrap();
        db.set_article_archived(archived_id, true).unwrap();

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
    fn tags_read_later_and_batch_actions_are_persistent() {
        let db = mem();
        let cfg = Config::default();
        let feed_id = db.add_feed("https://example.com/feed.xml", 0).unwrap();
        let feed = db.get_feed(feed_id).unwrap();
        db.record_success(&feed, 100, &cfg, None, &[art("one"), art("two")])
            .unwrap();
        let articles = db.articles_for_feed(feed_id).unwrap();
        let ids = articles
            .iter()
            .map(|article| article.id)
            .collect::<Vec<_>>();

        db.replace_article_tags(
            ids[0],
            &[" 架构 ".to_owned(), "Rust".to_owned(), "rust".to_owned()],
            101,
        )
        .unwrap();
        let tags = db.tags_for_article(ids[0]).unwrap();
        assert_eq!(tags.len(), 2);
        assert_eq!(db.search_library("架构", 20).unwrap()[0].article_id, ids[0]);

        db.apply_article_batch(&ids, ArticleBatchAction::ReadLater)
            .unwrap();
        assert_eq!(db.read_later_count().unwrap(), 2);
        db.apply_article_batch(&ids[..1], ArticleBatchAction::Bookmark)
            .unwrap();
        assert!(db.get_article(ids[0]).unwrap().starred);
        db.apply_article_batch(&ids[1..], ArticleBatchAction::Archive)
            .unwrap();
        assert_eq!(db.read_later_articles().unwrap().len(), 1);
    }

    #[test]
    fn anchored_selection_and_search_history_round_trip() {
        let db = mem();
        let cfg = Config::default();
        let feed_id = db.add_feed("https://example.com/feed.xml", 0).unwrap();
        let feed = db.get_feed(feed_id).unwrap();
        db.record_success(&feed, 100, &cfg, None, &[art("anchor")])
            .unwrap();
        let article_id = db.articles_for_feed(feed_id).unwrap()[0].id;
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
