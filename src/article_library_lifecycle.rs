//! Authoritative Article Library lifecycle and projection seam.
//!
//! GUI callers submit explicit target states and adopt the SQLite projection
//! returned by this module. Collection membership, counts, tags, fixed Web
//! Clipping bookmarks, transactions, and maintenance fencing stay here.

use std::collections::{BTreeSet, HashMap, HashSet};

use chrono::Utc;
use rusqlite::{Connection, Row, params, params_from_iter};

use crate::db::{Db, WEB_CLIPPINGS_FEED_URL};
use crate::model::Article;

const ARTICLE_COLUMNS: &str = "a.id, a.feed_id, a.entry_id, a.url, a.title, a.author, a.published, a.content, \
     a.is_read, a.starred, a.read_later, a.archived, a.fetched_at";
const QUERY_CHUNK: usize = 400;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProjectionScope {
    Feed(i64),
    ArticleBookmarks,
    ReadLater,
    Archive,
    Article(i64),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ArticleLifecycleChange {
    SetBookmark {
        article_id: i64,
        target: bool,
    },
    SetReadLater {
        article_id: i64,
        target: bool,
    },
    SetArchived {
        article_id: i64,
        target: bool,
    },
    SetRead {
        article_id: i64,
        target: bool,
    },
    ReplaceTags {
        article_id: i64,
        names: Vec<String>,
    },
    Batch {
        article_ids: Vec<i64>,
        action: ArticleBatchAction,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ArticleBatchAction {
    Archive,
    Bookmark,
    ReadLater,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ArticleLibraryCounts {
    pub(crate) bookmarks: usize,
    pub(crate) read_later: usize,
    pub(crate) archived: usize,
}

#[derive(Debug, Clone)]
pub(crate) struct ArticleLibraryProjection {
    pub(crate) scope: ProjectionScope,
    pub(crate) articles: Vec<Article>,
    pub(crate) tags: HashMap<i64, Vec<String>>,
    pub(crate) fixed_bookmark_ids: HashSet<i64>,
    pub(crate) counts: ArticleLibraryCounts,
    pub(crate) feed_unread: Vec<(i64, usize)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ChangeDisposition {
    Changed {
        matched_articles: usize,
        changed_articles: usize,
    },
    Unchanged {
        matched_articles: usize,
    },
}

impl ChangeDisposition {
    pub(crate) fn changed_articles(self) -> usize {
        match self {
            Self::Changed {
                changed_articles, ..
            } => changed_articles,
            Self::Unchanged { .. } => 0,
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ApplyOutcome {
    pub(crate) disposition: ChangeDisposition,
    pub(crate) projection: ArticleLibraryProjection,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FailureKind {
    Input,
    NotFound,
    Maintenance,
    Storage,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StorageOperation {
    Project,
    AcquireWriterPermit,
    BeginTransaction,
    ResolveArticles,
    ApplyChange,
    ReloadProjection,
    ValidateWriterPermit,
    Commit,
}

#[derive(Debug, Clone, thiserror::Error)]
#[error("{user_message}")]
pub(crate) struct LifecycleFailure {
    pub(crate) kind: FailureKind,
    pub(crate) user_message: String,
    pub(crate) technical_detail: String,
    pub(crate) operation: Option<StorageOperation>,
    pub(crate) missing_article_ids: Vec<i64>,
}

impl LifecycleFailure {
    pub(crate) fn input(user_message: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            kind: FailureKind::Input,
            user_message: user_message.into(),
            technical_detail: detail.into(),
            operation: None,
            missing_article_ids: Vec::new(),
        }
    }

    fn not_found(mut ids: Vec<i64>) -> Self {
        ids.sort_unstable();
        ids.dedup();
        Self {
            kind: FailureKind::NotFound,
            user_message: if ids.len() == 1 {
                "文章不存在或已经被删除".into()
            } else {
                "部分文章不存在，批量操作未执行".into()
            },
            technical_detail: format!("ARTICLE_NOT_FOUND: {ids:?}"),
            operation: None,
            missing_article_ids: ids,
        }
    }

    fn storage(operation: StorageOperation, error: impl std::fmt::Display) -> Self {
        let detail = error.to_string();
        let maintenance =
            detail.contains("MAINTENANCE_IN_PROGRESS") || detail.contains("STALE_LIBRARY_EPOCH");
        Self {
            kind: if maintenance {
                FailureKind::Maintenance
            } else {
                FailureKind::Storage
            },
            user_message: if maintenance {
                "资料维护期间不能修改文章".into()
            } else {
                "文章资料操作失败".into()
            },
            technical_detail: detail,
            operation: Some(operation),
            missing_article_ids: Vec::new(),
        }
    }
}

pub(crate) struct ArticleLibraryLifecycle<'db> {
    db: &'db Db,
}

impl<'db> ArticleLibraryLifecycle<'db> {
    pub(crate) fn new(db: &'db Db) -> Self {
        Self { db }
    }

    pub(crate) fn project(
        &self,
        scope: ProjectionScope,
    ) -> Result<ArticleLibraryProjection, LifecycleFailure> {
        let tx = self
            .db
            .conn
            .unchecked_transaction()
            .map_err(|error| LifecycleFailure::storage(StorageOperation::Project, error))?;
        let projection = build_projection(&tx, scope, StorageOperation::Project)?;
        tx.commit()
            .map_err(|error| LifecycleFailure::storage(StorageOperation::Project, error))?;
        Ok(projection)
    }

    pub(crate) fn apply(
        &self,
        change: ArticleLifecycleChange,
        refresh_scope: ProjectionScope,
    ) -> Result<ApplyOutcome, LifecycleFailure> {
        validate_change(&change)?;
        let permit = self.db.write_permit().map_err(|error| {
            LifecycleFailure::storage(StorageOperation::AcquireWriterPermit, error)
        })?;
        let tx = self.db.conn.unchecked_transaction().map_err(|error| {
            LifecycleFailure::storage(StorageOperation::BeginTransaction, error)
        })?;

        let disposition = apply_change(&tx, change)?;
        let projection = build_projection(&tx, refresh_scope, StorageOperation::ReloadProjection)?;
        if let Some(permit) = permit.as_ref() {
            permit.validate().map_err(|error| {
                LifecycleFailure::storage(StorageOperation::ValidateWriterPermit, error)
            })?;
        }
        tx.commit()
            .map_err(|error| LifecycleFailure::storage(StorageOperation::Commit, error))?;
        Ok(ApplyOutcome {
            disposition,
            projection,
        })
    }
}

fn validate_change(change: &ArticleLifecycleChange) -> Result<(), LifecycleFailure> {
    let ids = match change {
        ArticleLifecycleChange::SetBookmark { article_id, .. }
        | ArticleLifecycleChange::SetReadLater { article_id, .. }
        | ArticleLifecycleChange::SetArchived { article_id, .. }
        | ArticleLifecycleChange::SetRead { article_id, .. }
        | ArticleLifecycleChange::ReplaceTags { article_id, .. } => {
            std::slice::from_ref(article_id)
        }
        ArticleLifecycleChange::Batch { article_ids, .. } => {
            if article_ids.is_empty() {
                return Err(LifecycleFailure::input(
                    "请先选择文章",
                    "EMPTY_ARTICLE_BATCH",
                ));
            }
            article_ids.as_slice()
        }
    };
    if let Some(id) = ids.iter().find(|id| **id <= 0) {
        return Err(LifecycleFailure::input(
            "文章标识无效",
            format!("INVALID_ARTICLE_ID: {id}"),
        ));
    }
    Ok(())
}

#[derive(Debug, Clone, Copy)]
struct ResolvedArticle {
    id: i64,
    bookmarked: bool,
    read_later: bool,
    archived: bool,
    read: bool,
    fixed_bookmark: bool,
}

fn resolve_articles(
    conn: &Connection,
    ids: &[i64],
) -> Result<Vec<ResolvedArticle>, LifecycleFailure> {
    let unique = ids.iter().copied().collect::<BTreeSet<_>>();
    let ordered = unique.iter().copied().collect::<Vec<_>>();
    let mut resolved = Vec::with_capacity(ordered.len());
    for chunk in ordered.chunks(QUERY_CHUNK) {
        let placeholders = std::iter::repeat_n("?", chunk.len())
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            "SELECT a.id, a.starred, a.read_later, a.archived, a.is_read, f.url = ? \
             FROM articles a JOIN feeds f ON f.id = a.feed_id \
             WHERE a.id IN ({placeholders}) ORDER BY a.id"
        );
        let values = std::iter::once(rusqlite::types::Value::from(
            WEB_CLIPPINGS_FEED_URL.to_owned(),
        ))
        .chain(chunk.iter().copied().map(rusqlite::types::Value::from))
        .collect::<Vec<_>>();
        let mut statement = conn
            .prepare(&sql)
            .map_err(|error| LifecycleFailure::storage(StorageOperation::ResolveArticles, error))?;
        let rows = statement
            .query_map(params_from_iter(values), |row| {
                Ok(ResolvedArticle {
                    id: row.get(0)?,
                    bookmarked: row.get(1)?,
                    read_later: row.get(2)?,
                    archived: row.get(3)?,
                    read: row.get(4)?,
                    fixed_bookmark: row.get(5)?,
                })
            })
            .map_err(|error| LifecycleFailure::storage(StorageOperation::ResolveArticles, error))?;
        resolved.extend(
            rows.collect::<rusqlite::Result<Vec<_>>>()
                .map_err(|error| {
                    LifecycleFailure::storage(StorageOperation::ResolveArticles, error)
                })?,
        );
    }
    let found = resolved
        .iter()
        .map(|article| article.id)
        .collect::<HashSet<_>>();
    let missing = ordered
        .into_iter()
        .filter(|id| !found.contains(id))
        .collect::<Vec<_>>();
    if !missing.is_empty() {
        return Err(LifecycleFailure::not_found(missing));
    }
    Ok(resolved)
}

fn apply_change(
    conn: &Connection,
    change: ArticleLifecycleChange,
) -> Result<ChangeDisposition, LifecycleFailure> {
    match change {
        ArticleLifecycleChange::SetBookmark { article_id, target } => {
            let article = resolve_articles(conn, &[article_id])?[0];
            if article.fixed_bookmark {
                return if target {
                    Ok(ChangeDisposition::Unchanged {
                        matched_articles: 1,
                    })
                } else {
                    Err(LifecycleFailure::input(
                        "网页收藏是固定资料，请使用永久删除",
                        format!("FIXED_WEB_CLIPPING_BOOKMARK: {article_id}"),
                    ))
                };
            }
            set_bool(conn, article_id, "starred", article.bookmarked, target)
        }
        ArticleLifecycleChange::SetReadLater { article_id, target } => {
            let article = resolve_articles(conn, &[article_id])?[0];
            set_bool(conn, article_id, "read_later", article.read_later, target)
        }
        ArticleLifecycleChange::SetArchived { article_id, target } => {
            let article = resolve_articles(conn, &[article_id])?[0];
            set_bool(conn, article_id, "archived", article.archived, target)
        }
        ArticleLifecycleChange::SetRead { article_id, target } => {
            let article = resolve_articles(conn, &[article_id])?[0];
            set_bool(conn, article_id, "is_read", article.read, target)
        }
        ArticleLifecycleChange::ReplaceTags { article_id, names } => {
            resolve_articles(conn, &[article_id])?;
            replace_tags(conn, article_id, names)
        }
        ArticleLifecycleChange::Batch {
            article_ids,
            action,
        } => apply_batch(conn, article_ids, action),
    }
}

fn set_bool(
    conn: &Connection,
    article_id: i64,
    column: &'static str,
    current: bool,
    target: bool,
) -> Result<ChangeDisposition, LifecycleFailure> {
    if current == target {
        return Ok(ChangeDisposition::Unchanged {
            matched_articles: 1,
        });
    }
    let sql = format!("UPDATE articles SET {column} = ?2 WHERE id = ?1");
    conn.execute(&sql, params![article_id, target])
        .map_err(|error| LifecycleFailure::storage(StorageOperation::ApplyChange, error))?;
    Ok(ChangeDisposition::Changed {
        matched_articles: 1,
        changed_articles: 1,
    })
}

fn apply_batch(
    conn: &Connection,
    article_ids: Vec<i64>,
    action: ArticleBatchAction,
) -> Result<ChangeDisposition, LifecycleFailure> {
    let resolved = resolve_articles(conn, &article_ids)?;
    let ids = resolved
        .iter()
        .map(|article| article.id)
        .collect::<Vec<_>>();
    let mut changed = 0usize;
    for chunk in ids.chunks(QUERY_CHUNK) {
        let placeholders = std::iter::repeat_n("?", chunk.len())
            .collect::<Vec<_>>()
            .join(", ");
        let (assignment, predicate, exclude_clippings) = match action {
            ArticleBatchAction::Archive => ("archived = 1", "archived = 0", false),
            ArticleBatchAction::Bookmark => ("starred = 1", "starred = 0", true),
            ArticleBatchAction::ReadLater => ("read_later = 1", "read_later = 0", false),
        };
        let clipping_guard = if exclude_clippings {
            " AND feed_id NOT IN (SELECT id FROM feeds WHERE url = ?)"
        } else {
            ""
        };
        let sql = format!(
            "UPDATE articles SET {assignment} WHERE id IN ({placeholders}) \
             AND {predicate}{clipping_guard}"
        );
        let mut values = chunk
            .iter()
            .copied()
            .map(rusqlite::types::Value::from)
            .collect::<Vec<_>>();
        if exclude_clippings {
            values.push(rusqlite::types::Value::from(
                WEB_CLIPPINGS_FEED_URL.to_owned(),
            ));
        }
        changed += conn
            .execute(&sql, params_from_iter(values))
            .map_err(|error| LifecycleFailure::storage(StorageOperation::ApplyChange, error))?;
    }
    if changed == 0 {
        Ok(ChangeDisposition::Unchanged {
            matched_articles: resolved.len(),
        })
    } else {
        Ok(ChangeDisposition::Changed {
            matched_articles: resolved.len(),
            changed_articles: changed,
        })
    }
}

fn replace_tags(
    conn: &Connection,
    article_id: i64,
    names: Vec<String>,
) -> Result<ChangeDisposition, LifecycleFailure> {
    let normalized = normalize_tags(names);
    let current = tags_for_ids(conn, &[article_id], StorageOperation::ResolveArticles)?
        .remove(&article_id)
        .unwrap_or_default();
    if same_tag_set(&current, &normalized) {
        return Ok(ChangeDisposition::Unchanged {
            matched_articles: 1,
        });
    }

    conn.execute(
        "DELETE FROM article_tags WHERE article_id = ?1",
        params![article_id],
    )
    .map_err(|error| LifecycleFailure::storage(StorageOperation::ApplyChange, error))?;
    let now = Utc::now().timestamp();
    for name in normalized {
        conn.execute(
            "INSERT INTO tags(name, created_at) VALUES (?1, ?2) \
             ON CONFLICT(name) DO NOTHING",
            params![name, now],
        )
        .map_err(|error| LifecycleFailure::storage(StorageOperation::ApplyChange, error))?;
        conn.execute(
            "INSERT INTO article_tags(article_id, tag_id, created_at) \
             SELECT ?1, id, ?3 FROM tags WHERE name = ?2 COLLATE NOCASE",
            params![article_id, name, now],
        )
        .map_err(|error| LifecycleFailure::storage(StorageOperation::ApplyChange, error))?;
    }
    conn.execute(
        "DELETE FROM tags WHERE NOT EXISTS \
         (SELECT 1 FROM article_tags WHERE article_tags.tag_id = tags.id)",
        [],
    )
    .map_err(|error| LifecycleFailure::storage(StorageOperation::ApplyChange, error))?;
    Ok(ChangeDisposition::Changed {
        matched_articles: 1,
        changed_articles: 1,
    })
}

fn normalize_tags(names: Vec<String>) -> Vec<String> {
    let mut normalized = Vec::new();
    let mut seen = HashSet::new();
    for name in names {
        let name = name.trim();
        if name.is_empty() {
            continue;
        }
        let folded = name.to_lowercase();
        if seen.insert(folded) {
            normalized.push(name.to_owned());
        }
    }
    normalized.sort_by_key(|name| name.to_lowercase());
    normalized
}

fn same_tag_set(left: &[String], right: &[String]) -> bool {
    left.len() == right.len()
        && left
            .iter()
            .zip(right)
            .all(|(left, right)| left.eq_ignore_ascii_case(right))
}

fn build_projection(
    conn: &Connection,
    scope: ProjectionScope,
    operation: StorageOperation,
) -> Result<ArticleLibraryProjection, LifecycleFailure> {
    let (mut articles, fixed_bookmark_ids) = load_articles(conn, scope, operation)?;
    if matches!(scope, ProjectionScope::Article(_)) && articles.is_empty() {
        let ProjectionScope::Article(id) = scope else {
            unreachable!()
        };
        return Err(LifecycleFailure::not_found(vec![id]));
    }
    for article in &mut articles {
        if fixed_bookmark_ids.contains(&article.id) {
            article.starred = true;
        }
    }
    let article_ids = articles
        .iter()
        .map(|article| article.id)
        .collect::<Vec<_>>();
    let tags = tags_for_ids(conn, &article_ids, operation)?;
    let counts = load_counts(conn, operation)?;
    let feed_unread = load_feed_unread(conn, operation)?;
    Ok(ArticleLibraryProjection {
        scope,
        articles,
        tags,
        fixed_bookmark_ids,
        counts,
        feed_unread,
    })
}

fn map_article(row: &Row<'_>) -> rusqlite::Result<(Article, bool)> {
    Ok((
        Article {
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
        },
        row.get(13)?,
    ))
}

fn load_articles(
    conn: &Connection,
    scope: ProjectionScope,
    operation: StorageOperation,
) -> Result<(Vec<Article>, HashSet<i64>), LifecycleFailure> {
    let order = "ORDER BY COALESCE(a.published, a.fetched_at) DESC, a.id DESC";
    let (sql, values): (String, Vec<rusqlite::types::Value>) = match scope {
        ProjectionScope::Feed(feed_id) => (
            format!(
                "SELECT {ARTICLE_COLUMNS}, f.url = ?2 FROM articles a \
                 JOIN feeds f ON f.id = a.feed_id \
                 WHERE a.feed_id = ?1 AND a.archived = 0 {order}"
            ),
            vec![feed_id.into(), WEB_CLIPPINGS_FEED_URL.to_owned().into()],
        ),
        ProjectionScope::ArticleBookmarks => (
            format!(
                "SELECT {ARTICLE_COLUMNS}, f.url = ?1 FROM articles a \
                 JOIN feeds f ON f.id = a.feed_id \
                 WHERE a.archived = 0 AND (a.starred = 1 OR f.url = ?1) {order}"
            ),
            vec![WEB_CLIPPINGS_FEED_URL.to_owned().into()],
        ),
        ProjectionScope::ReadLater => (
            format!(
                "SELECT {ARTICLE_COLUMNS}, f.url = ?1 FROM articles a \
                 JOIN feeds f ON f.id = a.feed_id \
                 WHERE a.archived = 0 AND a.read_later = 1 {order}"
            ),
            vec![WEB_CLIPPINGS_FEED_URL.to_owned().into()],
        ),
        ProjectionScope::Archive => (
            format!(
                "SELECT {ARTICLE_COLUMNS}, f.url = ?1 FROM articles a \
                 JOIN feeds f ON f.id = a.feed_id WHERE a.archived = 1 {order}"
            ),
            vec![WEB_CLIPPINGS_FEED_URL.to_owned().into()],
        ),
        ProjectionScope::Article(article_id) => (
            format!(
                "SELECT {ARTICLE_COLUMNS}, f.url = ?2 FROM articles a \
                 JOIN feeds f ON f.id = a.feed_id WHERE a.id = ?1"
            ),
            vec![article_id.into(), WEB_CLIPPINGS_FEED_URL.to_owned().into()],
        ),
    };
    let mut statement = conn
        .prepare(&sql)
        .map_err(|error| LifecycleFailure::storage(operation, error))?;
    let rows = statement
        .query_map(params_from_iter(values), map_article)
        .map_err(|error| LifecycleFailure::storage(operation, error))?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|error| LifecycleFailure::storage(operation, error))?;
    let mut fixed = HashSet::new();
    let articles = rows
        .into_iter()
        .map(|(article, is_fixed)| {
            if is_fixed {
                fixed.insert(article.id);
            }
            article
        })
        .collect();
    Ok((articles, fixed))
}

fn tags_for_ids(
    conn: &Connection,
    article_ids: &[i64],
    operation: StorageOperation,
) -> Result<HashMap<i64, Vec<String>>, LifecycleFailure> {
    let mut tags = article_ids
        .iter()
        .copied()
        .map(|id| (id, Vec::new()))
        .collect::<HashMap<_, _>>();
    for chunk in article_ids.chunks(QUERY_CHUNK) {
        let placeholders = std::iter::repeat_n("?", chunk.len())
            .collect::<Vec<_>>()
            .join(", ");
        if placeholders.is_empty() {
            continue;
        }
        let sql = format!(
            "SELECT at.article_id, t.name FROM article_tags at \
             JOIN tags t ON t.id = at.tag_id \
             WHERE at.article_id IN ({placeholders}) \
             ORDER BY at.article_id, t.name COLLATE NOCASE"
        );
        let mut statement = conn
            .prepare(&sql)
            .map_err(|error| LifecycleFailure::storage(operation, error))?;
        let rows = statement
            .query_map(params_from_iter(chunk.iter()), |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(|error| LifecycleFailure::storage(operation, error))?;
        for row in rows {
            let (article_id, name) =
                row.map_err(|error| LifecycleFailure::storage(operation, error))?;
            tags.entry(article_id).or_default().push(name);
        }
    }
    Ok(tags)
}

fn load_counts(
    conn: &Connection,
    operation: StorageOperation,
) -> Result<ArticleLibraryCounts, LifecycleFailure> {
    conn.query_row(
        "SELECT \
           (SELECT COUNT(*) FROM articles a JOIN feeds f ON f.id = a.feed_id \
            WHERE a.archived = 0 AND (a.starred = 1 OR f.url = ?1)), \
           (SELECT COUNT(*) FROM articles WHERE archived = 0 AND read_later = 1), \
           (SELECT COUNT(*) FROM articles WHERE archived = 1)",
        params![WEB_CLIPPINGS_FEED_URL],
        |row| {
            let bookmarks = row.get::<_, i64>(0)?.max(0) as usize;
            let read_later = row.get::<_, i64>(1)?.max(0) as usize;
            let archived = row.get::<_, i64>(2)?.max(0) as usize;
            Ok(ArticleLibraryCounts {
                bookmarks,
                read_later,
                archived,
            })
        },
    )
    .map_err(|error| LifecycleFailure::storage(operation, error))
}

fn load_feed_unread(
    conn: &Connection,
    operation: StorageOperation,
) -> Result<Vec<(i64, usize)>, LifecycleFailure> {
    let mut statement = conn
        .prepare(
            "SELECT f.id, COUNT(a.id) FROM feeds f \
             LEFT JOIN articles a ON a.feed_id = f.id AND a.is_read = 0 AND a.archived = 0 \
             WHERE f.url <> ?1 GROUP BY f.id ORDER BY f.id",
        )
        .map_err(|error| LifecycleFailure::storage(operation, error))?;
    let rows = statement
        .query_map(params![WEB_CLIPPINGS_FEED_URL], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?.max(0) as usize))
        })
        .map_err(|error| LifecycleFailure::storage(operation, error))?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|error| LifecycleFailure::storage(operation, error))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::model::NewArticle;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEST_LIBRARY_ID: AtomicU64 = AtomicU64::new(1);

    struct TestLibrary {
        db: Option<Db>,
        root: PathBuf,
    }

    impl TestLibrary {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "shiyue-article-lifecycle-{}-{}-{}",
                std::process::id(),
                Utc::now().timestamp_nanos_opt().unwrap_or_default(),
                NEXT_TEST_LIBRARY_ID.fetch_add(1, Ordering::Relaxed),
            ));
            std::fs::create_dir_all(&root).unwrap();
            let db = Db::open(&root.join("library.db")).unwrap();
            Self { db: Some(db), root }
        }

        fn db(&self) -> &Db {
            self.db.as_ref().unwrap()
        }

        fn add_feed_articles(&self, entries: &[&str]) -> (i64, Vec<i64>) {
            let db = self.db();
            let feed_id = db.add_feed("https://example.com/feed.xml", 0).unwrap();
            let feed = db.get_feed(feed_id).unwrap();
            let articles = entries
                .iter()
                .map(|entry| NewArticle {
                    entry_id: (*entry).to_owned(),
                    url: Some(format!("https://example.com/{entry}")),
                    title: Some((*entry).to_owned()),
                    author: None,
                    published: None,
                    content: Some(format!("body {entry}")),
                })
                .collect::<Vec<_>>();
            db.record_success(&feed, 10, &Config::default(), None, &articles)
                .unwrap();
            let ids = db
                .conn
                .prepare("SELECT id FROM articles WHERE feed_id=?1 ORDER BY id")
                .unwrap()
                .query_map([feed_id], |row| row.get(0))
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap();
            (feed_id, ids)
        }
    }

    impl Drop for TestLibrary {
        fn drop(&mut self) {
            self.db.take();
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn states_are_independent_and_idempotent() {
        let library = TestLibrary::new();
        let (feed_id, ids) = library.add_feed_articles(&["one"]);
        let lifecycle = ArticleLibraryLifecycle::new(library.db());
        for change in [
            ArticleLifecycleChange::SetBookmark {
                article_id: ids[0],
                target: true,
            },
            ArticleLifecycleChange::SetReadLater {
                article_id: ids[0],
                target: true,
            },
            ArticleLifecycleChange::SetRead {
                article_id: ids[0],
                target: true,
            },
        ] {
            lifecycle
                .apply(change, ProjectionScope::Feed(feed_id))
                .unwrap();
        }
        let first = lifecycle
            .apply(
                ArticleLifecycleChange::SetRead {
                    article_id: ids[0],
                    target: true,
                },
                ProjectionScope::Feed(feed_id),
            )
            .unwrap();
        assert_eq!(
            first.disposition,
            ChangeDisposition::Unchanged {
                matched_articles: 1
            }
        );
        let article = &first.projection.articles[0];
        assert!(article.starred);
        assert!(article.read_later);
        assert!(article.is_read);
        assert!(!article.archived);
    }

    #[test]
    fn archive_hides_and_restore_recovers_retained_state_and_counts() {
        let library = TestLibrary::new();
        let (feed_id, ids) = library.add_feed_articles(&["one"]);
        let lifecycle = ArticleLibraryLifecycle::new(library.db());
        lifecycle
            .apply(
                ArticleLifecycleChange::SetBookmark {
                    article_id: ids[0],
                    target: true,
                },
                ProjectionScope::Feed(feed_id),
            )
            .unwrap();
        lifecycle
            .apply(
                ArticleLifecycleChange::SetReadLater {
                    article_id: ids[0],
                    target: true,
                },
                ProjectionScope::Feed(feed_id),
            )
            .unwrap();
        let archived = lifecycle
            .apply(
                ArticleLifecycleChange::SetArchived {
                    article_id: ids[0],
                    target: true,
                },
                ProjectionScope::Archive,
            )
            .unwrap();
        assert_eq!(archived.projection.articles.len(), 1);
        assert_eq!(archived.projection.counts.bookmarks, 0);
        assert_eq!(archived.projection.counts.read_later, 0);
        assert_eq!(archived.projection.counts.archived, 1);
        assert_eq!(archived.projection.feed_unread, vec![(feed_id, 0)]);

        let restored = lifecycle
            .apply(
                ArticleLifecycleChange::SetArchived {
                    article_id: ids[0],
                    target: false,
                },
                ProjectionScope::Feed(feed_id),
            )
            .unwrap();
        let article = &restored.projection.articles[0];
        assert!(article.starred);
        assert!(article.read_later);
        assert_eq!(restored.projection.counts.bookmarks, 1);
        assert_eq!(restored.projection.counts.read_later, 1);
        assert_eq!(restored.projection.feed_unread, vec![(feed_id, 1)]);
    }

    #[test]
    fn web_clipping_bookmark_is_fixed() {
        let library = TestLibrary::new();
        let clipping_id = library
            .db()
            .save_web_clipping(
                Some("https://example.com/page"),
                Some("saved"),
                "<main>saved</main>",
                1,
            )
            .unwrap();
        let lifecycle = ArticleLibraryLifecycle::new(library.db());
        let projection = lifecycle
            .project(ProjectionScope::ArticleBookmarks)
            .unwrap();
        assert!(projection.fixed_bookmark_ids.contains(&clipping_id));
        assert!(projection.articles[0].starred);

        let error = lifecycle
            .apply(
                ArticleLifecycleChange::SetBookmark {
                    article_id: clipping_id,
                    target: false,
                },
                ProjectionScope::ArticleBookmarks,
            )
            .unwrap_err();
        assert_eq!(error.kind, FailureKind::Input);
        assert!(
            lifecycle
                .project(ProjectionScope::ArticleBookmarks)
                .unwrap()
                .fixed_bookmark_ids
                .contains(&clipping_id)
        );
    }

    #[test]
    fn missing_batch_target_rolls_back_every_article() {
        let library = TestLibrary::new();
        let (feed_id, ids) = library.add_feed_articles(&["one", "two"]);
        let lifecycle = ArticleLibraryLifecycle::new(library.db());
        let error = lifecycle
            .apply(
                ArticleLifecycleChange::Batch {
                    article_ids: vec![ids[0], ids[0], 999_999, ids[1]],
                    action: ArticleBatchAction::ReadLater,
                },
                ProjectionScope::Feed(feed_id),
            )
            .unwrap_err();
        assert_eq!(error.kind, FailureKind::NotFound);
        assert_eq!(error.missing_article_ids, vec![999_999]);
        assert!(
            lifecycle
                .project(ProjectionScope::Feed(feed_id))
                .unwrap()
                .articles
                .iter()
                .all(|article| !article.read_later)
        );
    }

    #[test]
    fn successful_batch_deduplicates_targets_and_reports_changed_rows() {
        let library = TestLibrary::new();
        let (feed_id, ids) = library.add_feed_articles(&["one", "two"]);
        let lifecycle = ArticleLibraryLifecycle::new(library.db());
        let outcome = lifecycle
            .apply(
                ArticleLifecycleChange::Batch {
                    article_ids: vec![ids[0], ids[1], ids[0]],
                    action: ArticleBatchAction::Bookmark,
                },
                ProjectionScope::Feed(feed_id),
            )
            .unwrap();
        assert_eq!(
            outcome.disposition,
            ChangeDisposition::Changed {
                matched_articles: 2,
                changed_articles: 2,
            }
        );
        assert!(
            outcome
                .projection
                .articles
                .iter()
                .all(|article| article.starred)
        );

        let archived = lifecycle
            .apply(
                ArticleLifecycleChange::Batch {
                    article_ids: ids,
                    action: ArticleBatchAction::Archive,
                },
                ProjectionScope::Archive,
            )
            .unwrap();
        assert_eq!(archived.projection.articles.len(), 2);
        assert_eq!(archived.projection.counts.bookmarks, 0);
    }

    #[test]
    fn article_projection_distinguishes_missing_from_no_tags() {
        let library = TestLibrary::new();
        let (_, ids) = library.add_feed_articles(&["one"]);
        let lifecycle = ArticleLibraryLifecycle::new(library.db());
        let projection = lifecycle.project(ProjectionScope::Article(ids[0])).unwrap();
        assert!(projection.tags[&ids[0]].is_empty());

        let error = lifecycle
            .project(ProjectionScope::Article(999_999))
            .unwrap_err();
        assert_eq!(error.kind, FailureKind::NotFound);
        assert_eq!(error.missing_article_ids, vec![999_999]);
    }

    #[test]
    fn replacing_tags_normalizes_complete_set_and_cleans_orphans() {
        let library = TestLibrary::new();
        let (_, ids) = library.add_feed_articles(&["one"]);
        let lifecycle = ArticleLibraryLifecycle::new(library.db());
        let first = lifecycle
            .apply(
                ArticleLifecycleChange::ReplaceTags {
                    article_id: ids[0],
                    names: vec![" Rust ".into(), "rust".into(), "".into(), "架构".into()],
                },
                ProjectionScope::Article(ids[0]),
            )
            .unwrap();
        assert_eq!(
            first.projection.tags[&ids[0]],
            vec!["Rust".to_owned(), "架构".to_owned()]
        );
        assert_eq!(
            library.db().search_library("架构", 20).unwrap()[0].article_id,
            ids[0]
        );
        let unchanged = lifecycle
            .apply(
                ArticleLifecycleChange::ReplaceTags {
                    article_id: ids[0],
                    names: vec!["rust".into(), "架构".into()],
                },
                ProjectionScope::Article(ids[0]),
            )
            .unwrap();
        assert!(matches!(
            unchanged.disposition,
            ChangeDisposition::Unchanged { .. }
        ));
        let cleared = lifecycle
            .apply(
                ArticleLifecycleChange::ReplaceTags {
                    article_id: ids[0],
                    names: Vec::new(),
                },
                ProjectionScope::Article(ids[0]),
            )
            .unwrap();
        assert!(cleared.projection.tags[&ids[0]].is_empty());
        let orphan_count: i64 = library
            .db()
            .conn
            .query_row("SELECT COUNT(*) FROM tags", [], |row| row.get(0))
            .unwrap();
        assert_eq!(orphan_count, 0);
    }

    #[test]
    fn storage_failure_does_not_change_projection() {
        let library = TestLibrary::new();
        let (feed_id, ids) = library.add_feed_articles(&["one"]);
        library
            .db()
            .conn
            .execute_batch(
                "CREATE TRIGGER reject_article_update BEFORE UPDATE ON articles \
                 BEGIN SELECT RAISE(ABORT, 'forced storage failure'); END;",
            )
            .unwrap();
        let lifecycle = ArticleLibraryLifecycle::new(library.db());
        let before = lifecycle.project(ProjectionScope::Feed(feed_id)).unwrap();
        let error = lifecycle
            .apply(
                ArticleLifecycleChange::SetReadLater {
                    article_id: ids[0],
                    target: true,
                },
                ProjectionScope::Feed(feed_id),
            )
            .unwrap_err();
        assert_eq!(error.kind, FailureKind::Storage);
        let after = lifecycle.project(ProjectionScope::Feed(feed_id)).unwrap();
        assert_eq!(before.articles[0].read_later, after.articles[0].read_later);
        assert_eq!(before.counts, after.counts);
    }

    #[test]
    fn maintenance_markers_are_typed() {
        let failure = LifecycleFailure::storage(
            StorageOperation::AcquireWriterPermit,
            "MAINTENANCE_IN_PROGRESS",
        );
        assert_eq!(failure.kind, FailureKind::Maintenance);
    }
}
