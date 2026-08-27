use std::collections::HashSet;

use anyhow::Context;
use rusqlite::{Connection, OptionalExtension, Row, params};
use sha2::{Digest, Sha256};

use super::{
    ArticleNavigation, ArticleOrigin, ChangeDisposition, ExcerptCapture, ExcerptIdentityKind,
    ExcerptResolution, ExcerptTarget, ExcerptThoughtChange, ExcerptThoughtCounts,
    ExcerptThoughtProjection, ExcerptView, ProjectionScope, ThoughtView,
};
use crate::article_document_presentation::article_selection_text;
use crate::db::WEB_CLIPPINGS_FEED_URL;
use crate::library_projection_revision::ProjectionStamp;
use crate::model::{TextAnchor, resolve_excerpt_anchor};

const ACTIVE_SELECTION: &str =
    "(s.is_favorite=1 OR (s.comment IS NOT NULL AND length(trim(s.comment))>0))";

#[derive(Debug, thiserror::Error)]
pub(super) enum StoreFailure {
    #[error("NOT_FOUND: {0}")]
    NotFound(String),
    #[error("INVALID_CAPTURE: {0}")]
    InvalidCapture(String),
    #[error("INVARIANT: {0}")]
    Invariant(String),
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct StoreChange {
    pub(super) disposition: ChangeDisposition,
    pub(super) excerpt_id: i64,
}

pub(super) fn capture_identity(capture: &ExcerptCapture) -> Option<Vec<u8>> {
    stable_identity(&capture.selected_text, &capture.anchor)
}

fn stable_identity(selected_text: &str, anchor: &TextAnchor) -> Option<Vec<u8>> {
    let offsets_are_valid = matches!(
        (anchor.start_offset, anchor.end_offset),
        (Some(start), Some(end)) if start >= 0 && end >= start
    );
    if !offsets_are_valid && anchor.prefix.is_empty() && anchor.suffix.is_empty() {
        return None;
    }

    let mut digest = Sha256::new();
    digest.update(b"rrss-excerpt-anchor-v1\0");
    hash_string(&mut digest, selected_text);
    hash_optional_i64(&mut digest, anchor.start_offset);
    hash_optional_i64(&mut digest, anchor.end_offset);
    hash_string(&mut digest, &anchor.prefix);
    hash_string(&mut digest, &anchor.suffix);
    Some(digest.finalize().to_vec())
}

fn hash_string(digest: &mut Sha256, value: &str) {
    digest.update((value.len() as u64).to_be_bytes());
    digest.update(value.as_bytes());
}

fn hash_optional_i64(digest: &mut Sha256, value: Option<i64>) {
    match value {
        Some(value) => {
            digest.update([1]);
            digest.update(value.to_be_bytes());
        }
        None => digest.update([0]),
    }
}

pub(super) fn project(
    conn: &Connection,
    scope: ProjectionScope,
    stamp: ProjectionStamp,
) -> Result<ExcerptThoughtProjection, StoreFailure> {
    if let ProjectionScope::Article(article_id) = scope
        && !article_exists(conn, article_id)?
    {
        return Err(StoreFailure::NotFound(format!(
            "ARTICLE_NOT_FOUND: {article_id}"
        )));
    }

    let mut sql = format!(
        "SELECT s.id,s.article_id,s.selected_text,s.start_offset,s.end_offset,
                s.anchor_prefix,s.anchor_suffix,s.comment,s.created_at,s.updated_at,
                s.lifecycle_identity,a.feed_id,a.title,a.url,a.content,
                CASE WHEN f.url=?1 THEN 1 ELSE 0 END
         FROM article_selections s
         JOIN articles a ON a.id=s.article_id
         JOIN feeds f ON f.id=a.feed_id
         WHERE {ACTIVE_SELECTION}"
    );
    if matches!(scope, ProjectionScope::Article(_)) {
        sql.push_str(" AND s.article_id=?2");
    }
    sql.push_str(" ORDER BY s.updated_at DESC,s.id DESC");

    let mut statement = conn.prepare(&sql)?;
    let excerpts = match scope {
        ProjectionScope::Article(article_id) => statement
            .query_map(params![WEB_CLIPPINGS_FEED_URL, article_id], map_excerpt)?
            .collect::<rusqlite::Result<Vec<_>>>()?,
        ProjectionScope::Library => statement
            .query_map([WEB_CLIPPINGS_FEED_URL], map_excerpt)?
            .collect::<rusqlite::Result<Vec<_>>>()?,
    };

    let (library_excerpts, library_thoughts) = count_scope(conn, None)?;
    let (scope_excerpts, scope_thoughts) = match scope {
        ProjectionScope::Article(article_id) => count_scope(conn, Some(article_id))?,
        ProjectionScope::Library => (library_excerpts, library_thoughts),
    };
    Ok(ExcerptThoughtProjection {
        stamp,
        scope,
        excerpts,
        counts: ExcerptThoughtCounts {
            library_excerpts,
            library_thoughts,
            scope_excerpts,
            scope_thoughts,
        },
    })
}

fn map_excerpt(row: &Row<'_>) -> rusqlite::Result<ExcerptView> {
    let id = row.get(0)?;
    let article_id = row.get(1)?;
    let selected_text: String = row.get(2)?;
    let anchor = TextAnchor {
        start_offset: row.get(3)?,
        end_offset: row.get(4)?,
        prefix: row.get(5)?,
        suffix: row.get(6)?,
    };
    let comment: Option<String> = row.get(7)?;
    let created_at = row.get(8)?;
    let updated_at = row.get(9)?;
    let lifecycle_identity: Option<Vec<u8>> = row.get(10)?;
    let feed_id = row.get(11)?;
    let title = row.get(12)?;
    let url = row.get(13)?;
    let content: Option<String> = row.get(14)?;
    let web_clipping: bool = row.get(15)?;
    let resolution = content
        .as_deref()
        .map(|document| article_selection_text(document, None))
        .as_deref()
        .and_then(|document| resolve_excerpt_anchor(document, &selected_text, &anchor))
        .map_or(ExcerptResolution::Unresolved, |range| {
            ExcerptResolution::Resolved {
                char_start: range.start,
                char_end: range.end,
            }
        });
    Ok(ExcerptView {
        id,
        article_id,
        selected_text,
        anchor,
        thought: comment.map(|content| ThoughtView {
            content,
            updated_at,
        }),
        resolution,
        identity_kind: if lifecycle_identity.is_some() {
            ExcerptIdentityKind::Managed
        } else {
            ExcerptIdentityKind::Legacy
        },
        source: ArticleNavigation {
            article_id,
            feed_id,
            title,
            url,
            origin: if web_clipping {
                ArticleOrigin::WebClipping
            } else {
                ArticleOrigin::Feed
            },
        },
        created_at,
        updated_at,
        lifecycle_identity,
    })
}

fn count_scope(conn: &Connection, article_id: Option<i64>) -> Result<(usize, usize), StoreFailure> {
    let mut sql = format!(
        "SELECT COUNT(*),COALESCE(SUM(CASE WHEN s.comment IS NOT NULL AND length(trim(s.comment))>0 THEN 1 ELSE 0 END),0)
         FROM article_selections s WHERE {ACTIVE_SELECTION}"
    );
    if article_id.is_some() {
        sql.push_str(" AND s.article_id=?1");
    }
    let (excerpts, thoughts): (i64, i64) = match article_id {
        Some(article_id) => {
            conn.query_row(&sql, [article_id], |row| Ok((row.get(0)?, row.get(1)?)))?
        }
        None => conn.query_row(&sql, [], |row| Ok((row.get(0)?, row.get(1)?)))?,
    };
    Ok((excerpts.max(0) as usize, thoughts.max(0) as usize))
}

pub(super) fn apply_change(
    conn: &Connection,
    change: &ExcerptThoughtChange,
    now: i64,
) -> Result<StoreChange, StoreFailure> {
    match change {
        ExcerptThoughtChange::EnsureExcerpt { capture } => {
            let (excerpt_id, created) = find_or_create_capture(conn, capture, now)?;
            Ok(StoreChange {
                disposition: if created {
                    ChangeDisposition::Created
                } else {
                    ChangeDisposition::Unchanged
                },
                excerpt_id,
            })
        }
        ExcerptThoughtChange::PutThought { target, content } => {
            let (excerpt_id, created) = match target {
                ExcerptTarget::Existing(excerpt_id) => {
                    ensure_active_excerpt(conn, *excerpt_id)?;
                    (*excerpt_id, false)
                }
                ExcerptTarget::Captured(capture) => find_or_create_capture(conn, capture, now)?,
            };
            let existing: Option<String> = conn.query_row(
                "SELECT comment FROM article_selections WHERE id=?1",
                [excerpt_id],
                |row| row.get(0),
            )?;
            let changed = existing.as_deref() != Some(content.as_str());
            if changed {
                conn.execute(
                    "UPDATE article_selections
                     SET comment=?2,is_favorite=1,updated_at=?3 WHERE id=?1",
                    params![excerpt_id, content, now],
                )?;
            }
            Ok(StoreChange {
                disposition: if created {
                    ChangeDisposition::Created
                } else if changed {
                    ChangeDisposition::Changed
                } else {
                    ChangeDisposition::Unchanged
                },
                excerpt_id,
            })
        }
        ExcerptThoughtChange::RemoveThought { excerpt_id } => {
            ensure_active_excerpt(conn, *excerpt_id)?;
            let has_thought: bool = conn.query_row(
                "SELECT comment IS NOT NULL AND length(trim(comment))>0
                 FROM article_selections WHERE id=?1",
                [excerpt_id],
                |row| row.get(0),
            )?;
            if has_thought {
                conn.execute(
                    "UPDATE article_selections SET comment=NULL,is_favorite=1,updated_at=?2 WHERE id=?1",
                    params![excerpt_id, now],
                )?;
            }
            Ok(StoreChange {
                disposition: if has_thought {
                    ChangeDisposition::Changed
                } else {
                    ChangeDisposition::Unchanged
                },
                excerpt_id: *excerpt_id,
            })
        }
        ExcerptThoughtChange::DeleteExcerpt { excerpt_id } => {
            let deleted = conn.execute(
                "DELETE FROM article_selections
                 WHERE id=?1 AND (is_favorite=1 OR (comment IS NOT NULL AND length(trim(comment))>0))",
                [excerpt_id],
            )?;
            if deleted == 0 {
                return Err(StoreFailure::NotFound(format!(
                    "EXCERPT_NOT_FOUND: {excerpt_id}"
                )));
            }
            Ok(StoreChange {
                disposition: ChangeDisposition::Deleted,
                excerpt_id: *excerpt_id,
            })
        }
    }
}

fn find_or_create_capture(
    conn: &Connection,
    capture: &ExcerptCapture,
    now: i64,
) -> Result<(i64, bool), StoreFailure> {
    if !article_exists(conn, capture.article_id)? {
        return Err(StoreFailure::NotFound(format!(
            "ARTICLE_NOT_FOUND: {}",
            capture.article_id
        )));
    }
    let identity = capture_identity(capture)
        .ok_or_else(|| StoreFailure::Invariant("CAPTURE_WITHOUT_STABLE_IDENTITY".into()))?;
    if let Some(excerpt_id) = conn
        .query_row(
            "SELECT id FROM article_selections
             WHERE article_id=?1 AND lifecycle_identity=?2",
            params![capture.article_id, identity],
            |row| row.get(0),
        )
        .optional()?
    {
        return Ok((excerpt_id, false));
    }

    let mut statement = conn.prepare(
        "SELECT id,selected_text,start_offset,end_offset,anchor_prefix,anchor_suffix
         FROM article_selections
         WHERE article_id=?1 AND lifecycle_identity IS NULL
           AND (is_favorite=1 OR (comment IS NOT NULL AND length(trim(comment))>0))
         ORDER BY updated_at DESC,id DESC",
    )?;
    let candidates = statement
        .query_map([capture.article_id], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                TextAnchor {
                    start_offset: row.get(2)?,
                    end_offset: row.get(3)?,
                    prefix: row.get(4)?,
                    suffix: row.get(5)?,
                },
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if let Some((excerpt_id, _, _)) = candidates.into_iter().find(|(_, text, anchor)| {
        stable_identity(text, anchor).as_deref() == Some(identity.as_slice())
    }) {
        conn.execute(
            "UPDATE article_selections SET lifecycle_identity=?2,is_favorite=1 WHERE id=?1",
            params![excerpt_id, identity],
        )?;
        return Ok((excerpt_id, false));
    }

    ensure_capture_matches_current_article(conn, capture)?;

    conn.execute(
        "INSERT INTO article_selections
         (article_id,selected_text,start_offset,end_offset,anchor_prefix,anchor_suffix,
          comment,is_favorite,created_at,updated_at,lifecycle_identity)
         VALUES(?1,?2,?3,?4,?5,?6,NULL,1,?7,?7,?8)",
        params![
            capture.article_id,
            capture.selected_text,
            capture.anchor.start_offset,
            capture.anchor.end_offset,
            capture.anchor.prefix,
            capture.anchor.suffix,
            now,
            identity
        ],
    )?;
    Ok((conn.last_insert_rowid(), true))
}

fn ensure_capture_matches_current_article(
    conn: &Connection,
    capture: &ExcerptCapture,
) -> Result<(), StoreFailure> {
    let content = conn
        .query_row(
            "SELECT content FROM articles WHERE id=?1",
            [capture.article_id],
            |row| row.get::<_, Option<String>>(0),
        )?
        .map(|html| article_selection_text(&html, None))
        .unwrap_or_default();
    let (Some(start), Some(end)) = (capture.anchor.start_offset, capture.anchor.end_offset) else {
        return Err(StoreFailure::InvalidCapture(
            "EXCERPT_ANCHOR_OFFSETS_MISSING".into(),
        ));
    };
    let (Ok(start), Ok(end)) = (usize::try_from(start), usize::try_from(end)) else {
        return Err(StoreFailure::InvalidCapture(
            "EXCERPT_ANCHOR_OFFSETS_NEGATIVE".into(),
        ));
    };
    let expected_len = end.saturating_sub(start);
    let selected = content
        .chars()
        .skip(start)
        .take(expected_len)
        .collect::<String>();
    if start >= end || selected.chars().count() != expected_len || selected != capture.selected_text
    {
        return Err(StoreFailure::InvalidCapture(
            "EXCERPT_ANCHOR_DOES_NOT_MATCH_CURRENT_ARTICLE".into(),
        ));
    }
    Ok(())
}

fn ensure_active_excerpt(conn: &Connection, excerpt_id: i64) -> Result<(), StoreFailure> {
    let exists: bool = conn.query_row(
        "SELECT EXISTS(
           SELECT 1 FROM article_selections
           WHERE id=?1 AND (is_favorite=1 OR (comment IS NOT NULL AND length(trim(comment))>0))
         )",
        [excerpt_id],
        |row| row.get(0),
    )?;
    if exists {
        Ok(())
    } else {
        Err(StoreFailure::NotFound(format!(
            "EXCERPT_NOT_FOUND: {excerpt_id}"
        )))
    }
}

fn article_exists(conn: &Connection, article_id: i64) -> Result<bool, StoreFailure> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM articles WHERE id=?1)",
        [article_id],
        |row| row.get(0),
    )?)
}

pub(crate) fn migrate_to_v7(conn: &Connection) -> anyhow::Result<()> {
    if !has_column(conn, "article_selections", "lifecycle_identity")? {
        conn.execute(
            "ALTER TABLE article_selections ADD COLUMN lifecycle_identity BLOB",
            [],
        )?;
    }

    // Thought-only historical rows become retained Excerpts without changing
    // their quote, Thought, Anchor, or timestamps.
    conn.execute(
        "UPDATE article_selections SET is_favorite=1
         WHERE is_favorite=0 AND comment IS NOT NULL AND length(trim(comment))>0",
        [],
    )?;
    conn.execute("UPDATE article_selections SET lifecycle_identity=NULL", [])?;

    let mut statement = conn.prepare(
        "SELECT id,article_id,selected_text,start_offset,end_offset,anchor_prefix,anchor_suffix
         FROM article_selections
         WHERE is_favorite=1 OR (comment IS NOT NULL AND length(trim(comment))>0)
         ORDER BY updated_at DESC,id DESC",
    )?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
                TextAnchor {
                    start_offset: row.get(3)?,
                    end_offset: row.get(4)?,
                    prefix: row.get(5)?,
                    suffix: row.get(6)?,
                },
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    drop(statement);

    let mut managed = HashSet::<(i64, Vec<u8>)>::new();
    for (excerpt_id, article_id, selected_text, anchor) in rows {
        let Some(identity) = stable_identity(&selected_text, &anchor) else {
            continue;
        };
        if managed.insert((article_id, identity.clone())) {
            conn.execute(
                "UPDATE article_selections SET lifecycle_identity=?2 WHERE id=?1",
                params![excerpt_id, identity],
            )?;
        }
    }
    conn.execute_batch(
        "CREATE UNIQUE INDEX IF NOT EXISTS idx_article_selections_managed_identity
           ON article_selections(article_id,lifecycle_identity)
           WHERE lifecycle_identity IS NOT NULL;",
    )?;

    if has_table(conn, "library_search_fts")? {
        conn.execute(
            "DELETE FROM library_search_fts WHERE source_kind IN ('excerpt','thought')",
            [],
        )?;
        conn.execute_batch(
            "INSERT INTO library_search_fts(source_kind,source_id,article_id,canonical_url,updated_at,identity_text,title_text,metadata_text,note_text,excerpt_text,body_text)
             SELECT 'excerpt',s.id,s.article_id,'',s.updated_at,'','','','',s.selected_text,''
             FROM article_selections s WHERE s.is_favorite=1;
             INSERT INTO library_search_fts(source_kind,source_id,article_id,canonical_url,updated_at,identity_text,title_text,metadata_text,note_text,excerpt_text,body_text)
             SELECT 'thought',s.id,s.article_id,'',s.updated_at,'','','',s.comment,'',''
             FROM article_selections s
             WHERE s.comment IS NOT NULL AND length(trim(s.comment))>0;",
        )?;
    }
    Ok(())
}

pub(crate) fn verify_schema_v7(conn: &Connection) -> anyhow::Result<()> {
    anyhow::ensure!(
        has_column(conn, "article_selections", "lifecycle_identity")?,
        "EXCERPT_SCHEMA_COLUMN_MISSING: lifecycle_identity"
    );
    let index_exists: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='index' AND name='idx_article_selections_managed_identity')",
        [],
        |row| row.get(0),
    )?;
    anyhow::ensure!(
        index_exists,
        "EXCERPT_SCHEMA_INDEX_MISSING: idx_article_selections_managed_identity"
    );
    let thought_only: i64 = conn.query_row(
        "SELECT COUNT(*) FROM article_selections
         WHERE is_favorite=0 AND comment IS NOT NULL AND length(trim(comment))>0",
        [],
        |row| row.get(0),
    )?;
    anyhow::ensure!(
        thought_only == 0,
        "EXCERPT_THOUGHT_ONLY_ROWS: {thought_only}"
    );
    let duplicate_identities: i64 = conn.query_row(
        "SELECT COUNT(*) FROM (
           SELECT article_id,lifecycle_identity
           FROM article_selections
           WHERE lifecycle_identity IS NOT NULL
           GROUP BY article_id,lifecycle_identity HAVING COUNT(*)>1
         )",
        [],
        |row| row.get(0),
    )?;
    anyhow::ensure!(
        duplicate_identities == 0,
        "EXCERPT_DUPLICATE_IDENTITIES: {duplicate_identities}"
    );
    Ok(())
}

fn has_column(conn: &Connection, table: &str, expected: &str) -> anyhow::Result<bool> {
    let mut statement = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let columns = statement
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(columns.iter().any(|column| column == expected))
}

fn has_table(conn: &Connection, table: &str) -> anyhow::Result<bool> {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
        [table],
        |row| row.get(0),
    )
    .with_context(|| format!("check table {table}"))
}
