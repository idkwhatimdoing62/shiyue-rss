//! SQLite 访问层（ADR-3）。daemon 与 TUI 两进程共享同一库，WAL 模式扛并发。

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, Row, params};

use crate::config::Config;
use crate::model::{Article, ArticleSelection, Feed, NewArticle};

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
  comment       TEXT,
  is_favorite   INTEGER NOT NULL DEFAULT 0 CHECK (is_favorite IN (0, 1)),
  created_at    INTEGER NOT NULL,
  updated_at    INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_article_selections_article
  ON article_selections(article_id, created_at DESC, id DESC);
CREATE INDEX IF NOT EXISTS idx_article_selections_favorite
  ON article_selections(is_favorite, created_at DESC, id DESC);
"#;

const FEED_COLS: &str =
    "id, url, title, interval_secs, last_fetch, next_fetch, last_error, fail_count, disabled";
const ARTICLE_COLS: &str = "id, feed_id, entry_id, url, title, author, published, content, \
                            is_read, starred, archived, fetched_at";
const SELECTION_COLS: &str = "id, article_id, selected_text, start_offset, end_offset, \
                              comment, is_favorite, created_at, updated_at";

pub struct Db {
    conn: Connection,
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
        archived: row.get(10)?,
        fetched_at: row.get(11)?,
    })
}

fn migrate(conn: &Connection) -> Result<()> {
    let has_archived = {
        let mut stmt = conn.prepare("PRAGMA table_info(articles)")?;
        let columns = stmt.query_map([], |row| row.get::<_, String>(1))?;
        columns
            .collect::<rusqlite::Result<Vec<_>>>()?
            .iter()
            .any(|name| name == "archived")
    };
    if !has_archived {
        conn.execute(
            "ALTER TABLE articles ADD COLUMN archived INTEGER NOT NULL DEFAULT 0",
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
        comment: row.get(5)?,
        is_favorite: row.get(6)?,
        created_at: row.get(7)?,
        updated_at: row.get(8)?,
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
        Ok(Self { conn })
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
            self.conn
                .execute("DELETE FROM feeds WHERE id = ?1", params![id])?
        } else {
            self.conn
                .execute("DELETE FROM feeds WHERE url = ?1", params![target])?
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
            "WHERE disabled = 0 AND next_fetch <= ?1 ORDER BY id",
            params![now],
        )
    }

    /// 所有未禁用的源（update 用）。
    pub fn enabled_feeds(&self) -> Result<Vec<Feed>> {
        self.query_feeds("WHERE disabled = 0 ORDER BY id", &[])
    }

    /// 最近一个到期时间（daemon 决定 sleep 多久）。
    pub fn earliest_next_fetch(&self) -> Result<Option<i64>> {
        let v: Option<i64> = self.conn.query_row(
            "SELECT MIN(next_fetch) FROM feeds WHERE disabled = 0",
            [],
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
             FROM feeds ORDER BY id"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map([], |row| Ok((map_feed(row)?, row.get::<_, i64>(9)?)))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
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
        self.conn.execute(
            "UPDATE articles SET starred = 1 - starred WHERE id = ?1",
            params![article_id],
        )?;
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
        let selected_text = selected_text.trim();
        if selected_text.is_empty() {
            bail!("选中的文字不能为空");
        }
        if let (Some(start), Some(end)) = (start_offset, end_offset) {
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
             (article_id, selected_text, start_offset, end_offset, comment, is_favorite, created_at, updated_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7)",
            params![
                article_id,
                selected_text,
                start_offset,
                end_offset,
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
                    s.comment, s.is_favorite, s.created_at, s.updated_at, \
                    a.feed_id, a.title \
             FROM article_selections s \
             JOIN articles a ON a.id = s.article_id \
             WHERE s.is_favorite = 1 \
                OR (s.comment IS NOT NULL AND length(trim(s.comment)) > 0) \
             ORDER BY s.updated_at DESC, s.id DESC",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((map_selection(row)?, row.get(9)?, row.get(10)?))
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::NewArticle;

    fn mem() -> Db {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        Db { conn }
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
}
