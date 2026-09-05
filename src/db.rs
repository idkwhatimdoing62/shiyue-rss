//! SQLite 访问层（ADR-3）。GUI、CLI 与后台工作流共享同一库，WAL 模式扛并发。

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, Row, TransactionBehavior, params};
use std::path::{Path, PathBuf};

use crate::config::Config;
use crate::library_projection_revision::{
    self, LibraryGeneration, ProjectionFamily, ProjectionImpact,
};
use crate::local_data_maintenance::{
    ConnectionFence, FencedTransaction, GenerationFence, MaintenanceFence, run_schema_migration,
};
use crate::model::{Article, Feed, NewArticle};
#[cfg(test)]
use crate::model::{ArticleSelection, TextAnchor};
use crate::schema_evolution::{self, SchemaReadiness};

/// Hidden, non-network feed used to reuse the normal article reader and its
/// annotations for locally saved web pages.
pub const WEB_CLIPPINGS_FEED_URL: &str = "shiyue://web-clippings";
#[cfg(test)]
const WEB_CLIPPINGS_FEED_TITLE: &str = "网页收藏";

#[derive(Debug, Clone)]
pub struct ArticleAiContent {
    pub summary_zh: String,
    pub translation_zh: String,
    pub model: String,
    pub updated_at: i64,
}

const FEED_COLS: &str =
    "id, url, title, interval_secs, last_fetch, next_fetch, last_error, fail_count, disabled";
const ARTICLE_COLS: &str = "id, feed_id, entry_id, url, title, author, published, content, \
                            is_read, starred, read_later, archived, fetched_at";
#[cfg(test)]
const SELECTION_COLS: &str = "id, article_id, selected_text, start_offset, end_offset, \
                              anchor_prefix, anchor_suffix, comment, is_favorite, created_at, updated_at";

pub struct Db {
    pub(crate) conn: Connection,
    pub(crate) path: Option<PathBuf>,
    // The maintenance module owns both the connection-lifetime lease and the
    // generation used by every fenced write.
    pub(crate) _maintenance_fence: Option<ConnectionFence>,
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

#[cfg(test)]
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

impl Db {
    pub fn open(path: &std::path::Path) -> Result<Self> {
        loop {
            let maintenance_fence = MaintenanceFence::open_connection(path)?;
            let conn = Connection::open(path)
                .with_context(|| format!("打开数据库失败: {}", path.display()))?;
            conn.pragma_update(None, "journal_mode", "WAL")?;
            conn.pragma_update(None, "foreign_keys", "ON")?;
            match schema_evolution::inspect(&conn)? {
                SchemaReadiness::Ready { .. } => {}
                SchemaReadiness::Uninitialized | SchemaReadiness::NeedsEvolution { .. } => {
                    drop(conn);
                    drop(maintenance_fence);
                    run_schema_migration(path, || {
                        let conn = Connection::open(path)?;
                        conn.pragma_update(None, "journal_mode", "WAL")?;
                        conn.pragma_update(None, "foreign_keys", "ON")?;
                        schema_evolution::evolve(&conn)?;
                        Ok(())
                    })?;
                    continue;
                }
                SchemaReadiness::Drifted { .. } | SchemaReadiness::NewerUnsupported { .. } => {
                    schema_evolution::require_ready(&conn)?;
                    unreachable!("non-ready schema was accepted")
                }
            }
            maintenance_fence.validate()?;
            return Ok(Self {
                conn,
                path: Some(path.to_path_buf()),
                _maintenance_fence: Some(maintenance_fence),
            });
        }
    }

    /// Open while the caller holds the exclusive maintenance writer lock.
    pub(crate) fn open_for_maintenance(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)
            .with_context(|| format!("open database for maintenance: {}", path.display()))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        schema_evolution::evolve(&conn)?;
        Ok(Self {
            conn,
            path: Some(path.to_path_buf()),
            _maintenance_fence: None,
        })
    }

    pub(crate) fn fenced_transaction(&self) -> Result<FencedTransaction<'_>> {
        match self._maintenance_fence.as_ref() {
            Some(fence) => fence.begin_write(&self.conn),
            None => Ok(FencedTransaction::uncoordinated(
                self.conn.unchecked_transaction()?,
            )),
        }
    }

    pub(crate) fn fenced_immediate_transaction(&mut self) -> Result<FencedTransaction<'_>> {
        match self._maintenance_fence.as_ref() {
            Some(fence) => fence.begin_immediate_write(&mut self.conn),
            None => Ok(FencedTransaction::uncoordinated(
                self.conn
                    .transaction_with_behavior(TransactionBehavior::Immediate)?,
            )),
        }
    }

    /// Begin a short transaction whose permit is the lifecycle's commit
    /// linearization witness. The caller must acquire this before publishing
    /// an irreversible state such as `Committing`.
    pub(crate) fn fenced_linearized_immediate_transaction(
        &mut self,
    ) -> Result<FencedTransaction<'_>> {
        match self._maintenance_fence.as_ref() {
            Some(fence) => fence.begin_linearized_immediate_write(&mut self.conn),
            None => Ok(FencedTransaction::uncoordinated(
                self.conn
                    .transaction_with_behavior(TransactionBehavior::Immediate)?,
            )),
        }
    }

    pub(crate) fn maintenance_drain_transaction(&self) -> Result<FencedTransaction<'_>> {
        match self._maintenance_fence.as_ref() {
            Some(fence) => fence.begin_drain_write(&self.conn),
            None => Ok(FencedTransaction::uncoordinated(
                self.conn.unchecked_transaction()?,
            )),
        }
    }

    pub(crate) fn fenced_transaction_for<'connection>(
        &'connection mut self,
        witness: &GenerationFence,
    ) -> Result<FencedTransaction<'connection>> {
        // Refresh commits must take the SQLite writer lock before reading any
        // mutable feed state. Otherwise a settings update can land between
        // the snapshot read and the result commit.
        witness.begin_immediate_write(&mut self.conn)
    }

    pub(crate) fn library_generation(&self) -> LibraryGeneration {
        self._maintenance_fence
            .as_ref()
            .map(|fence| LibraryGeneration::from_epoch(fence.generation()))
            .unwrap_or_else(LibraryGeneration::uncoordinated)
    }

    // ---- 源的增删查改 ----

    /// 添加源（幂等：已存在则返回既有 id）。
    #[cfg(test)]
    pub(crate) fn add_feed(&self, url: &str, now: i64) -> Result<i64> {
        Ok(self.add_feed_with_disposition(url, now)?.0)
    }

    #[cfg(test)]
    pub(crate) fn add_feed_with_disposition(&self, url: &str, now: i64) -> Result<(i64, bool)> {
        let tx = self.fenced_transaction()?;
        tx.execute(
            "INSERT OR IGNORE INTO feeds (url, next_fetch) VALUES (?1, ?2)",
            params![url, now],
        )?;
        let created = tx.changes() > 0;
        let id = tx.query_row("SELECT id FROM feeds WHERE url = ?1", params![url], |row| {
            row.get(0)
        })?;
        if created {
            library_projection_revision::record(&tx, ProjectionImpact::article())?;
        }
        tx.commit()?;
        Ok((id, created))
    }

    pub(crate) fn add_feed_with_disposition_matching<F>(
        &mut self,
        url: &str,
        now: i64,
        matches_existing: F,
    ) -> Result<(i64, bool)>
    where
        F: Fn(&str) -> bool,
    {
        let tx = self.fenced_immediate_transaction()?;
        let existing_id = tx
            .query_row("SELECT id FROM feeds WHERE url = ?1", params![url], |row| {
                row.get::<_, i64>(0)
            })
            .optional()?;
        let existing_id = if let Some(id) = existing_id {
            Some(id)
        } else {
            let mut statement = tx.prepare("SELECT id, url FROM feeds ORDER BY id")?;
            let mut rows = statement.query([])?;
            let mut found = None;
            while let Some(row) = rows.next()? {
                let id = row.get::<_, i64>(0)?;
                let candidate = row.get::<_, String>(1)?;
                if matches_existing(&candidate) {
                    found = Some(id);
                    break;
                }
            }
            found
        };
        let (id, created) = if let Some(id) = existing_id {
            (id, false)
        } else {
            tx.execute(
                "INSERT INTO feeds (url, next_fetch) VALUES (?1, ?2)",
                params![url, now],
            )?;
            (tx.last_insert_rowid(), true)
        };
        if created {
            library_projection_revision::record(&tx, ProjectionImpact::article())?;
        }
        tx.commit()?;
        Ok((id, created))
    }

    /// 按 id 或 url 删除，返回删除行数。
    pub(crate) fn remove_feed(&self, target: &str) -> Result<usize> {
        let tx = self.fenced_transaction()?;
        let feed_id = if let Ok(id) = target.parse::<i64>() {
            tx.query_row(
                "SELECT id FROM feeds WHERE id=?1 AND url<>?2",
                params![id, WEB_CLIPPINGS_FEED_URL],
                |row| row.get::<_, i64>(0),
            )
            .optional()?
        } else {
            tx.query_row(
                "SELECT id FROM feeds WHERE url=?1 AND url<>?2",
                params![target, WEB_CLIPPINGS_FEED_URL],
                |row| row.get::<_, i64>(0),
            )
            .optional()?
        };
        let Some(feed_id) = feed_id else {
            tx.commit()?;
            return Ok(0);
        };
        let affects_resource: bool = tx.query_row(
            "SELECT EXISTS(
               SELECT 1 FROM resources r JOIN articles a ON a.id=r.linked_article_id
               WHERE a.feed_id=?1
             )",
            [feed_id],
            |row| row.get(0),
        )?;
        let affects_excerpt: bool = tx.query_row(
            "SELECT EXISTS(
               SELECT 1 FROM article_selections s JOIN articles a ON a.id=s.article_id
               WHERE a.feed_id=?1
                 AND (s.is_favorite=1 OR (s.comment IS NOT NULL AND length(trim(s.comment))>0))
             )",
            [feed_id],
            |row| row.get(0),
        )?;
        let n = tx.execute("DELETE FROM feeds WHERE id=?1", [feed_id])?;
        if n > 0 {
            let mut impact = ProjectionImpact::article();
            if affects_resource {
                impact = impact.with(ProjectionFamily::Resource);
            }
            if affects_excerpt {
                impact = impact.with(ProjectionFamily::Excerpt);
            }
            library_projection_revision::record(&tx, impact)?;
        }
        tx.commit()?;
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
    #[cfg(test)]
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
    #[cfg(test)]
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

    #[cfg(test)]
    pub(crate) fn is_web_clipping(&self, article_id: i64) -> Result<bool> {
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
    #[cfg(test)]
    pub fn delete_web_clipping(&self, article_id: i64) -> Result<usize> {
        Ok(self.conn.execute(
            "DELETE FROM articles \
             WHERE id = ?1 AND feed_id = \
               (SELECT id FROM feeds WHERE url = ?2)",
            params![article_id, WEB_CLIPPINGS_FEED_URL],
        )?)
    }

    pub(crate) fn set_subscription_interval(&self, id: i64, secs: i64, now: i64) -> Result<usize> {
        let tx = self.fenced_transaction()?;
        let changed = tx.execute(
            "UPDATE feeds
             SET interval_secs = ?2,
                 next_fetch = COALESCE(last_fetch, ?3) + ?2
             WHERE id = ?1",
            params![id, secs, now],
        )?;
        tx.commit()?;
        Ok(changed)
    }

    pub(crate) fn request_subscription_refresh(&self, id: i64, now: i64) -> Result<usize> {
        let tx = self.fenced_transaction()?;
        let changed = tx.execute(
            "UPDATE feeds SET next_fetch = ?2 WHERE id = ?1",
            params![id, now],
        )?;
        tx.commit()?;
        Ok(changed)
    }

    pub(crate) fn set_disabled(&self, id: i64, disabled: bool, now: i64) -> Result<usize> {
        // 启用时清空失败状态并让它尽快重抓。
        let tx = self.fenced_transaction()?;
        let changed = if disabled {
            tx.execute("UPDATE feeds SET disabled = 1 WHERE id = ?1", params![id])?
        } else {
            tx.execute(
                "UPDATE feeds SET disabled = 0, fail_count = 0, last_error = NULL, next_fetch = ?2 WHERE id = ?1",
                params![id, now],
            )?
        };
        tx.commit()?;
        Ok(changed)
    }

    // ---- 抓取结果落库（ADR-8 去重 / ADR-11 退避禁用）----

    /// 抓取成功：插入新文章（INSERT OR IGNORE 去重），刷新源状态，返回新增条数。
    #[cfg(test)]
    pub fn record_success(
        &self,
        feed: &Feed,
        now: i64,
        cfg: &Config,
        title: Option<String>,
        articles: &[NewArticle],
    ) -> Result<usize> {
        let tx = self.fenced_transaction()?;
        let new = Self::record_success_on(&tx, feed, now, cfg, title, articles)?;
        tx.commit()?;
        Ok(new)
    }

    pub(crate) fn record_success_on(
        conn: &Connection,
        feed: &Feed,
        now: i64,
        cfg: &Config,
        title: Option<String>,
        articles: &[NewArticle],
    ) -> Result<usize> {
        let mut new = 0usize;
        let mut impact = ProjectionImpact::none();
        for a in articles {
            let inserted = conn.execute(
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
            if inserted > 0 {
                impact = impact.with(ProjectionFamily::Article);
                continue;
            }
            // Refresh mutable feed data on later fetches. Besides fixing corrected titles,
            // this lets improved parsers recover lazy-loaded/MediaRSS images for old rows.
            let current = conn.query_row(
                "SELECT id,url,title,author,published,content FROM articles
                 WHERE feed_id=?1 AND entry_id=?2",
                params![feed.id, a.entry_id],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, Option<String>>(3)?,
                        row.get::<_, Option<i64>>(4)?,
                        row.get::<_, Option<String>>(5)?,
                    ))
                },
            )?;
            let next_url = a.url.clone().or_else(|| current.1.clone());
            let next_title = a.title.clone().or_else(|| current.2.clone());
            let next_author = a.author.clone().or_else(|| current.3.clone());
            let next_published = a.published.or(current.4);
            let next_content = a
                .content
                .as_ref()
                .filter(|content| !content.is_empty())
                .cloned()
                .or_else(|| current.5.clone());
            let content_changed = next_content != current.5;
            let article_changed = next_url != current.1
                || next_title != current.2
                || next_author != current.3
                || next_published != current.4
                || content_changed;
            if article_changed {
                conn.execute(
                    "UPDATE articles SET url=?3,title=?4,author=?5,published=?6,content=?7
                     WHERE feed_id=?1 AND entry_id=?2",
                    params![
                        feed.id,
                        a.entry_id,
                        next_url,
                        next_title,
                        next_author,
                        next_published,
                        next_content,
                    ],
                )?;
                impact = impact.with(ProjectionFamily::Article);
                if content_changed {
                    let has_excerpt: bool = conn.query_row(
                        "SELECT EXISTS(
                           SELECT 1 FROM article_selections
                           WHERE article_id=?1
                             AND (is_favorite=1 OR (comment IS NOT NULL AND length(trim(comment))>0))
                         )",
                        [current.0],
                        |row| row.get(0),
                    )?;
                    if has_excerpt {
                        impact = impact.with(ProjectionFamily::Excerpt);
                    }
                }
            }
        }
        conn.execute(
            "UPDATE feeds SET title = COALESCE(title, ?2), last_fetch = ?3, \
             next_fetch = ?3 + COALESCE(interval_secs, ?4), fail_count = 0, last_error = NULL WHERE id = ?1",
            params![feed.id, title, now, cfg.default_interval_secs],
        )?;
        library_projection_revision::record(conn, impact)?;
        Ok(new)
    }

    /// 抓取失败：记录错误、指数退避、超阈值自动禁用。
    #[cfg(test)]
    pub fn record_failure(&self, feed: &Feed, now: i64, cfg: &Config, err: &str) -> Result<()> {
        let tx = self.fenced_transaction()?;
        Self::record_failure_on(&tx, feed, now, cfg, err)?;
        tx.commit()
    }

    pub(crate) fn record_failure_on(
        conn: &Connection,
        feed: &Feed,
        now: i64,
        cfg: &Config,
        err: &str,
    ) -> Result<()> {
        let current_fail_count: i64 = conn.query_row(
            "SELECT fail_count FROM feeds WHERE id = ?1",
            params![feed.id],
            |row| row.get(0),
        )?;
        let fc = current_fail_count + 1;
        let mult = 2i64.saturating_pow(fc.clamp(0, 16) as u32);
        let backoff = cfg
            .backoff_base_secs
            .saturating_mul(mult)
            .min(cfg.backoff_cap_secs);
        let disabled = (fc >= cfg.disable_after_failures) as i64;
        conn.execute(
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

    // ---- 文章选区：评论与收藏 ----

    /// 保存一段文章选区。评论和收藏可以同时存在，因此使用同一条记录承载。
    ///
    /// `start_offset`/`end_offset` 采用字符偏移而不是字节偏移；它们是可选的，
    /// 仅用于 UI 重新定位选区，正文变化后仍以 `selected_text` 为准。
    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn add_selection(
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

    #[cfg(test)]
    pub(crate) fn add_selection_with_anchor(
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
    #[cfg(test)]
    pub(crate) fn add_comment(
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

    /// 只收藏一段选区的便捷接口。
    #[cfg(test)]
    pub(crate) fn add_favorite_selection(
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

    #[cfg(test)]
    pub(crate) fn add_favorite_selection_with_anchor(
        &self,
        article_id: i64,
        selected_text: &str,
        anchor: &TextAnchor,
        now: i64,
    ) -> Result<i64> {
        self.add_selection_with_anchor(article_id, selected_text, anchor, None, true, now)
    }

    #[cfg(test)]
    pub(crate) fn get_selection(&self, selection_id: i64) -> Result<ArticleSelection> {
        let sql = format!("SELECT {SELECTION_COLS} FROM article_selections WHERE id = ?1");
        Ok(self
            .conn
            .query_row(&sql, params![selection_id], map_selection)?)
    }

    /// 返回文章中的选区，最新添加的排在前面。
    #[cfg(test)]
    pub(crate) fn selections_for_article(&self, article_id: i64) -> Result<Vec<ArticleSelection>> {
        let sql = format!(
            "SELECT {SELECTION_COLS} FROM article_selections \
             WHERE article_id = ?1 ORDER BY created_at DESC, id DESC"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(params![article_id], map_selection)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// 返回所有收藏的文字选区，供单独的“收藏/摘录”视图使用。
    #[cfg(test)]
    pub(crate) fn favorite_selections(&self) -> Result<Vec<ArticleSelection>> {
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
    #[cfg(test)]
    pub(crate) fn saved_selections(&self) -> Result<Vec<(ArticleSelection, i64, Option<String>)>> {
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

    #[cfg(test)]
    pub(crate) fn saved_selection_count(&self) -> Result<usize> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM article_selections \
             WHERE is_favorite = 1 \
                OR (comment IS NOT NULL AND length(trim(comment)) > 0)",
            [],
            |row| row.get(0),
        )?;
        Ok(count.max(0) as usize)
    }

    #[cfg(test)]
    pub(crate) fn set_selection_comment(
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

    #[cfg(test)]
    pub(crate) fn set_selection_favorite(
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

    #[cfg(test)]
    pub(crate) fn toggle_selection_favorite(&self, selection_id: i64, now: i64) -> Result<usize> {
        Ok(self.conn.execute(
            "UPDATE article_selections \
             SET is_favorite = 1 - is_favorite, updated_at = ?2 WHERE id = ?1",
            params![selection_id, now],
        )?)
    }

    #[cfg(test)]
    pub(crate) fn delete_selection(&self, selection_id: i64) -> Result<usize> {
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
        schema_evolution::evolve(&self.conn)?;
        Ok(())
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

    #[cfg(test)]
    fn compact(&self) -> Result<CompactionReport> {
        self.compact_uncoordinated()
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
    use crate::library_search::{
        EvidenceKind, FailureKind as SearchFailureKind, LibrarySearch, PrimaryIdentity, ResultType,
        SearchOrigin, SearchRequest, SearchScope,
    };
    use crate::model::NewArticle;

    fn mem() -> Db {
        let conn = Connection::open_in_memory().unwrap();
        crate::schema_evolution::evolve(&conn).unwrap();
        Db {
            conn,
            path: None,
            _maintenance_fence: None,
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
    fn refresh_commit_uses_current_interval_and_failure_count() {
        let db = mem();
        let cfg = Config::default();
        let feed_id = db.add_feed("http://x/current-state", 0).unwrap();
        let stale = db.get_feed(feed_id).unwrap();

        db.set_subscription_interval(feed_id, 3_600, 10).unwrap();
        db.record_success(&stale, 20, &cfg, None, &[]).unwrap();
        assert_eq!(db.get_feed(feed_id).unwrap().next_fetch, 3_620);

        db.record_failure(&stale, 30, &cfg, "first").unwrap();
        db.record_failure(&stale, 40, &cfg, "second").unwrap();
        let current = db.get_feed(feed_id).unwrap();
        assert_eq!(current.fail_count, 2);
        assert_eq!(current.last_error.as_deref(), Some("second"));
    }

    #[test]
    fn rss_revision_witness_ignores_noop_and_tracks_excerpt_resolution_input() {
        let db = mem();
        let cfg = Config::default();
        let feed_id = db.add_feed("http://x/revision-feed", 0).unwrap();
        let feed = db.get_feed(feed_id).unwrap();
        let after_feed = library_projection_revision::read(&db.conn).unwrap();
        db.record_success(&feed, 10, &cfg, None, &[art("revision")])
            .unwrap();
        let after_insert = library_projection_revision::read(&db.conn).unwrap();
        assert_eq!(after_insert.article, after_feed.article + 1);

        db.record_success(&feed, 20, &cfg, None, &[art("revision")])
            .unwrap();
        assert_eq!(
            library_projection_revision::read(&db.conn).unwrap(),
            after_insert
        );

        let article_id = feed_articles(&db, feed_id)[0].id;
        db.conn
            .execute(
                "INSERT INTO article_selections(
                   article_id,selected_text,is_favorite,created_at,updated_at
                 ) VALUES(?1,'body',1,1,1)",
                [article_id],
            )
            .unwrap();
        let mut changed = art("revision");
        changed.content = Some("changed body".into());
        db.record_success(&feed, 30, &cfg, None, &[changed])
            .unwrap();
        let after_body = library_projection_revision::read(&db.conn).unwrap();
        assert_eq!(after_body.article, after_insert.article + 1);
        assert_eq!(after_body.excerpt, after_insert.excerpt + 1);
    }

    #[test]
    fn feed_delete_records_all_actual_cascade_impacts_together() {
        let db = mem();
        let cfg = Config::default();
        let feed_id = db.add_feed("http://x/delete-feed", 0).unwrap();
        let feed = db.get_feed(feed_id).unwrap();
        db.record_success(&feed, 10, &cfg, None, &[art("delete")])
            .unwrap();
        let article_id = feed_articles(&db, feed_id)[0].id;
        db.conn
            .execute(
                "INSERT INTO article_selections(
                   article_id,selected_text,is_favorite,created_at,updated_at
                 ) VALUES(?1,'body',1,1,1)",
                [article_id],
            )
            .unwrap();
        db.conn
            .execute(
                "INSERT INTO resources(
                   url,canonical_url,linked_article_id,kind,status,source,created_at,updated_at
                 ) VALUES('https://resource.test','https://resource.test',?1,'page','active','import',1,1)",
                [article_id],
            )
            .unwrap();
        let resource_id = db.conn.last_insert_rowid();
        let before = library_projection_revision::read(&db.conn).unwrap();

        assert_eq!(db.remove_feed(&feed_id.to_string()).unwrap(), 1);
        let after = library_projection_revision::read(&db.conn).unwrap();
        assert_eq!(after.article, before.article + 1);
        assert_eq!(after.resource, before.resource + 1);
        assert_eq!(after.excerpt, before.excerpt + 1);
        assert_eq!(
            db.conn
                .query_row(
                    "SELECT linked_article_id FROM resources WHERE id=?1",
                    [resource_id],
                    |row| row.get::<_, Option<i64>>(0),
                )
                .unwrap(),
            None
        );
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

        let search = |query: &str, scope| {
            LibrarySearch::new(&db)
                .search(SearchRequest {
                    query: query.into(),
                    scope,
                    result_type: ResultType::All,
                    origin: SearchOrigin::Agent,
                    limit: 20,
                })
                .unwrap()
                .results
        };
        let article_hits = search("事件驱动", SearchScope::AllArticles);
        assert_eq!(article_hits.len(), 1);
        assert_eq!(
            article_hits[0].primary,
            PrimaryIdentity::Article(article_id)
        );

        let clip_hits = search("缓存策略", SearchScope::Curated);
        assert_eq!(clip_hits.len(), 1);
        assert_eq!(clip_hits[0].primary, PrimaryIdentity::Article(clipping_id));
        assert_eq!(clip_hits[0].evidence[0].kind, EvidenceKind::WebClipping);

        let excerpt_hits = search("领域模型", SearchScope::Curated);
        assert_eq!(excerpt_hits.len(), 1);
        assert_eq!(excerpt_hits[0].evidence[0].kind, EvidenceKind::Excerpt);

        let thought_hits = search("状态机", SearchScope::Curated);
        assert_eq!(thought_hits.len(), 1);
        assert_eq!(thought_hits[0].evidence[0].kind, EvidenceKind::Thought);
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

        let search = LibrarySearch::new(&db);
        let invalid = search
            .search(SearchRequest {
                query: "   ".into(),
                scope: SearchScope::AllArticles,
                result_type: ResultType::All,
                origin: SearchOrigin::Agent,
                limit: 20,
            })
            .unwrap_err();
        assert_eq!(invalid.kind, SearchFailureKind::Input);
        let active = search
            .search(SearchRequest {
                query: "SHARED".into(),
                scope: SearchScope::AllArticles,
                result_type: ResultType::All,
                origin: SearchOrigin::Agent,
                limit: 20,
            })
            .unwrap();
        assert_eq!(active.results.len(), 1);
        let archived = search
            .search(SearchRequest {
                query: "SHARED".into(),
                scope: SearchScope::Archive,
                result_type: ResultType::All,
                origin: SearchOrigin::Agent,
                limit: 20,
            })
            .unwrap();
        assert_eq!(
            archived.results[0].primary,
            PrimaryIdentity::Article(archived_id)
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

        let search = LibrarySearch::new(&db);
        for _ in 0..2 {
            assert_eq!(
                search
                    .search(SearchRequest {
                        query: "稳定锚点".into(),
                        scope: SearchScope::Curated,
                        result_type: ResultType::All,
                        origin: SearchOrigin::Human,
                        limit: 20,
                    })
                    .unwrap()
                    .results
                    .len(),
                1
            );
        }
        let history = search.history(10).unwrap();
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
